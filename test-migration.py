#!/usr/bin/env python3
"""
E2E Session Migration Test — verifies Mac <-> Cloud round-trip.

1. Launch 4 local agents (simulating 2 Mac + 2 Cloud)
2. Build state on agent-1 (exec.run + chat.send)
3. Checkpoint agent-1 → S3
4. Restore on agent-2 → verify state
5. Checkpoint agent-2 → restore on agent-1 (round trip)
6. Verify final state

Requires:
  - BOT_GATEWAY_TOKEN env var
  - S3 (MinIO) accessible for checkpoint storage
"""

import asyncio
import json
import os
import platform
import signal
import subprocess
import sys
import time
import uuid

try:
    import websockets
except ImportError:
    subprocess.check_call([sys.executable, "-m", "pip", "install", "websockets", "-q"])
    import websockets

N_AGENTS = 4
TUNNEL_URL = os.environ.get("TUNNEL_URL", "wss://app.hanzo.bot/v1/tunnel")
OPERATOR_URL = os.environ.get("OPERATOR_URL", "wss://app.hanzo.bot/")
TOKEN = os.environ.get("BOT_GATEWAY_TOKEN", "")

PASS = 0
FAIL = 0


def result(ok, label, detail=""):
    global PASS, FAIL
    if ok:
        PASS += 1
        print(f"  PASS  {label}" + (f" — {detail}" if detail else ""))
    else:
        FAIL += 1
        print(f"  FAIL  {label}" + (f" — {detail}" if detail else ""))


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
    raw = await asyncio.wait_for(ws.recv(), timeout=10)
    challenge = json.loads(raw)

    req = make_req("connect", {
        "minProtocol": 3,
        "maxProtocol": 3,
        "client": {
            "id": "bot-control-ui",
            "displayName": "Migration Test",
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
        raise Exception(f"connect failed: {resp.get('error')}")
    return resp


async def node_invoke(ws, node_id, command, params=None, timeout=60):
    req = make_req("node.invoke", {
        "nodeId": node_id,
        "command": command,
        "params": params or {},
        "idempotencyKey": str(uuid.uuid4()),
    })
    await ws.send(json.dumps(req))
    resp = await read_response(ws, req["id"], timeout=timeout)
    payload = resp.get("payload", {})
    if isinstance(payload.get("payloadJSON"), str):
        inner = json.loads(payload["payloadJSON"])
    elif isinstance(payload.get("payload"), dict):
        inner = payload["payload"]
    elif isinstance(payload.get("payload"), str):
        inner = json.loads(payload["payload"])
    else:
        inner = payload
    return resp.get("ok", False), inner


async def main():
    global PASS, FAIL

    if not TOKEN:
        print("ERROR: Set BOT_GATEWAY_TOKEN env var")
        sys.exit(1)

    print(f"=== Session Migration E2E Test ===")
    print(f"  Agents:   {N_AGENTS} (2 simulated Mac + 2 simulated Cloud)")
    print(f"  Tunnel:   {TUNNEL_URL}")
    print(f"  Operator: {OPERATOR_URL}")
    print()

    # 1. Launch 4 agent-bridge processes
    procs = []
    instance_ids = []
    bridge_script = os.path.join(os.path.dirname(os.path.abspath(__file__)), "agent-bridge.py")
    for i in range(N_AGENTS):
        kind = "dev" if i < 2 else "cloud"
        iid = f"migrate-test-{kind}-{i+1}-{os.getpid()}"
        instance_ids.append(iid)
        env = os.environ.copy()
        env["INSTANCE_ID"] = iid
        env["TUNNEL_URL"] = TUNNEL_URL
        env["APP_KIND"] = kind
        p = subprocess.Popen(
            [sys.executable, bridge_script],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        procs.append(p)

    print(f"  Launched {N_AGENTS} agents, waiting 8s for registration...")
    await asyncio.sleep(8)

    # Check all alive
    alive = 0
    for i, p in enumerate(procs):
        ok = p.poll() is None
        if ok:
            alive += 1
        result(ok, f"agent-{i+1} alive ({instance_ids[i][:20]}...)")
    if alive < 2:
        print("  Not enough agents alive, aborting.")
        for p in procs:
            p.kill()
        sys.exit(1)

    try:
        headers = {
            "Origin": "https://app.hanzo.bot",
            "Authorization": f"Bearer {TOKEN}",
        }
        async with websockets.connect(OPERATOR_URL, additional_headers=headers) as ws:
            await operator_connect(ws)

            # 2. Verify all agents in node.list
            req = make_req("node.list")
            await ws.send(json.dumps(req))
            resp = await read_response(ws, req["id"])
            nodes = resp.get("payload", {}).get("nodes", [])
            node_ids = [n["nodeId"] for n in nodes]
            found = sum(1 for iid in instance_ids if iid in node_ids)
            result(found == N_AGENTS, f"all {N_AGENTS} agents visible", f"{found}/{N_AGENTS}")

            a1 = instance_ids[0]  # "Mac" agent
            a2 = instance_ids[2]  # "Cloud" agent

            # 3. Build state on agent-1
            print(f"\n  --- Building state on {a1[:20]}... ---")
            ok, data = await node_invoke(ws, a1, "exec.run", {"command": "echo hello-from-mac"})
            result(ok, "exec.run on agent-1", data.get("stdout", "")[:40])

            ok, data = await node_invoke(ws, a1, "exec.run", {"command": "uname -a"})
            result(ok, "exec.run uname on agent-1")

            # Get status to see state counts
            ok, data = await node_invoke(ws, a1, "status.get")
            sess = data.get("session", {})
            result(sess.get("command_count", 0) >= 2, "agent-1 tracked commands",
                   f"count={sess.get('command_count')}")

            # 4. Checkpoint agent-1
            print(f"\n  --- Checkpointing {a1[:20]}... ---")
            ok, data = await node_invoke(ws, a1, "session.checkpoint")
            result(ok, "session.checkpoint on agent-1")
            checkpoint_url = data.get("checkpoint_url", data.get("key", ""))
            result(bool(checkpoint_url), "got checkpoint_url", checkpoint_url[:60])
            cp_stats = data.get("session_stats", {})
            print(f"    Stats: {cp_stats}")

            # 5. Restore on agent-2 (cloud)
            print(f"\n  --- Restoring on {a2[:20]}... ---")
            ok, data = await node_invoke(ws, a2, "session.restore",
                                         {"checkpoint_url": checkpoint_url})
            result(ok, "session.restore on agent-2")
            result(data.get("restored_from", "") == a1, "restored from correct source",
                   data.get("restored_from", "?"))
            result(data.get("command_count", 0) >= 2, "restored commands",
                   f"count={data.get('command_count')}")

            # 6. Verify agent-2 status has restored state
            ok, data = await node_invoke(ws, a2, "status.get")
            sess2 = data.get("session", {})
            result(sess2.get("command_count", 0) >= 2, "agent-2 has restored commands",
                   f"count={sess2.get('command_count')}")

            # 7. Reverse: checkpoint cloud, restore Mac
            print(f"\n  --- Reverse migration: {a2[:20]}... -> {a1[:20]}... ---")

            # Add a command on agent-2 first to prove round-trip adds state
            ok, data = await node_invoke(ws, a2, "exec.run", {"command": "echo hello-from-cloud"})
            result(ok, "exec.run on agent-2 (cloud)")

            ok, data = await node_invoke(ws, a2, "session.checkpoint")
            result(ok, "session.checkpoint on agent-2")
            checkpoint_url_2 = data.get("checkpoint_url", data.get("key", ""))
            result(bool(checkpoint_url_2), "got checkpoint_url (reverse)")

            ok, data = await node_invoke(ws, a1, "session.restore",
                                         {"checkpoint_url": checkpoint_url_2})
            result(ok, "session.restore on agent-1 (reverse)")

            # Agent-1 should now have agent-2's additional command
            ok, data = await node_invoke(ws, a1, "status.get")
            sess3 = data.get("session", {})
            result(sess3.get("command_count", 0) >= 3, "agent-1 has round-trip state",
                   f"count={sess3.get('command_count')}")

            # 8. List checkpoints
            print(f"\n  --- Listing checkpoints ---")
            ok, data = await node_invoke(ws, a1, "session.list")
            result(ok, "session.list")
            cp_count = data.get("count", 0)
            result(cp_count >= 2, "at least 2 checkpoints exist", f"count={cp_count}")

    except Exception as e:
        import traceback
        result(False, "test session", f"{e}\n{traceback.format_exc()}")

    # Cleanup
    print(f"\n  Stopping agents...")
    for p in procs:
        p.send_signal(signal.SIGTERM)
    for p in procs:
        try:
            p.wait(timeout=5)
        except subprocess.TimeoutExpired:
            p.kill()

    print(f"\n=== Results: {PASS} passed, {FAIL} failed ===")
    sys.exit(0 if FAIL == 0 else 1)


if __name__ == "__main__":
    asyncio.run(main())
