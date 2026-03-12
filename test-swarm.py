#!/usr/bin/env python3
"""
Swarm E2E test — launches N agent-bridge instances, verifies all visible
via node.list, invokes status.get on each, then cleans up.

Uses the bot gateway protocol v3:
  - Request frame: {"type": "req", "id": "...", "method": "...", "params": {...}}
  - Response frame: {"type": "res", "id": "...", "ok": true/false, "payload": {...}}
  - Connect handshake: method="connect", params=ConnectParams
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

N_AGENTS = int(os.environ.get("N_AGENTS", "4"))
TUNNEL_URL = os.environ.get("TUNNEL_URL", "wss://app.hanzo.bot/v1/tunnel")
# Operator WS for sending node.list / node.invoke commands
# Operator WS: root path, NOT /__bot__/ws (that's the canvas live-reload WS)
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
    """Build a protocol v3 request frame."""
    return {
        "type": "req",
        "id": str(uuid.uuid4()),
        "method": method,
        "params": params or {},
    }


async def read_response(ws, req_id, timeout=15):
    """Read WS messages until we find the 'res' frame matching req_id."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        remaining = max(1, deadline - time.time())
        raw = await asyncio.wait_for(ws.recv(), timeout=remaining)
        msg = json.loads(raw)
        # Protocol v3: responses have type="res" and matching id
        if msg.get("type") == "res" and msg.get("id") == req_id:
            return msg
        # Also accept events (connect.challenge, tick, etc.) — skip them
    raise TimeoutError(f"no response for {req_id} within {timeout}s")


async def main():
    global PASS, FAIL

    if not TOKEN:
        print("ERROR: Set BOT_GATEWAY_TOKEN env var")
        sys.exit(1)

    print(f"=== Swarm E2E Test ===")
    print(f"  Agents: {N_AGENTS}")
    print(f"  Tunnel: {TUNNEL_URL}")
    print(f"  Operator: {OPERATOR_URL}")
    print()

    # 1. Launch N agent-bridge processes
    procs = []
    instance_ids = []
    for i in range(N_AGENTS):
        iid = f"swarm-test-{i+1}-{os.getpid()}"
        instance_ids.append(iid)
        env = os.environ.copy()
        env["INSTANCE_ID"] = iid
        env["TUNNEL_URL"] = TUNNEL_URL
        p = subprocess.Popen(
            [sys.executable, os.path.join(os.path.dirname(__file__), "agent-bridge.py")],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        procs.append(p)

    print(f"  Launched {N_AGENTS} agents, waiting 8s for registration...")
    await asyncio.sleep(8)

    # Check all still running
    for i, p in enumerate(procs):
        if p.poll() is not None:
            result(False, f"agent-{i+1} alive", f"exited with code {p.returncode}")
        else:
            result(True, f"agent-{i+1} alive")

    # 2. Connect as operator and call node.list
    try:
        headers = {
            "Origin": "https://app.hanzo.bot",
            "Authorization": f"Bearer {TOKEN}",
        }
        async with websockets.connect(OPERATOR_URL, additional_headers=headers) as ws:
            # Gateway immediately sends connect.challenge event — read it
            challenge_raw = await asyncio.wait_for(ws.recv(), timeout=10)
            challenge = json.loads(challenge_raw)
            if challenge.get("type") == "event" and challenge.get("event") == "connect.challenge":
                print(f"  Got connect.challenge (nonce={challenge['payload']['nonce'][:8]}...)")
            else:
                print(f"  First message: {json.dumps(challenge)[:120]}")

            # Send connect request (protocol v3 format)
            # Use bot-control-ui client ID to get dangerouslyDisableDeviceAuth bypass
            connect_req = make_req("connect", {
                "minProtocol": 3,
                "maxProtocol": 3,
                "client": {
                    "id": "bot-control-ui",
                    "displayName": "Swarm Test",
                    "version": "1.0.0",
                    "platform": f"{platform.system().lower()}-{platform.machine()}",
                    "mode": "ui",
                },
                "auth": {
                    "token": TOKEN,
                },
                "role": "operator",
                "scopes": ["operator.read", "operator.write", "node.read", "node.write"],
            })
            connect_id = connect_req["id"]
            await ws.send(json.dumps(connect_req))

            # Read response — gateway sends type="res" with hello-ok payload
            conn_resp = await read_response(ws, connect_id, timeout=15)
            conn_ok = conn_resp.get("ok", False)
            if conn_ok:
                payload = conn_resp.get("payload", {})
                server_ver = payload.get("server", {}).get("version", "?")
                protocol = payload.get("protocol", "?")
                result(True, "operator connect", f"server={server_ver} protocol={protocol}")
            else:
                err = conn_resp.get("error", {})
                result(False, "operator connect", f"{err.get('code','?')}: {err.get('message','?')}")
                # Can't continue without connect
                raise Exception(f"connect failed: {err}")

            # node.list
            list_req = make_req("node.list")
            list_id = list_req["id"]
            await ws.send(json.dumps(list_req))

            resp = await read_response(ws, list_id)
            ok = resp.get("ok", False)
            payload = resp.get("payload", {})
            nodes = payload.get("nodes", [])
            node_ids = [n["nodeId"] for n in nodes]
            result(ok, "node.list", f"{len(nodes)} nodes")

            # Print all nodes for debugging
            for n in nodes:
                local = n.get("local", "?")
                pod = n.get("podId", "?")
                print(f"    node: {n['nodeId']} (local={local}, pod={pod[:12]}...)")

            # Check all our agents appear
            found = 0
            for iid in instance_ids:
                if iid in node_ids:
                    found += 1
            result(found == N_AGENTS, f"all {N_AGENTS} agents visible",
                   f"{found}/{N_AGENTS} found")

            # 3. Invoke status.get on each agent
            for i, iid in enumerate(instance_ids):
                if iid not in node_ids:
                    result(False, f"invoke agent-{i+1}", "not in node list")
                    continue
                inv_req = make_req("node.invoke", {
                    "nodeId": iid,
                    "command": "status.get",
                    "params": {},
                    "idempotencyKey": str(uuid.uuid4()),
                })
                inv_id = inv_req["id"]
                await ws.send(json.dumps(inv_req))
                try:
                    inv_resp = await read_response(ws, inv_id, timeout=15)
                    inv_ok = inv_resp.get("ok", False)
                    payload = inv_resp.get("payload", {})
                    # Parse payloadJSON if present (nested JSON)
                    if isinstance(payload.get("payloadJSON"), str):
                        inner = json.loads(payload["payloadJSON"])
                    elif isinstance(payload.get("payload"), dict):
                        inner = payload["payload"]
                    elif isinstance(payload.get("payload"), str):
                        inner = json.loads(payload["payload"])
                    else:
                        inner = payload
                    status = inner.get("status", "?") if isinstance(inner, dict) else "?"
                    pid = inner.get("pid", "?") if isinstance(inner, dict) else "?"
                    if not inv_ok:
                        err = inv_resp.get("error", {})
                        result(False, f"invoke agent-{i+1} status.get",
                               f"err={err.get('code','?')}: {err.get('message','?')} raw={json.dumps(payload)[:200]}")
                    else:
                        result(True, f"invoke agent-{i+1} status.get",
                               f"status={status}, pid={pid}")
                except Exception as e:
                    result(False, f"invoke agent-{i+1} status.get", str(e))

    except Exception as e:
        import traceback
        result(False, "operator session", f"{e}\n{traceback.format_exc()}")

    # 4. Cleanup
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
