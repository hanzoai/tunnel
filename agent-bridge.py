#!/usr/bin/env python3
"""
Hanzo Tunnel Agent Bridge — connects local machine to app.hanzo.bot

Registers this computer as a dev agent on the cloud control panel.
Handles commands from the web UI and executes them locally.

Session migration: checkpoint state to S3, restore on another agent.
"""

import asyncio
import concurrent.futures
import json
import os
import platform
import signal
import subprocess
import sys
import time
import uuid
from collections import deque
from datetime import datetime, timezone
from functools import lru_cache

try:
    import websockets
except ImportError:
    print("Installing websockets...")
    subprocess.check_call([sys.executable, "-m", "pip", "install", "websockets", "-q"])
    import websockets

try:
    import boto3
    from botocore.config import Config as BotoConfig
except ImportError:
    print("Installing boto3...")
    subprocess.check_call([sys.executable, "-m", "pip", "install", "boto3", "-q"])
    import boto3
    from botocore.config import Config as BotoConfig

TUNNEL_URL = os.environ.get("TUNNEL_URL", "wss://app.hanzo.bot/v1/tunnel")
INSTANCE_ID = os.environ.get("INSTANCE_ID", f"{platform.node().split('.')[0]}-{os.getpid()}")
APP_KIND = os.environ.get("APP_KIND", "dev")
CWD = os.environ.get("WORKSPACE_DIR", os.getcwd())

# S3 config — no defaults for credentials (fail fast if missing)
S3_ENDPOINT_URL = os.environ.get("S3_ENDPOINT_URL", "https://s3.hanzo.ai")
S3_ACCESS_KEY = os.environ.get("S3_ACCESS_KEY")
S3_SECRET_KEY = os.environ.get("S3_SECRET_KEY")
S3_BUCKET = os.environ.get("S3_BUCKET", "hanzo-sessions")

COMMANDS = [
    "chat.send", "exec.run", "status.get",
    "session.checkpoint", "session.restore", "session.list",
]

SCROLLBACK_MAX = 500
CHECKPOINT_VERSION = 1

# Thread pool for blocking S3 I/O
_s3_executor = concurrent.futures.ThreadPoolExecutor(max_workers=2)


def get_display_name():
    return f"{os.environ.get('USER', 'user')}@{platform.node().split('.')[0]}"


def get_platform_str():
    return f"{platform.system().lower()}-{platform.machine()}"


def _s3_available():
    return bool(S3_ACCESS_KEY and S3_SECRET_KEY)


@lru_cache(maxsize=1)
def _get_s3_client():
    if not _s3_available():
        raise RuntimeError("S3_ACCESS_KEY and S3_SECRET_KEY env vars required for session commands")
    return boto3.client(
        "s3",
        endpoint_url=S3_ENDPOINT_URL,
        aws_access_key_id=S3_ACCESS_KEY,
        aws_secret_access_key=S3_SECRET_KEY,
        config=BotoConfig(signature_version="s3v4"),
        region_name="us-east-1",
    )


def _ensure_bucket(s3):
    """Create bucket if it doesn't exist."""
    try:
        s3.head_bucket(Bucket=S3_BUCKET)
    except s3.exceptions.ClientError as e:
        if e.response["Error"]["Code"] in ("404", "NoSuchBucket"):
            s3.create_bucket(Bucket=S3_BUCKET)
        else:
            raise


class SessionState:
    """Tracks conversation, commands, and scrollback for checkpoint/restore."""

    def __init__(self):
        self.started_at = int(time.time())
        self.conversation = []
        self.commands = []
        self.scrollback = deque(maxlen=SCROLLBACK_MAX)

    def add_user_message(self, message, cmd_id=""):
        self.conversation.append({
            "role": "user",
            "content": message,
            "ts": int(time.time()),
            "cmd_id": cmd_id,
        })

    def add_assistant_message(self, content, cmd_id=""):
        self.conversation.append({
            "role": "assistant",
            "content": content,
            "ts": int(time.time()),
            "cmd_id": cmd_id,
        })

    def add_command(self, cmd, exit_code, stdout="", stderr=""):
        entry = {
            "cmd": cmd,
            "exit_code": exit_code,
            "stdout": stdout[-2048:],
            "ts": int(time.time()),
        }
        if stderr:
            entry["stderr"] = stderr[-1024:]
        self.commands.append(entry)
        self.scrollback.append(f"$ {cmd}")
        for line in stdout.split("\n")[-20:]:
            if line.strip():
                self.scrollback.append(line)

    def to_dict(self):
        return {
            "started_at": self.started_at,
            "cwd": CWD,
            "conversation": list(self.conversation),
            "commands": list(self.commands),
            "scrollback": list(self.scrollback),
        }

    def restore_from(self, data):
        self.started_at = data.get("started_at", int(time.time()))
        self.conversation = list(data.get("conversation", []))
        self.commands = list(data.get("commands", []))
        self.scrollback = deque(data.get("scrollback", []), maxlen=SCROLLBACK_MAX)


# Global session state
session = SessionState()


async def capture_workspace():
    """Capture git workspace state for checkpoint."""
    workspace = {}
    try:
        proc = await asyncio.create_subprocess_exec(
            "git", "rev-parse", "--is-inside-work-tree",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            cwd=CWD,
        )
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=5)
        if proc.returncode != 0:
            return workspace
    except Exception:
        return workspace

    async def git_cmd(*args):
        try:
            proc = await asyncio.create_subprocess_exec(
                "git", *args,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=CWD,
            )
            stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
            return stdout.decode("utf-8", errors="replace").strip()
        except Exception:
            return ""

    branch = await git_cmd("branch", "--show-current")
    remote_url = await git_cmd("remote", "get-url", "origin")
    log_raw = await git_cmd("log", "--oneline", "-20")
    diff = await git_cmd("diff", "HEAD")
    untracked = await git_cmd("ls-files", "--others", "--exclude-standard")

    workspace["git_branch"] = branch
    workspace["git_remote_url"] = remote_url
    workspace["git_log"] = log_raw.split("\n") if log_raw else []
    workspace["git_diff"] = diff[-8192:]  # cap at 8KB
    workspace["git_untracked"] = untracked.split("\n") if untracked else []

    return workspace


def _upload_checkpoint(checkpoint_data):
    """Upload checkpoint JSON to S3. Returns (checkpoint_id, key). Blocking."""
    checkpoint_id = checkpoint_data["checkpoint_id"]
    instance_id = checkpoint_data["source_instance_id"]
    key = f"{instance_id}/{checkpoint_id}.json"

    s3 = _get_s3_client()
    _ensure_bucket(s3)
    s3.put_object(
        Bucket=S3_BUCKET,
        Key=key,
        Body=json.dumps(checkpoint_data, indent=2).encode("utf-8"),
        ContentType="application/json",
    )
    return checkpoint_id, key


def _download_checkpoint(checkpoint_url):
    """Download checkpoint from S3. Blocking. Parses s3://bucket/key URIs."""
    if checkpoint_url.startswith("s3://"):
        without_scheme = checkpoint_url[5:]
        bucket, _, key = without_scheme.partition("/")
        if not key:
            raise ValueError(f"invalid s3 URI (no key): {checkpoint_url}")
    else:
        bucket = S3_BUCKET
        key = checkpoint_url

    s3 = _get_s3_client()
    resp = s3.get_object(Bucket=bucket, Key=key)
    body = resp["Body"].read()
    return json.loads(body)


def _list_checkpoints(prefix=""):
    """List checkpoints in S3 with pagination. Blocking."""
    s3 = _get_s3_client()
    _ensure_bucket(s3)
    paginator = s3.get_paginator("list_objects_v2")
    kwargs = {"Bucket": S3_BUCKET}
    if prefix:
        kwargs["Prefix"] = prefix

    items = []
    for page in paginator.paginate(**kwargs):
        for obj in page.get("Contents", []):
            key = obj["Key"]
            if key.endswith(".json"):
                items.append({
                    "key": key,
                    "size": obj["Size"],
                    "last_modified": obj["LastModified"].isoformat(),
                })
    return items


async def run_command(method, params, cmd_id=""):
    """Execute a command locally and return the result."""
    loop = asyncio.get_running_loop()

    if method == "status.get":
        return {
            "status": "running",
            "cwd": CWD,
            "user": os.environ.get("USER"),
            "hostname": platform.node(),
            "platform": get_platform_str(),
            "python": sys.version.split()[0],
            "pid": os.getpid(),
            "session": {
                "started_at": session.started_at,
                "conversation_count": len(session.conversation),
                "command_count": len(session.commands),
                "scrollback_count": len(session.scrollback),
            },
        }

    elif method == "exec.run":
        cmd = params.get("command", "")
        if not cmd:
            return {"error": "no command provided"}
        proc = None
        try:
            proc = await asyncio.create_subprocess_shell(
                cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=params.get("cwd", CWD),
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=30)
            stdout_str = stdout.decode("utf-8", errors="replace")[-4096:]
            stderr_str = stderr.decode("utf-8", errors="replace")[-4096:]
            session.add_command(cmd, proc.returncode, stdout_str, stderr_str)
            return {
                "exitCode": proc.returncode,
                "stdout": stdout_str,
                "stderr": stderr_str,
            }
        except asyncio.TimeoutError:
            if proc:
                proc.kill()
                await proc.wait()
            return {"error": "command timed out (30s)"}
        except Exception as e:
            return {"error": str(e)}

    elif method == "chat.send":
        message = params.get("message", "")
        if not message:
            return {"error": "no message provided"}
        session.add_user_message(message, cmd_id)
        proc = None
        try:
            env = {k: v for k, v in os.environ.items() if k != "CLAUDECODE"}
            env["OTEL_SDK_DISABLED"] = "true"
            env["OTEL_TRACES_EXPORTER"] = "none"
            env["OTEL_METRICS_EXPORTER"] = "none"
            env["OTEL_LOGS_EXPORTER"] = "none"
            env["DO_NOT_TRACK"] = "1"
            proc = await asyncio.create_subprocess_exec(
                "claude", "-p", message, "--output-format", "text",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=params.get("cwd", CWD),
                env=env,
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=120)
            raw = stdout.decode("utf-8", errors="replace")
            lines = raw.split("\n")
            clean_lines = []
            in_otel_block = False
            for line in lines:
                stripped = line.strip()
                if stripped.startswith("{") and ("resource:" in stripped or "descriptor:" in stripped or "instrumentationScope:" in stripped):
                    in_otel_block = True
                elif in_otel_block and stripped == "}":
                    in_otel_block = False
                    continue
                if not in_otel_block:
                    clean_lines.append(line)
            clean = "\n".join(clean_lines).strip()
            session.add_assistant_message(clean, cmd_id)
            return {
                "response": clean,
                "exitCode": proc.returncode,
            }
        except FileNotFoundError:
            return {"error": "claude CLI not found — install with: npm i -g @anthropic-ai/claude-code"}
        except asyncio.TimeoutError:
            if proc:
                proc.kill()
                await proc.wait()
            return {"error": "claude timed out (120s)"}
        except Exception as e:
            return {"error": str(e)}

    elif method == "session.checkpoint":
        if not _s3_available():
            return {"error": "S3_ACCESS_KEY and S3_SECRET_KEY env vars required"}
        try:
            workspace = await capture_workspace()
            checkpoint_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ") + "-" + uuid.uuid4().hex[:8]
            checkpoint_data = {
                "version": CHECKPOINT_VERSION,
                "checkpoint_id": checkpoint_id,
                "source_instance_id": INSTANCE_ID,
                "source_display_name": get_display_name(),
                "platform": get_platform_str(),
                "timestamp": datetime.now(timezone.utc).isoformat(),
                "session": session.to_dict(),
                "workspace": workspace,
                "config": {
                    "commands": COMMANDS,
                    "capabilities": ["chat", "exec", "tools", "terminal"],
                },
            }
            # Run blocking S3 upload off the event loop
            cid, key = await loop.run_in_executor(_s3_executor, _upload_checkpoint, checkpoint_data)
            checkpoint_url = f"s3://{S3_BUCKET}/{key}"
            print(f"  Checkpoint saved: {checkpoint_url}")
            return {
                "checkpoint_id": cid,
                "checkpoint_url": checkpoint_url,
                "key": key,
                "session_stats": {
                    "conversation_count": len(session.conversation),
                    "command_count": len(session.commands),
                    "scrollback_count": len(session.scrollback),
                },
            }
        except Exception as e:
            return {"error": f"checkpoint failed: {e}"}

    elif method == "session.restore":
        if not _s3_available():
            return {"error": "S3_ACCESS_KEY and S3_SECRET_KEY env vars required"}
        checkpoint_url = params.get("checkpoint_url", "")
        if not checkpoint_url:
            return {"error": "no checkpoint_url provided"}
        try:
            # Run blocking S3 download off the event loop
            data = await loop.run_in_executor(_s3_executor, _download_checkpoint, checkpoint_url)
            # Version check
            version = data.get("version", 0)
            if version != CHECKPOINT_VERSION:
                return {"error": f"unsupported checkpoint version: {version} (expected {CHECKPOINT_VERSION})"}
            # Restore session state
            session.restore_from(data.get("session", {}))
            workspace = data.get("workspace", {})
            restore_info = {
                "restored_from": data.get("source_instance_id", "?"),
                "source_platform": data.get("platform", "?"),
                "checkpoint_id": data.get("checkpoint_id", "?"),
                "timestamp": data.get("timestamp", "?"),
                "conversation_count": len(session.conversation),
                "command_count": len(session.commands),
                "scrollback_count": len(session.scrollback),
            }
            # Try git clone + apply if workspace has remote and we're in a clean dir
            if workspace.get("git_remote_url") and workspace.get("git_branch"):
                git_url = workspace["git_remote_url"]
                git_branch = workspace["git_branch"]
                git_diff = workspace.get("git_diff", "")
                restore_info["git_restore"] = await restore_git_workspace(git_url, git_branch, git_diff)

            print(f"  Session restored from {data.get('source_instance_id', '?')}")
            return restore_info
        except Exception as e:
            return {"error": f"restore failed: {e}"}

    elif method == "session.list":
        if not _s3_available():
            return {"error": "S3_ACCESS_KEY and S3_SECRET_KEY env vars required"}
        try:
            prefix = params.get("instance_id", "")
            # Run blocking S3 list off the event loop
            items = await loop.run_in_executor(_s3_executor, _list_checkpoints, prefix)
            return {
                "checkpoints": items,
                "count": len(items),
                "bucket": S3_BUCKET,
            }
        except Exception as e:
            return {"error": f"list failed: {e}"}

    else:
        return {"error": f"unknown command: {method}"}


async def restore_git_workspace(git_url, branch, diff):
    """Clone repo and apply diff if workspace dir is empty."""
    info = {"attempted": True}
    git_dir = os.path.join(CWD, ".git")
    if os.path.isdir(git_dir):
        info["clone"] = "skipped (existing .git)"
    else:
        try:
            proc = await asyncio.create_subprocess_exec(
                "git", "clone", "--depth=1", "-b", branch, git_url, ".",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=CWD,
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=60)
            info["clone"] = "ok" if proc.returncode == 0 else stderr.decode()[-200:]
        except asyncio.TimeoutError:
            if proc:
                proc.kill()
                await proc.wait()
            info["clone"] = "error: clone timed out (60s)"
        except Exception as e:
            info["clone"] = f"error: {e}"

    if diff and diff.strip():
        try:
            proc = await asyncio.create_subprocess_exec(
                "git", "apply", "--allow-empty", "-",
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                cwd=CWD,
            )
            stdout, stderr = await asyncio.wait_for(
                proc.communicate(input=diff.encode()), timeout=30
            )
            info["apply_diff"] = "ok" if proc.returncode == 0 else stderr.decode()[-200:]
        except asyncio.TimeoutError:
            if proc:
                proc.kill()
                await proc.wait()
            info["apply_diff"] = "error: apply timed out (30s)"
        except Exception as e:
            info["apply_diff"] = f"error: {e}"

    return info


async def main():
    display_name = get_display_name()
    print(f"Hanzo Tunnel Agent Bridge")
    print(f"  Instance:  {INSTANCE_ID}")
    print(f"  Name:      {display_name}")
    print(f"  Platform:  {get_platform_str()}")
    print(f"  AppKind:   {APP_KIND}")
    print(f"  CWD:       {CWD}")
    print(f"  Tunnel:    {TUNNEL_URL}")
    s3_status = f"{S3_ENDPOINT_URL}/{S3_BUCKET}" if _s3_available() else "not configured (session commands disabled)"
    print(f"  S3:        {s3_status}")
    print()

    reconnect_delay = 1
    while True:
        try:
            async with websockets.connect(TUNNEL_URL, ping_interval=20, ping_timeout=10) as ws:
                # Register
                await ws.send(json.dumps({
                    "type": "register",
                    "instance_id": INSTANCE_ID,
                    "app_kind": APP_KIND,
                    "display_name": display_name,
                    "capabilities": ["chat", "exec", "tools", "terminal"],
                    "commands": COMMANDS,
                    "version": "0.2.0",
                    "platform": get_platform_str(),
                    "cwd": CWD,
                }))

                raw = await asyncio.wait_for(ws.recv(), timeout=10)
                frame = json.loads(raw)
                if frame.get("type") != "registered":
                    print(f"Registration failed: {frame}")
                    return

                session_url = frame.get("session_url", "")
                print(f"  Connected! View at: {session_url}")
                print(f"  Waiting for commands...\n")
                reconnect_delay = 1

                # Main loop
                while True:
                    raw = await ws.recv()
                    frame = json.loads(raw)

                    if frame.get("type") == "ping":
                        await ws.send(json.dumps({"type": "pong"}))

                    elif frame.get("type") == "command":
                        cmd_id = frame.get("id", "")
                        method = frame.get("method", "")
                        params = frame.get("params", {})
                        print(f"  <- command: {method} (id={cmd_id[:8]})")

                        try:
                            result = await run_command(method, params, cmd_id)
                            await ws.send(json.dumps({
                                "type": "response",
                                "id": cmd_id,
                                "ok": "error" not in result,
                                "data": result,
                                "error": result.get("error"),
                            }))
                            print(f"  -> response: ok={'error' not in result}")
                        except Exception as e:
                            await ws.send(json.dumps({
                                "type": "response",
                                "id": cmd_id,
                                "ok": False,
                                "error": str(e),
                            }))
                            print(f"  -> error: {e}")

                    elif frame.get("type") == "pong":
                        pass  # heartbeat ack

                    else:
                        print(f"  ?? unknown frame: {frame.get('type')}")

        except (websockets.exceptions.ConnectionClosed, ConnectionError, OSError) as e:
            print(f"  Disconnected: {e}")
            print(f"  Reconnecting in {reconnect_delay}s...")
            await asyncio.sleep(reconnect_delay)
            reconnect_delay = min(reconnect_delay * 2, 30)

        except asyncio.CancelledError:
            print("\nShutting down...")
            break


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\nBye.")
