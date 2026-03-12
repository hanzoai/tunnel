#!/usr/bin/env python3
"""
Session Migration CLI — orchestrates checkpoint/restore between agents.

Usage:
    python3 migrate.py --from <source_node_id> --to <target_node_id>
    python3 migrate.py --list                    # list all agents
    python3 migrate.py --checkpoints [node_id]   # list checkpoints

Connects as operator to the bot gateway and invokes session.checkpoint
on the source, then session.restore on the target.
"""

import argparse
import asyncio
import json
import os
import platform
import subprocess
import sys
import time
import uuid

try:
    import websockets
except ImportError:
    subprocess.check_call([sys.executable, "-m", "pip", "install", "websockets", "-q"])
    import websockets

OPERATOR_URL = os.environ.get("OPERATOR_URL", "wss://app.hanzo.bot/")
TOKEN = os.environ.get("BOT_GATEWAY_TOKEN", "")


def make_req(method, params=None):
    return {
        "type": "req",
        "id": str(uuid.uuid4()),
        "method": method,
        "params": params or {},
    }


async def read_response(ws, req_id, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        remaining = max(1, deadline - time.time())
        raw = await asyncio.wait_for(ws.recv(), timeout=remaining)
        msg = json.loads(raw)
        if msg.get("type") == "res" and msg.get("id") == req_id:
            return msg
    raise TimeoutError(f"no response for {req_id} within {timeout}s")


async def operator_connect(ws):
    """Complete the operator handshake."""
    # Read connect.challenge
    raw = await asyncio.wait_for(ws.recv(), timeout=10)
    challenge = json.loads(raw)
    if challenge.get("type") == "event" and challenge.get("event") == "connect.challenge":
        pass  # expected

    req = make_req("connect", {
        "minProtocol": 3,
        "maxProtocol": 3,
        "client": {
            "id": "migrate-cli",
            "displayName": "Session Migration",
            "version": "1.0.0",
            "platform": f"{platform.system().lower()}-{platform.machine()}",
            "mode": "ui",
        },
        "auth": {"token": TOKEN},
        "role": "operator",
        "scopes": ["operator.read", "operator.write", "node.read", "node.write"],
    })
    await ws.send(json.dumps(req))
    resp = await read_response(ws, req["id"])
    if not resp.get("ok"):
        err = resp.get("error", {})
        raise Exception(f"connect failed: {err.get('code','?')}: {err.get('message','?')}")
    return resp


async def node_invoke(ws, node_id, command, params=None, timeout=60):
    """Invoke a command on a specific node via the gateway."""
    req = make_req("node.invoke", {
        "nodeId": node_id,
        "command": command,
        "params": params or {},
        "idempotencyKey": str(uuid.uuid4()),
    })
    await ws.send(json.dumps(req))
    resp = await read_response(ws, req["id"], timeout=timeout)
    if not resp.get("ok"):
        err = resp.get("error", {})
        raise Exception(f"invoke {command} on {node_id} failed: {err}")
    # Parse nested payload
    payload = resp.get("payload", {})
    if isinstance(payload.get("payloadJSON"), str):
        return json.loads(payload["payloadJSON"])
    if isinstance(payload.get("payload"), dict):
        return payload["payload"]
    if isinstance(payload.get("payload"), str):
        return json.loads(payload["payload"])
    return payload


async def cmd_list():
    """List all connected agents."""
    headers = {
        "Origin": "https://app.hanzo.bot",
        "Authorization": f"Bearer {TOKEN}",
    }
    async with websockets.connect(OPERATOR_URL, additional_headers=headers) as ws:
        await operator_connect(ws)
        req = make_req("node.list")
        await ws.send(json.dumps(req))
        resp = await read_response(ws, req["id"])
        nodes = resp.get("payload", {}).get("nodes", [])
        if not nodes:
            print("No agents connected.")
            return
        print(f"{'Node ID':<40} {'Platform':<20} {'Commands'}")
        print("-" * 90)
        for n in nodes:
            nid = n.get("nodeId", "?")
            plat = n.get("platform", "?")
            cmds = ", ".join(n.get("commands", [])[:3])
            print(f"{nid:<40} {plat:<20} {cmds}")


async def cmd_checkpoints(instance_id=""):
    """List checkpoints for an agent."""
    headers = {
        "Origin": "https://app.hanzo.bot",
        "Authorization": f"Bearer {TOKEN}",
    }
    async with websockets.connect(OPERATOR_URL, additional_headers=headers) as ws:
        await operator_connect(ws)
        # Pick any node to list checkpoints (they all see the same S3)
        req = make_req("node.list")
        await ws.send(json.dumps(req))
        resp = await read_response(ws, req["id"])
        nodes = resp.get("payload", {}).get("nodes", [])
        if not nodes:
            print("No agents connected to query checkpoints.")
            return
        node_id = nodes[0]["nodeId"]
        result = await node_invoke(ws, node_id, "session.list", {"instance_id": instance_id})
        checkpoints = result.get("checkpoints", [])
        if not checkpoints:
            print("No checkpoints found.")
            return
        print(f"{'Key':<60} {'Size':>8} {'Modified'}")
        print("-" * 90)
        for cp in checkpoints:
            print(f"{cp['key']:<60} {cp['size']:>8} {cp['last_modified'][:19]}")


async def cmd_migrate(source, target):
    """Migrate session from source to target."""
    headers = {
        "Origin": "https://app.hanzo.bot",
        "Authorization": f"Bearer {TOKEN}",
    }
    async with websockets.connect(OPERATOR_URL, additional_headers=headers) as ws:
        await operator_connect(ws)

        # Step 1: Checkpoint source
        print(f"[1/3] Checkpointing {source}...")
        cp_result = await node_invoke(ws, source, "session.checkpoint", timeout=30)
        checkpoint_url = cp_result.get("checkpoint_url") or cp_result.get("key", "")
        if not checkpoint_url:
            print(f"  ERROR: no checkpoint_url in result: {cp_result}")
            return
        stats = cp_result.get("session_stats", {})
        print(f"  Checkpoint: {checkpoint_url}")
        print(f"  Stats: {stats.get('conversation_count', 0)} messages, "
              f"{stats.get('command_count', 0)} commands, "
              f"{stats.get('scrollback_count', 0)} scrollback lines")

        # Step 2: Restore on target
        print(f"\n[2/3] Restoring on {target}...")
        restore_result = await node_invoke(ws, target, "session.restore",
                                           {"checkpoint_url": checkpoint_url}, timeout=60)
        print(f"  Restored from: {restore_result.get('restored_from', '?')}")
        print(f"  Conversation: {restore_result.get('conversation_count', 0)} messages")
        print(f"  Commands: {restore_result.get('command_count', 0)}")
        git_info = restore_result.get("git_restore")
        if git_info:
            print(f"  Git clone: {git_info.get('clone', 'n/a')}")
            print(f"  Git diff: {git_info.get('apply_diff', 'n/a')}")

        # Step 3: Verify
        print(f"\n[3/3] Verifying {target}...")
        status = await node_invoke(ws, target, "status.get")
        sess = status.get("session", {})
        print(f"  Status: {status.get('status', '?')}")
        print(f"  Session: {sess.get('conversation_count', 0)} msgs, "
              f"{sess.get('command_count', 0)} cmds")

        print(f"\nMigration complete: {source} -> {target}")


def main():
    parser = argparse.ArgumentParser(description="Session migration between Hanzo agents")
    parser.add_argument("--list", action="store_true", help="List all connected agents")
    parser.add_argument("--checkpoints", nargs="?", const="", default=None,
                        help="List checkpoints (optional: filter by instance_id)")
    parser.add_argument("--from", dest="source", help="Source agent node ID")
    parser.add_argument("--to", dest="target", help="Target agent node ID")
    args = parser.parse_args()

    if not TOKEN:
        print("ERROR: Set BOT_GATEWAY_TOKEN env var")
        sys.exit(1)

    if args.list:
        asyncio.run(cmd_list())
    elif args.checkpoints is not None:
        asyncio.run(cmd_checkpoints(args.checkpoints))
    elif args.source and args.target:
        asyncio.run(cmd_migrate(args.source, args.target))
    else:
        parser.print_help()
        sys.exit(1)


if __name__ == "__main__":
    main()
