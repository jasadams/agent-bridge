#!/usr/bin/env python3
"""Disposable Codex delivery diagnostic and production regression; isolated state only.

Requires Linux, Python 3.11+, websockets 15.0.1 and a current agent-bridge.
Codex runtime version is recorded; eligibility is verified from its protocol.
All model traffic is served by a deterministic loopback Responses fixture.
"""
import argparse
import asyncio
import datetime
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import socket
import stat
import subprocess
import tempfile
import threading
import time
import uuid

from qualification_native import Processes


def stamp():
    return {"utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "monotonic_ns": time.monotonic_ns()}


def text_of(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(text_of(v) for v in value)
    if isinstance(value, dict):
        return "\n".join(text_of(v) for v in value.values())
    return ""


class Probe:
    def __init__(self, args):
        self.args = args
        self.root = Path(tempfile.mkdtemp(prefix="agb13-codex-" if args.production else "agb11-codex-"))
        self.root.chmod(0o700)
        for child in ("home", "codex", "state"):
            (self.root / child).mkdir(mode=0o700)
        self.env = {"PATH": os.environ["PATH"], "HOME": str(self.root / "home"),
                    "CODEX_HOME": str(self.root / "codex"), "TMPDIR": str(self.root),
                    "LANG": "C.UTF-8"}
        self.events = []
        self.cases = []
        self.processes = Processes(self.root)
        self.children = self.processes.children
        self.current = None
        self.expected_commit = None
        self.lock = threading.Lock()
        parent = Path(f"/tmp/codex-daemon-{os.getuid()}")
        self.sock = parent / ("agb11-" + uuid.uuid4().hex + ".sock")
        self.native_sock = self.root / "native.sock"
        self.socket_identity = None
        self.server = None
        self.worker = None

    def event(self, kind, **data):
        with self.lock:
            self.events.append(dict(stamp(), kind=kind, **data))

    def spawn(self, label, command):
        return self.processes.start(label, command, self.env, self.root)

    def broker(self, value):
        self.event("broker_request", frame=value)
        with socket.socket(socket.AF_UNIX) as conn:
            conn.settimeout(5)
            conn.connect(str(self.root / "state" / "bridge.sock"))
            conn.sendall((json.dumps(value) + "\n").encode())
            data = b""
            while b"\n" not in data:
                chunk = conn.recv(65536)
                if not chunk:
                    raise RuntimeError("broker closed without reply")
                data += chunk
                assert len(data) <= 1024 * 1024
        result = json.loads(data.split(b"\n")[0])
        self.event("broker_response", frame=result)
        assert result["ok"], result
        return result["result"]

    def fixture(self):
        probe = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                assert 0 < length <= 4 * 1024 * 1024
                body = json.loads(self.rfile.read(length))
                case = probe.current
                assert case is not None
                assert not self.headers.get("Authorization"), "fixture must not receive credentials"
                flattened = text_of(body.get("input", []))
                queued = case["marker"] in flattened
                probe.event("model_request", case=case["name"], queued_marker_present=queued,
                            body=body)
                if not queued:
                    case["active_requests"] += 1
                    if case["name"] not in ("held-tool", "long-turn", "steer-held-tool") or case["active_requests"] > 1:
                        case["gate_entered"].set()
                        assert case["release"].wait(30), "fixture gate deadline"
                    message = {"id": "msg_active", "type": "message", "role": "assistant",
                               "status": "completed", "content": [{"type": "output_text",
                               "text": "ACTIVE_TURN_FINISHED", "annotations": []}]}
                    if case["name"] in ("held-tool", "long-turn", "steer-held-tool") and case["active_requests"] == 1:
                        command = "sleep 4" if case["name"] in ("held-tool", "steer-held-tool") else "sleep 1"
                        if case["production"]:
                            command = "sh -c '" + command + "; printf " + case["tool_marker"] + "'"
                        message = {"id": "fc_hold", "type": "function_call", "call_id": "call_hold",
                                   "name": "exec_command", "arguments": json.dumps({"cmd": command, "yield_time_ms": 10000, "max_output_tokens": 100}), "status": "completed"}
                else:
                    stage = case["stage"]
                    if stage < 2:
                        suffix = "acknowledge_message" if stage == 0 else "send_message"
                        namespace = next(t for t in body["tools"] if t.get("name") == "mcp__bridge")
                        tool = next(t for t in namespace["tools"] if t["name"].endswith(suffix))
                        arguments = ({"id": case["incoming"], "agent": case["alias"]} if stage == 0
                                     else {"id": case["reply"], "from": case["alias"], "to": "user",
                                           "body": case["marker"] + " reply", "parent": case["incoming"]})
                        message = {"id": f"fc_{stage}", "type": "function_call",
                                   "call_id": f"call_{stage}", "name": tool["name"],
                                   "namespace": "mcp__bridge", "arguments": json.dumps(arguments),
                                   "status": "completed"}
                        case["stage"] += 1
                    else:
                        message = {"id": "msg_fixture", "type": "message", "role": "assistant",
                                   "status": "completed", "content": [{"type": "output_text",
                                   "text": "QUEUED_PUSH_ACK_REPLY_COMPLETE", "annotations": []}]}
                response = {"id": "resp_" + uuid.uuid4().hex, "object": "response",
                            "created_at": int(time.time()), "model": "fixture", "status": "completed",
                            "output": [message], "usage": {"input_tokens": 1, "output_tokens": 1,
                            "total_tokens": 2}}
                events = [("response.created", {"response": dict(response, status="in_progress", output=[])}),
                          ("response.output_item.added", {"output_index": 0, "item": dict(message, status="in_progress", **({"arguments": ""} if message["type"] == "function_call" else {"content": []}))})]
                if message["type"] == "function_call":
                    events.append(("response.function_call_arguments.delta", {"item_id": message["id"], "output_index": 0, "delta": message["arguments"]}))
                else:
                    events += [("response.content_part.added", {"item_id": message["id"], "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}),
                               ("response.output_text.delta", {"item_id": message["id"], "output_index": 0, "content_index": 0, "delta": message["content"][0]["text"]})]
                events += [("response.output_item.done", {"output_index": 0, "item": message}),
                           ("response.completed", {"response": response})]
                payload = "".join("event: " + name + "\ndata: " + json.dumps(dict(data, type=name)) + "\n\n" for name, data in events).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                if queued and case["stage"] == 2 and message["type"] == "message":
                    case["model_finished"].set()

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        try:
            self.worker = threading.Thread(target=self.server.serve_forever)
            self.worker.start()
        except BaseException:
            # shutdown() blocks when serve_forever never started. A failed
            # thread startup owns only a bound server, which must be closed.
            self.worker = None
            try:
                self.server.server_close()
            finally:
                self.server = None
            raise

    async def wait_path(self, path, seconds=15):
        async def observe():
            while not path.exists():
                # Observe exit without reaping: cleanup must reserve every owned
                # process-group ID until its final group signal is complete.
                assert all(os.waitid(os.P_PID, proc.pid, os.WEXITED | os.WNOWAIT | os.WNOHANG) is None
                           for _, proc in self.children), "child exited"
                await asyncio.sleep(.05)
        await asyncio.wait_for(observe(), timeout=seconds)

    async def proxy(self, ws):
        from websockets.asyncio.client import unix_connect
        connection = uuid.uuid4().hex
        requests = {}
        adapter_connection = False
        async with unix_connect(str(self.native_sock), uri="ws://localhost/") as native:
            async def forward(source, destination, direction):
                nonlocal adapter_connection
                async for message in source:
                    frame = json.loads(message)
                    if direction == "to_native" and "id" in frame and frame.get("method"):
                        requests[frame["id"]] = frame["method"]
                        if frame["method"] == "initialize":
                            adapter_connection = frame["params"]["clientInfo"]["name"] == "agent_bridge_queue"
                    self.event("wire_frame", connection=connection, direction=direction, frame=frame)
                    if (self.current and self.current["name"] in ("held-tool", "steer-held-tool") and direction == "from_native"
                            and frame.get("method") == "item/started"
                            and frame.get("params", {}).get("item", {}).get("type") == "commandExecution"):
                        self.current["gate_entered"].set()
                    if (self.args.production and self.current and self.current["name"] == "saturation"
                            and direction == "from_native" and adapter_connection
                            and requests.get(frame.get("id")) == "thread/read"
                            and frame.get("result", {}).get("thread", {}).get("id") == self.current.get("thread")
                            and "canAcceptDirectInput" in frame.get("result", {}).get("thread", {})):
                        transformed = json.loads(message)
                        del transformed["result"]["thread"]["canAcceptDirectInput"]
                        self.event("compatibility_metadata", case="saturation", connection=connection,
                                   original=frame, transformed=transformed)
                        message = json.dumps(transformed)
                    await destination.send(message)
            tasks = [asyncio.create_task(forward(ws, native, "to_native")),
                     asyncio.create_task(forward(native, ws, "from_native"))]
            try:
                done, _ = await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
                from websockets.exceptions import ConnectionClosed
                for task in done:
                    try:
                        task.result()
                    except ConnectionClosed as error:
                        self.event("wire_connection_closed", connection=connection, reason=str(error))
            finally:
                for task in tasks:
                    task.cancel()
                await asyncio.gather(*tasks, return_exceptions=True)

    async def connect(self):
        from websockets.asyncio.client import unix_connect
        ws = await unix_connect(str(self.sock), uri="ws://localhost/")
        result = await self.rpc(ws, "initialize", {"clientInfo": {"name": "agb11_fixture", "version": "1"}, "capabilities": {"experimentalApi": True}})
        assert Path(result["codexHome"]).resolve() == (self.root / "codex").resolve()
        await ws.send(json.dumps({"method": "initialized"}))
        return ws

    async def rpc(self, ws, method, params):
        request_id = uuid.uuid4().hex
        self.event("native_request", method=method, id=request_id, params=params)
        await ws.send(json.dumps({"id": request_id, "method": method, "params": params}))
        async with asyncio.timeout(15):
            while True:
                value = json.loads(await ws.recv())
                self.event("native_frame", frame=value)
                assert not (value.get("method") and "id" in value), "unexpected native request; never auto-approve"
                if value.get("id") == request_id:
                    assert "error" not in value, value
                    return value["result"]

    async def case(self, name):
        case = {"name": name, "alias": "fixture-" + name, "marker": "AGB11_" + uuid.uuid4().hex,
                "tool_marker": "AGB13_TOOL_" + uuid.uuid4().hex, "incoming": str(uuid.uuid4()), "reply": str(uuid.uuid4()), "stage": 0,
                "production": self.args.production, "active_requests": 0, "gate_entered": threading.Event(), "release": threading.Event(), "model_finished": threading.Event()}
        self.current = case
        ws = await self.connect()
        try:
            thread = (await self.rpc(ws, "thread/start", {"cwd": str(self.root), "model": "fixture", "approvalPolicy": "never", "sandbox": "read-only"}))["thread"]
            case["thread"] = thread["id"]
            self.event("case_start", case=name, thread=thread)
            self.broker({"op": "register", "agent": {"name": case["alias"], "provider": "codex", "session": case["thread"], "endpoint": "", "enabled": True}, "replace": False})
            if name != "idle":
                started = await self.rpc(ws, "turn/start", {"threadId": case["thread"], "input": [{"type": "text", "text": "DISPOSABLE_ACTIVE_TURN", "text_elements": []}]})
                case["active_turn"] = started["turn"]["id"]
                async with asyncio.timeout(15):
                    while not case["gate_entered"].is_set():
                        await asyncio.sleep(.05)
            if name.startswith("steer"):
                case["stage"] = 2  # Direct API feasibility, no broker send or MCP claim.
                fresh = await self.connect()
                try:
                    read = await self.rpc(fresh, "thread/read", {"threadId": case["thread"]})
                    assert read["thread"]["canAcceptDirectInput"] is True, read
                    turns = await self.rpc(fresh, "thread/turns/list", {"threadId": case["thread"], "limit": 1, "sortDirection": "desc", "itemsView": "summary"})
                    assert len(turns["data"]) == 1 and turns["data"][0]["status"] == "inProgress", turns
                    assert turns["data"][0]["id"] == case["active_turn"]
                    result = await self.rpc(fresh, "turn/steer", {"threadId": case["thread"],
                        "expectedTurnId": case["active_turn"], "clientUserMessageId": case["incoming"],
                        "input": [{"type": "text", "text": case["marker"], "text_elements": []}]})
                    self.event("steer_accepted", case=name, result=result)
                finally:
                    await fresh.close()
                self.event("steer_connection_closed", case=name)
                case["release"].set()
                async with asyncio.timeout(40):
                    while not case["model_finished"].is_set():
                        frame = json.loads(await ws.recv())
                        self.event("native_frame", frame=frame)
                        assert not (frame.get("method") and "id" in frame), "unexpected native request"
                async with asyncio.timeout(40):
                    while True:
                        frame = json.loads(await ws.recv())
                        self.event("native_frame", frame=frame)
                        assert not (frame.get("method") and "id" in frame), "unexpected native request"
                        if frame.get("method") == "turn/completed":
                            turn = frame["params"]["turn"]
                            assert turn["id"] == case["active_turn"] and turn["status"] == "completed" and turn["error"] is None, turn
                            break
                read = await self.rpc(ws, "thread/read", {"threadId": case["thread"], "includeTurns": True})
                assert case["marker"] in text_of(read)
                assert any(turn["id"] == case["active_turn"] and turn["status"] == "completed" for turn in read["thread"]["turns"])
                self.event("steer_verified", case=name, thread=read)
                self.cases.append({key: value for key, value in case.items() if key not in ("release", "gate_entered", "model_finished")})
                return
            if name == "disconnected":
                await ws.close()
                ws = None
                self.event("observer_disconnected", case=name)
            if name == "saturation":
                # Fill only this disposable held thread; preserve exact IDs/errors.
                for index in range(100):
                    entry = str(uuid.uuid4())
                    added = await self.rpc(ws, "thread/queue/add", {"threadId": case["thread"], "clientUserMessageId": entry, "input": [{"type": "text", "text": f"DISPOSABLE_QUEUE_{index}", "text_elements": []}]})
                    assert added["queuedSubmission"]["clientUserMessageId"] == entry
                case["stage"] = 2
            self.broker({"op": "send", "id": case["incoming"], "sender": "user", "recipient": case["alias"], "body": case["marker"], "parent": None})
            # Read only native queue diagnostics; no inbox reads trigger execution.
            if ws is None:
                ws = await self.connect()
                self.event("observer_reconnected", case=name)
            async with asyncio.timeout(8):
                while True:
                    state = self.broker({"op": "get", "id": case["incoming"]})
                    if state["state"] != "queued" and state["state"] != "sending":
                        break
                    await asyncio.sleep(.05)
            if name == "saturation":
                assert state["state"] == ("failed" if self.args.production else "uncertain"), state
                queue = await self.rpc(ws, "thread/queue/list", {"threadId": case["thread"], "limit": 100})
                assert len(queue["data"]) == 100 and queue["nextCursor"] is None, queue
                assert all(row["clientUserMessageId"] != case["incoming"] for row in queue["data"])
                self.event("saturation_verified", case=name, rejected=state, accepted_count=100)
                # End the held native turn naturally; fixture gives final answers to remaining queue.
                case["release"].set()
                async with asyncio.timeout(40):
                    while True:
                        drained = await self.rpc(ws, "thread/queue/list", {"threadId": case["thread"], "limit": 100})
                        status = await self.rpc(ws, "thread/read", {"threadId": case["thread"]})
                        if not drained["data"] and status["thread"]["status"]["type"] == "idle":
                            break
                        await asyncio.sleep(.2)
                self.event("saturation_drained", case=name)
                self.cases.append({key: value for key, value in case.items() if key not in ("release", "gate_entered", "model_finished")})
                return
            assert state["state"] in ("submitted", "acknowledged"), state
            queue = await self.rpc(ws, "thread/queue/list", {"threadId": case["thread"], "limit": 100})
            self.event("queue_snapshot", case=name, result=queue)
            if self.args.production and name != "idle":
                assert not any(row["clientUserMessageId"] == case["incoming"] for row in queue["data"]), queue
                case["release"].set()
                self.event("fixture_released", case=name)
            elif name != "idle":
                assert any(row["clientUserMessageId"] == case["incoming"] for row in queue["data"]), queue
                await asyncio.sleep(self.args.hold_seconds)
                assert case["stage"] == 0, "queued input reached model during gated active turn"
                read = await self.rpc(ws, "thread/read", {"threadId": case["thread"], "includeTurns": True})
                assert case["marker"] not in text_of(read), "queued input inserted before active turn release"
                self.event("active_boundary_verified", case=name)
                case["release"].set()
                self.event("fixture_released", case=name)
            if name == "disconnected":
                # Fresh metadata-only connections receive no turn subscription events.
                # The third model request follows both actual MCP tool results.
                async with asyncio.timeout(40):
                    while not case["model_finished"].is_set():
                        await asyncio.sleep(.05)
            else:
                async with asyncio.timeout(40):
                    while True:
                        value = json.loads(await ws.recv())
                        self.event("native_frame", frame=value)
                        assert not (value.get("method") and "id" in value), "unexpected native request"
                        if value.get("method") == "turn/completed" and case["stage"] == 2:
                            assert value["params"]["turn"]["status"] == "completed" and value["params"]["turn"]["error"] is None, value
                            break
            async with asyncio.timeout(15):
                while True:
                    final = await self.rpc(ws, "thread/read", {"threadId": case["thread"], "includeTurns": True})
                    if all(turn["status"] == "completed" for turn in final["thread"]["turns"]):
                        break
                    await asyncio.sleep(.05)
            assert case["marker"] in text_of(final)
            original = self.broker({"op": "get", "id": case["incoming"]})
            reply = self.broker({"op": "get", "id": case["reply"]})
            assert original["state"] == "acknowledged", original
            assert reply["parent"] == case["incoming"] and reply["sender"] == case["alias"] and reply["recipient"] == "user", reply
            self.event("case_verified", case=name, incoming=original, reply=reply)
            self.cases.append({key: value for key, value in case.items() if key not in ("release", "gate_entered", "model_finished")})
        finally:
            case["release"].set()
            if ws is not None:
                await ws.close()

    async def run(self):
        meta = self.sock.parent.lstat()
        assert stat.S_ISDIR(meta.st_mode) and meta.st_uid == os.getuid()
        assert stat.S_IMODE(meta.st_mode) & 0o077 == 0
        assert not self.sock.exists(), "unique proxy path already exists"
        self.fixture()
        config = '[mcp_servers.bridge]\nenabled_tools = ["acknowledge_message", "send_message"]\ncommand = ' + json.dumps(self.args.broker) + '\nargs = ["--state-dir", ' + json.dumps(str(self.root / "state")) + ', "mcp"]\n'
        config += '\n[mcp_servers.bridge.tools.acknowledge_message]\napproval_mode = "approve"\n[mcp_servers.bridge.tools.send_message]\napproval_mode = "approve"\n'
        (self.root / "codex" / "config.toml").write_text(config)
        self.spawn("broker", [self.args.broker, "--state-dir", str(self.root / "state"), "serve", "--no-discovery"])
        await self.wait_path(self.root / "state" / "bridge.sock")
        if self.args.production:
            status = self.broker({"op": "status"})
            assert status.get("build_commit") == self.expected_commit, "rebuild broker with AGENT_BRIDGE_BUILD_COMMIT matching source HEAD"
        command = [self.args.codex, "app-server", "--listen", "unix://" + str(self.native_sock)]
        for setting in ['model_provider="fixture"', 'model="fixture"', 'model_providers.fixture.name="fixture"', f'model_providers.fixture.base_url="http://127.0.0.1:{self.server.server_port}/v1"', 'model_providers.fixture.wire_api="responses"', 'model_providers.fixture.requires_openai_auth=false', 'model_providers.fixture.supports_websockets=false']:
            command += ["-c", setting]
        self.spawn("native", command)
        await self.wait_path(self.native_sock)
        from websockets.asyncio.server import unix_serve
        async with unix_serve(self.proxy, path=str(self.sock)):
            self.sock.chmod(0o600)
            meta = self.sock.lstat()
            self.socket_identity = (meta.st_dev, meta.st_ino)
            for name in self.args.cases.split(","):
                await self.case(name)

    def cleanup(self):
        if self.current:
            self.current["release"].set()
        # Shared cleanup signals groups while their unreaped leaders still own
        # the group IDs, attempts every child and then closes every log.
        results = self.processes.close()
        errors = list(self.processes.cleanup_errors)
        audits = {}
        def cleanup(label, action):
            try:
                action()
            except Exception as error:
                errors.append(f"{label}: {error}")
        for label, proc in self.children:
            def audit(label=label, proc=proc):
                deadline = time.monotonic() + 2
                while True:
                    survivors = []
                    for entry in Path("/proc").iterdir():
                        if not entry.name.isdigit():
                            continue
                        try:
                            fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
                            if int(fields[2]) == proc.pid and fields[0] != "Z":
                                survivors.append(int(entry.name))
                        except (FileNotFoundError, ProcessLookupError):
                            pass
                    if not survivors or time.monotonic() >= deadline:
                        break
                    time.sleep(.05)
                audits[label] = {"process_group": proc.pid, "surviving_non_zombie_pids": survivors, **stamp()}
                assert not survivors, (label, survivors)
            cleanup(label + " group audit", audit)
        if self.server:
            cleanup("provider shutdown", self.server.shutdown)
            cleanup("provider close", self.server.server_close)
        if self.worker:
            cleanup("provider join", lambda: self.worker.join(timeout=5))
            if self.worker.is_alive():
                errors.append("Provider thread did not stop")
        def remove_socket():
            if self.socket_identity is not None and self.sock.exists():
                meta = self.sock.lstat()
                assert stat.S_ISSOCK(meta.st_mode) and meta.st_uid == os.getuid()
                assert (meta.st_dev, meta.st_ino) == self.socket_identity
                self.sock.unlink()
        cleanup("proxy socket", remove_socket)
        (self.root / "events.json").write_text(json.dumps(self.events, indent=2))
        (self.root / "results.json").write_text(json.dumps({"cases": self.cases, "cleanup": results,
            "cleanup_audit": audits, "cleanup_errors": errors}, indent=2))
        if errors:
            raise RuntimeError("Native regression cleanup failed: " + "; ".join(errors))


def verify_production_active(case, rows, wire, outgoing, models, timeline):
    """Reconstruct production delivery from raw requests, native state and model input."""
    assert len(outgoing) == 1 and outgoing[0]["frame"]["method"] == "turn/steer", outgoing
    request = outgoing[0]
    params = request["frame"]["params"]
    assert params["threadId"] == case["thread"] and params["expectedTurnId"] == case["active_turn"]
    connection = [event for event in wire if event["connection"] == request["connection"]]
    methods = [event["frame"].get("method") for event in connection if event["direction"] == "to_native"]
    assert methods[:2] == ["initialize", "initialized"] and methods[-1] == "turn/steer", methods
    assert "thread/loaded/list" in methods, methods
    assert [method for method in methods if method != "thread/loaded/list"] == ["initialize", "initialized", "thread/read", "thread/turns/list", "turn/steer"], methods
    def response(method):
        query = next(event for event in connection if event["direction"] == "to_native" and event["frame"].get("method") == method)
        replies = [event for event in connection if event["direction"] == "from_native" and event["frame"].get("id") == query["frame"]["id"] and not event["frame"].get("method")]
        assert len(replies) == 1
        assert query["monotonic_ns"] < replies[0]["monotonic_ns"]
        return query, replies[0]
    _, metadata = response("thread/read")
    assert metadata["frame"]["result"]["thread"]["canAcceptDirectInput"] is True
    query, turns = response("thread/turns/list")
    assert query["frame"]["params"]["limit"] == 1
    assert turns["frame"]["result"]["data"][0]["id"] == case["active_turn"]
    assert turns["frame"]["result"]["data"][0]["status"] == "inProgress"
    _, receipt = response("turn/steer")
    assert receipt["frame"]["result"]["turnId"] == case["active_turn"]
    assert metadata["monotonic_ns"] < query["monotonic_ns"] < turns["monotonic_ns"] < request["monotonic_ns"]
    verified = next(event for event in rows if event["kind"] == "case_verified")
    assert verified["incoming"]["id"] == case["incoming"] and verified["incoming"]["state"] == "acknowledged"
    assert verified["reply"]["id"] == case["reply"] and verified["reply"]["parent"] == case["incoming"]
    assert verified["reply"]["sender"] == case["alias"] and verified["reply"]["recipient"] == "user"
    assert len(models) == 3
    outputs = {item["call_id"]: item["output"] for item in models[-1]["body"]["input"] if item.get("type") == "function_call_output"}
    assert case["incoming"] in outputs.get("call_0", "") and '"state":"acknowledged"' in outputs["call_0"]
    assert case["reply"] in outputs.get("call_1", "") and '"state":"queued"' in outputs["call_1"]
    reads = [event["frame"].get("result", {}).get("thread", {}) for event in wire]
    original = [turn for read in reads if read.get("id") == case["thread"] for turn in read.get("turns", []) if turn["id"] == case["active_turn"]]
    assert any(turn["status"] == "completed" and turn.get("error") is None for turn in original)
    assert all(turn["status"] != "interrupted" for turn in original)
    completed = [event for event in wire if event["frame"].get("method") == "turn/completed" and event["frame"].get("params", {}).get("turn", {}).get("id") == case["active_turn"]]
    if case["name"] != "disconnected":
        assert completed and models[0]["monotonic_ns"] < completed[0]["monotonic_ns"]
    else:
        # Final native history has only the original turn: no queued follow-up turn.
        assert any(len(read.get("turns", [])) == 1 and read["turns"][0]["id"] == case["active_turn"] and read["turns"][0]["status"] == "completed" for read in reads if read.get("id") == case["thread"])
    if case["name"] in ("held-tool", "long-turn"):
        tools = [event for event in wire if event["frame"].get("method") == "item/completed" and event["frame"].get("params", {}).get("threadId") == case["thread"] and event["frame"]["params"].get("turnId") == case["active_turn"] and event["frame"]["params"].get("item", {}).get("id") == "call_hold"]
        assert tools
        tool = tools[0]["frame"]["params"]["item"]
        assert tool["status"] == "completed" and tool["exitCode"] == 0
        outputs = [item for item in models[0]["body"]["input"] if item.get("type") == "function_call_output" and item.get("call_id") == "call_hold"]
        assert len(outputs) == 1 and "Process exited with code 0" in outputs[0]["output"]
        assert tool["aggregatedOutput"] == case["tool_marker"]
        assert case["tool_marker"] in outputs[0]["output"]
        assert tools[0]["monotonic_ns"] < models[0]["monotonic_ns"]
        timeline["held_tool_completed"] = tools[0]["utc"]
    timeline.update(path="production_active_steer", native_accepted=receipt["utc"], model_received=models[0]["utc"], ack_and_reply_verified=verified["utc"])


def verify_artifacts(root):
    """Recheck saved evidence without starting native or reading an agent inbox."""
    events = json.loads((root / "events.json").read_text())
    results = json.loads((root / "results.json").read_text())
    wire = [event for event in events if event["kind"] == "wire_frame"]
    assert not results.get("cleanup_errors", []), results.get("cleanup_errors")
    assert results["cleanup"] == {"native": 0, "broker": 0}, results["cleanup"]
    assert results["cases"], "no completed cases"
    if any(case.get("production") for case in results["cases"]):
        assert set(results["cleanup_audit"]) == {"native", "broker"}
        assert all(not audit["surviving_non_zombie_pids"] for audit in results["cleanup_audit"].values())
    forbidden = {"turn/interrupt", "turn/cancel", "thread/resume", "thread/queue/start", "thread/queue/delete"}
    assert not any(event["frame"].get("method") in forbidden for event in wire)
    summary = {"versions": json.loads((root / "versions.json").read_text()), "cases": []}
    for case in results["cases"]:
        name = case["name"]
        identifier = case["incoming"]
        rows = [event for event in events if event.get("case") == name]
        outgoing = [event for event in wire if event["direction"] == "to_native"
                    and event["frame"].get("params", {}).get("clientUserMessageId") == identifier]
        timeline = {"name": name, "thread": case["thread"], "message_id": identifier}
        native_items = {}
        for event in wire:
            frame = event["frame"]
            candidates = []
            if frame.get("params", {}).get("threadId") == case["thread"]:
                candidates.append(frame["params"].get("item", {}))
            thread = frame.get("result", {}).get("thread", {})
            if thread.get("id") == case["thread"]:
                candidates += [item for turn in thread.get("turns", []) for item in turn.get("items", [])]
            for item in candidates:
                if item.get("type") == "userMessage" and item.get("clientId") == identifier:
                    native_items.setdefault(item["id"], (event, item))
        if name == "saturation":
            assert not native_items, "rejected submission inserted in native conversation"
        else:
            assert len(native_items) == 1, (name, native_items.keys())
            observed, item = next(iter(native_items.values()))
            assert len(outgoing) == 1 and item["content"] == outgoing[0]["frame"]["params"]["input"]
            timeline.update(native_user_item=item["id"], native_user_observed=observed["utc"])
        model_rows = [event for event in rows if event["kind"] == "model_request"]
        assert all(event["queued_marker_present"] == (case["marker"] in text_of(event["body"].get("input", []))) for event in model_rows)
        models = [event for event in model_rows if case["marker"] in text_of(event["body"].get("input", []))]
        if case.get("production"):
            assert not any(event["kind"] == "compatibility_metadata" for event in rows) or name == "saturation"
            sends = [event for event in events if event["kind"] == "broker_request" and event["frame"].get("op") == "send" and event["frame"].get("id") == identifier]
            assert len(sends) == 1 and sends[0]["frame"]["body"] == case["marker"]
            marker_writes = [event for event in wire if event["direction"] == "to_native" and event["frame"].get("method") in ("thread/queue/add", "turn/steer") and case["marker"] in text_of(event["frame"].get("params", {}).get("input", []))]
            assert marker_writes == outgoing
            assert case["marker"] in text_of(outgoing[0]["frame"]["params"]["input"])
            adapter_methods = [event["frame"].get("method") for event in wire if event["connection"] == outgoing[0]["connection"] and event["direction"] == "to_native"]
            assert set(adapter_methods) <= {"initialize", "initialized", "thread/loaded/list", "thread/read", "thread/turns/list", "thread/queue/add", "turn/steer"}
        if case.get("production") and name not in ("idle", "saturation"):
            verify_production_active(case, rows, wire, outgoing, models, timeline)
        elif name.startswith("steer"):
            assert len(outgoing) == 1 and outgoing[0]["frame"]["method"] == "turn/steer"
            connection = outgoing[0]["connection"]
            connection_methods = [event["frame"].get("method") for event in wire
                                  if event["connection"] == connection and event["direction"] == "to_native"]
            assert connection_methods == ["initialize", "initialized", "thread/read", "thread/turns/list", "turn/steer"], connection_methods
            accepted = next(event for event in rows if event["kind"] == "steer_accepted")
            closed = next(event for event in rows if event["kind"] == "steer_connection_closed")
            verified = next(event for event in rows if event["kind"] == "steer_verified")
            assert accepted["result"]["turnId"] == case["active_turn"]
            assert models and models[0]["monotonic_ns"] > closed["monotonic_ns"]
            assert any(turn["id"] == case["active_turn"] and turn["status"] == "completed"
                       for turn in verified["thread"]["thread"]["turns"])
            if name == "steer-held-tool":
                completed_tools = [event for event in wire if event["frame"].get("method") == "item/completed"
                    and event["frame"].get("params", {}).get("threadId") == case["thread"]
                    and event["frame"]["params"].get("turnId") == case["active_turn"]
                    and event["frame"]["params"].get("item", {}).get("id") == "call_hold"]
                assert completed_tools
                tool = completed_tools[0]["frame"]["params"]["item"]
                assert tool["type"] == "commandExecution" and tool["status"] == "completed" and tool["exitCode"] == 0, tool
                outputs = [item for item in models[0]["body"]["input"]
                           if item.get("type") == "function_call_output" and item.get("call_id") == "call_hold"]
                assert len(outputs) == 1 and "Process exited with code 0" in outputs[0]["output"], outputs
                assert models[0]["monotonic_ns"] > completed_tools[0]["monotonic_ns"]
                timeline["held_tool_completed"] = completed_tools[0]["utc"]
            timeline.update(accepted=accepted["utc"], connection_closed=closed["utc"],
                            model_received=models[0]["utc"], verified=verified["utc"])
        else:
            assert len(outgoing) == 1 and outgoing[0]["frame"]["method"] == "thread/queue/add"
            request = outgoing[0]
            replies = [event for event in wire if event["connection"] == request["connection"]
                       and event["direction"] == "from_native"
                       and event["frame"].get("id") == request["frame"]["id"]
                       and not event["frame"].get("method")]
            assert len(replies) == 1
            reply = replies[0]
            timeline.update(queue_request=request["utc"], queue_response=reply["utc"])
            if name == "saturation":
                assert reply["frame"]["error"] == {"code": -32600, "message": "queue cannot contain more than 100 submissions"}
                verified = next(event for event in rows if event["kind"] == "saturation_verified")
                assert verified["accepted_count"] == 100 and verified["rejected"]["state"] == ("failed" if case.get("production") else "uncertain")
                accepted_ids = set()
                for entry in wire:
                    frame = entry["frame"]
                    if entry["direction"] != "to_native" or frame.get("method") != "thread/queue/add":
                        continue
                    params = frame["params"]
                    if params["threadId"] != case["thread"] or params["clientUserMessageId"] == identifier:
                        continue
                    responses = [response["frame"] for response in wire
                        if response["direction"] == "from_native" and response["connection"] == entry["connection"]
                        and response["frame"].get("id") == frame["id"] and not response["frame"].get("method")]
                    assert len(responses) == 1 and responses[0].get("result", {}).get("queuedSubmission", {}).get("clientUserMessageId") == params["clientUserMessageId"]
                    accepted_ids.add(params["clientUserMessageId"])
                assert len(accepted_ids) == 100
                snapshots = [event["frame"].get("result", {}) for event in wire
                             if event["frame"].get("result", {}).get("data")
                             and len(event["frame"]["result"]["data"]) == 100
                             and all("clientUserMessageId" in row for row in event["frame"]["result"]["data"])]
                assert any({row["clientUserMessageId"] for row in snapshot["data"]} == accepted_ids
                           and snapshot["nextCursor"] is None for snapshot in snapshots)
                assert identifier not in accepted_ids
                assert not models, "rejected submission reached the model"
                assert any(event["kind"] == "saturation_drained" for event in rows)
                if case.get("production"):
                    compatibility = [event for event in rows if event["kind"] == "compatibility_metadata"]
                    assert compatibility
                    for event in compatibility:
                        original = event["original"]
                        assert original["result"]["thread"]["id"] == case["thread"]
                        assert original["result"]["thread"]["canAcceptDirectInput"] is True
                        expected = json.loads(json.dumps(original))
                        del expected["result"]["thread"]["canAcceptDirectInput"]
                        assert expected == event["transformed"]
                        queries = [entry for entry in wire if entry["connection"] == event["connection"] and entry["direction"] == "to_native" and entry["frame"].get("id") == original["id"]]
                        assert len(queries) == 1 and queries[0]["frame"]["method"] == "thread/read"
                        assert queries[0]["frame"]["params"]["threadId"] == case["thread"]
                        initializes = [entry for entry in wire if entry["connection"] == event["connection"] and entry["direction"] == "to_native" and entry["frame"].get("method") == "initialize"]
                        assert len(initializes) == 1 and initializes[0]["frame"]["params"]["clientInfo"]["name"] == "agent_bridge_queue"
                    timeline["fixture"] = "missing-capability compatibility metadata; real native queue capacity"
                timeline.update(error=reply["frame"]["error"], accepted_count=100)
            else:
                queued = reply["frame"]["result"]["queuedSubmission"]
                assert queued["clientUserMessageId"] == identifier
                assert queued["input"] == request["frame"]["params"]["input"]
                verified = next(event for event in rows if event["kind"] == "case_verified")
                assert verified["incoming"]["id"] == identifier and verified["incoming"]["state"] == "acknowledged"
                assert verified["reply"]["parent"] == identifier
                assert len(models) == 3, (name, len(models))
                timeline.update(native_queue_id=queued["id"], model_received=models[0]["utc"],
                                ack_and_reply_verified=verified["utc"])
                if name == "disconnected":
                    reads = [event["frame"].get("result", {}).get("thread", {}) for event in wire
                             if event["frame"].get("result", {}).get("thread", {}).get("id") == case["thread"]]
                    assert any(any(turn["id"] == case["active_turn"] and turn["status"] == "completed" for turn in read.get("turns", []))
                               and len(read.get("turns", [])) == 2 and all(turn["status"] == "completed" for turn in read["turns"]) for read in reads)
                if name != "idle":
                    released = next(event for event in rows if event["kind"] == "fixture_released")
                    assert models[0]["monotonic_ns"] > released["monotonic_ns"]
                    timeline["active_released"] = released["utc"]
                    if name != "disconnected":
                        completed = [event for event in wire if event["frame"].get("method") == "turn/completed"
                            and event["frame"].get("params", {}).get("turn", {}).get("id") == case["active_turn"]]
                        assert completed and completed[0]["frame"]["params"]["turn"]["status"] == "completed"
                        assert models[0]["monotonic_ns"] > completed[0]["monotonic_ns"]
                        timeline["original_turn_completed"] = completed[0]["utc"]
        summary["cases"].append(timeline)
    (root / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return summary


def verify_negative_tests(root):
    """Reject independent corruptions of saved production wire/model evidence."""
    verify_artifacts(root)
    original = json.loads((root / "events.json").read_text())
    cases = json.loads((root / "results.json").read_text())["cases"]
    by_name = {case["name"]: case for case in cases}
    assert all(name in by_name for name in ("generating", "held-tool", "saturation"))
    assert all(case.get("production") for case in cases)
    rejected = []
    for corruption in ("missing_acceptance", "missing_tool_completion", "interrupted_original",
                       "premature_model", "wrong_native_client", "missing_capability",
                       "double_write", "altered_compatibility"):
        events = json.loads(json.dumps(original))
        active = by_name["generating"]
        write = next(event for event in events if event["kind"] == "wire_frame" and event["direction"] == "to_native" and event["frame"].get("params", {}).get("clientUserMessageId") == active["incoming"])
        if corruption == "missing_acceptance":
            events = [event for event in events if not (event["kind"] == "wire_frame" and event["direction"] == "from_native" and event["connection"] == write["connection"] and event["frame"].get("id") == write["frame"]["id"])]
        elif corruption == "missing_tool_completion":
            events = [event for event in events if not (event["kind"] == "wire_frame" and event["frame"].get("method") == "item/completed" and event["frame"].get("params", {}).get("threadId") == by_name["held-tool"]["thread"] and event["frame"]["params"].get("item", {}).get("id") == "call_hold")]
        elif corruption in ("interrupted_original", "wrong_native_client"):
            def mutate(value):
                if isinstance(value, dict):
                    if corruption == "interrupted_original" and value.get("id") == active["active_turn"] and "status" in value:
                        value["status"] = "interrupted"
                    if corruption == "wrong_native_client" and value.get("type") == "userMessage" and value.get("clientId") == active["incoming"]:
                        value["clientId"] = str(uuid.uuid4())
                    for child in value.values():
                        mutate(child)
                elif isinstance(value, list):
                    for child in value:
                        mutate(child)
            mutate(events)
        elif corruption == "premature_model":
            for event in events:
                if event.get("case") == "generating" and event["kind"] == "model_request" and event["queued_marker_present"]:
                    event["monotonic_ns"] = 2 ** 63
        elif corruption == "missing_capability":
            for event in events:
                if event["kind"] == "wire_frame" and event["connection"] == write["connection"]:
                    event["frame"].get("result", {}).get("thread", {}).pop("canAcceptDirectInput", None)
        elif corruption == "double_write":
            events.append(json.loads(json.dumps(write)))
        else:
            next(event for event in events if event["kind"] == "compatibility_metadata")["transformed"]["result"]["thread"]["id"] = str(uuid.uuid4())
        with tempfile.TemporaryDirectory(prefix="agb13-verifier-negative-") as temporary:
            target = Path(temporary)
            (target / "events.json").write_text(json.dumps(events))
            for filename in ("results.json", "versions.json"):
                (target / filename).write_bytes((root / filename).read_bytes())
            try:
                verify_artifacts(target)
            except (AssertionError, KeyError, StopIteration):
                rejected.append(corruption)
            else:
                raise AssertionError("verifier accepted corruption: " + corruption)
    return rejected


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--broker", type=lambda value: str(Path(value).resolve()), required=False)
    parser.add_argument("--codex", default="/home/jason/.local/bin/codex")
    parser.add_argument("--cases", default="idle,generating,held-tool,long-turn,disconnected,saturation,steer,steer-held-tool")
    parser.add_argument("--production", action="store_true", help="verify built production adapter active steer through broker sends")
    parser.add_argument("--hold-seconds", type=float, default=2)
    parser.add_argument("--verify-artifacts", type=Path, help="offline evidence self-check; starts no processes")
    parser.add_argument("--verify-negative-tests", type=Path, help="offline production evidence corruption tests")
    args = parser.parse_args()
    if args.verify_negative_tests:
        rejected = verify_negative_tests(args.verify_negative_tests)
        print("REJECTED", ",".join(rejected))
        return
    if args.verify_artifacts:
        summary = verify_artifacts(args.verify_artifacts)
        print("VERIFIED", ",".join(case["name"] for case in summary["cases"]))
        return
    if not args.broker:
        parser.error("--broker is required for native runs")
    if args.production and args.cases == "idle,generating,held-tool,long-turn,disconnected,saturation,steer,steer-held-tool":
        args.cases = "idle,generating,held-tool,long-turn,disconnected,saturation"
    if args.production and any(name.startswith("steer") for name in args.cases.split(",")):
        parser.error("--production requires broker-send cases; steer cases are historical direct-API feasibility only")
    assert 0 < args.hold_seconds <= 10
    assert set(args.cases.split(",")) <= {"idle", "generating", "held-tool", "long-turn", "disconnected", "saturation", "steer", "steer-held-tool"}
    args.codex = str(Path(args.codex).resolve(strict=True))
    probe = Probe(args)
    try:
        print("ARTIFACT_ROOT", probe.root, flush=True)
        import websockets
        assert platform.system() == "Linux" and tuple(map(int, platform.python_version_tuple()[:2])) >= (3, 11)
        assert websockets.__version__ == "15.0.1", "native regression pins websockets 15.0.1"
        versions = {"platform": platform.platform(), "python": platform.python_version(),
                    "websockets": websockets.__version__, "production": args.production,
                    "source_diff_sha256": hashlib.sha256(subprocess.check_output(["git", "diff", "HEAD"], cwd=Path(__file__).resolve().parent.parent)).hexdigest(), "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=Path(__file__).resolve().parent.parent, text=True).strip()}
        repository = Path(__file__).resolve().parent.parent
        probe.expected_commit = versions["source_commit"]
        if args.production:
            subprocess.run(["git", "diff", "--exit-code", "HEAD", "--", "src", "Cargo.toml", "Cargo.lock", "build.rs"], cwd=repository, check=True, stdout=subprocess.DEVNULL)
        paths = subprocess.check_output(["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"], cwd=repository).decode().split("\0")
        versions["source_files_sha256"] = {path: hashlib.sha256((repository / path).read_bytes()).hexdigest()
                                           for path in sorted(set(paths)) if path and (repository / path).is_file()}
        for name, binary in (("broker", args.broker), ("codex", args.codex)):
            versions[name] = {"path": str(Path(binary).resolve()), "sha256": hashlib.sha256(Path(binary).resolve().read_bytes()).hexdigest(), "version": subprocess.check_output([binary, "--version"], text=True, env=probe.env, cwd=probe.root, timeout=10).strip()}
        (probe.root / "versions.json").write_text(json.dumps(versions, indent=2))
        asyncio.run(probe.run())
    finally:
        probe.cleanup()
    verify_artifacts(probe.root)
    print("PASS", ",".join(case["name"] for case in probe.cases), flush=True)


if __name__ == "__main__":
    main()
