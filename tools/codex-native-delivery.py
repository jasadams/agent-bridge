#!/usr/bin/env python3
"""Disposable Codex queue-boundary diagnostic; never uses owner configuration.

Requires Python 3.11+, websockets, codex 0.165.0 and a current agent-bridge.
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
import signal
import socket
import stat
import subprocess
import tempfile
import threading
import time
import uuid


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
        self.root = Path(tempfile.mkdtemp(prefix="agb11-codex-"))
        self.root.chmod(0o700)
        for child in ("home", "codex", "state"):
            (self.root / child).mkdir(mode=0o700)
        self.env = {"PATH": os.environ["PATH"], "HOME": str(self.root / "home"),
                    "CODEX_HOME": str(self.root / "codex"), "TMPDIR": str(self.root),
                    "LANG": "C.UTF-8"}
        self.events = []
        self.cases = []
        self.children = []
        self.files = []
        self.current = None
        self.lock = threading.Lock()
        parent = Path(f"/tmp/codex-daemon-{os.getuid()}")
        meta = parent.lstat()
        assert stat.S_ISDIR(meta.st_mode) and meta.st_uid == os.getuid()
        assert stat.S_IMODE(meta.st_mode) & 0o077 == 0
        self.sock = parent / ("agb11-" + uuid.uuid4().hex + ".sock")
        self.native_sock = self.root / "native.sock"
        self.server = None
        self.worker = None

    def event(self, kind, **data):
        with self.lock:
            self.events.append(dict(stamp(), kind=kind, **data))

    def spawn(self, label, command):
        out = (self.root / (label + ".stdout")).open("w")
        err = (self.root / (label + ".stderr")).open("w")
        self.files += [out, err]
        proc = subprocess.Popen(command, cwd=self.root, env=self.env,
                                stdin=subprocess.DEVNULL, stdout=out, stderr=err, start_new_session=True)
        self.children.append((label, proc))
        return proc

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
                        message = {"id": "fc_hold", "type": "function_call", "call_id": "call_hold",
                                   "name": "exec_command", "arguments": json.dumps({"cmd": "sleep 4" if case["name"] in ("held-tool", "steer-held-tool") else "sleep 1", "yield_time_ms": 10000, "max_output_tokens": 100}), "status": "completed"}
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
        self.worker = threading.Thread(target=self.server.serve_forever)
        self.worker.start()

    async def wait_path(self, path):
        async with asyncio.timeout(15):
            while not path.exists():
                assert all(proc.poll() is None for _, proc in self.children), "child exited"
                await asyncio.sleep(.05)

    async def proxy(self, ws):
        from websockets.asyncio.client import unix_connect
        connection = uuid.uuid4().hex
        async with unix_connect(str(self.native_sock), uri="ws://localhost/") as native:
            async def forward(source, destination, direction):
                async for message in source:
                    frame = json.loads(message)
                    self.event("wire_frame", connection=connection, direction=direction, frame=frame)
                    if (self.current and self.current["name"] in ("held-tool", "steer-held-tool") and direction == "from_native"
                            and frame.get("method") == "item/started"
                            and frame.get("params", {}).get("item", {}).get("type") == "commandExecution"):
                        self.current["gate_entered"].set()
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
                "incoming": str(uuid.uuid4()), "reply": str(uuid.uuid4()), "stage": 0,
                "active_requests": 0, "gate_entered": threading.Event(), "release": threading.Event(), "model_finished": threading.Event()}
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
                assert state["state"] == "uncertain", state
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
            if name != "idle":
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
            final = await self.rpc(ws, "thread/read", {"threadId": case["thread"], "includeTurns": True})
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
        self.fixture()
        config = '[mcp_servers.bridge]\nenabled_tools = ["acknowledge_message", "send_message"]\ncommand = ' + json.dumps(self.args.broker) + '\nargs = ["--state-dir", ' + json.dumps(str(self.root / "state")) + ', "mcp"]\n'
        config += '\n[mcp_servers.bridge.tools.acknowledge_message]\napproval_mode = "approve"\n[mcp_servers.bridge.tools.send_message]\napproval_mode = "approve"\n'
        (self.root / "codex" / "config.toml").write_text(config)
        self.spawn("broker", [self.args.broker, "--state-dir", str(self.root / "state"), "serve", "--no-discovery"])
        await self.wait_path(self.root / "state" / "bridge.sock")
        command = [self.args.codex, "app-server", "--listen", "unix://" + str(self.native_sock)]
        for setting in ['model_provider="fixture"', 'model="fixture"', 'model_providers.fixture.name="fixture"', f'model_providers.fixture.base_url="http://127.0.0.1:{self.server.server_port}/v1"', 'model_providers.fixture.wire_api="responses"', 'model_providers.fixture.requires_openai_auth=false', 'model_providers.fixture.supports_websockets=false']:
            command += ["-c", setting]
        self.spawn("native", command)
        await self.wait_path(self.native_sock)
        from websockets.asyncio.server import unix_serve
        async with unix_serve(self.proxy, path=str(self.sock)):
            self.sock.chmod(0o600)
            for name in self.args.cases.split(","):
                await self.case(name)

    def cleanup(self):
        if self.current:
            self.current["release"].set()
        results = {}
        for label, proc in reversed(self.children):
            # Each owned child has its own session; include its MCP descendants.
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                results[label] = proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                results[label] = proc.wait(timeout=5)
            # A parent can exit before its MCP descendants. Never signal our own group.
            assert proc.pid != os.getpgrp()
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
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
            assert not survivors, (label, survivors)
        if self.server:
            self.server.shutdown()
            self.server.server_close()
            self.worker.join(timeout=5)
            assert not self.worker.is_alive()
        if self.sock.exists():
            meta = self.sock.lstat()
            assert stat.S_ISSOCK(meta.st_mode) and meta.st_uid == os.getuid()
            self.sock.unlink()
        for handle in self.files:
            handle.close()
        (self.root / "events.json").write_text(json.dumps(self.events, indent=2))
        (self.root / "results.json").write_text(json.dumps({"cases": self.cases, "cleanup": results}, indent=2))


def verify_artifacts(root):
    """Recheck saved evidence without starting native or reading an agent inbox."""
    events = json.loads((root / "events.json").read_text())
    results = json.loads((root / "results.json").read_text())
    wire = [event for event in events if event["kind"] == "wire_frame"]
    assert results["cleanup"] == {"native": 0, "broker": 0}, results["cleanup"]
    assert results["cases"], "no completed cases"
    forbidden = {"turn/interrupt", "thread/resume", "thread/queue/start", "thread/queue/delete"}
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
        models = [event for event in rows if event["kind"] == "model_request"
                  and event["queued_marker_present"]]
        if name.startswith("steer"):
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
                assert verified["accepted_count"] == 100 and verified["rejected"]["state"] == "uncertain"
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--broker", type=lambda value: str(Path(value).resolve()), required=False)
    parser.add_argument("--codex", default="/home/jason/.local/bin/codex")
    parser.add_argument("--cases", default="idle,generating,held-tool,long-turn,disconnected,saturation,steer,steer-held-tool")
    parser.add_argument("--hold-seconds", type=float, default=2)
    parser.add_argument("--verify-artifacts", type=Path, help="offline evidence self-check; starts no processes")
    args = parser.parse_args()
    if args.verify_artifacts:
        summary = verify_artifacts(args.verify_artifacts)
        print("VERIFIED", ",".join(case["name"] for case in summary["cases"]))
        return
    if not args.broker:
        parser.error("--broker is required for native runs")
    assert 0 < args.hold_seconds <= 10
    assert set(args.cases.split(",")) <= {"idle", "generating", "held-tool", "long-turn", "disconnected", "saturation", "steer", "steer-held-tool"}
    probe = Probe(args)
    print("ARTIFACT_ROOT", probe.root, flush=True)
    import websockets
    versions = {"platform": platform.platform(), "python": platform.python_version(),
                "websockets": websockets.__version__, "source_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=Path(__file__).resolve().parent.parent, text=True).strip()}
    for name, binary in (("broker", args.broker), ("codex", args.codex)):
        versions[name] = {"path": str(Path(binary).resolve()), "sha256": hashlib.sha256(Path(binary).resolve().read_bytes()).hexdigest(), "version": subprocess.check_output([binary, "--version"], text=True).strip()}
    (probe.root / "versions.json").write_text(json.dumps(versions, indent=2))
    try:
        asyncio.run(probe.run())
    finally:
        probe.cleanup()
    verify_artifacts(probe.root)
    print("PASS", ",".join(case["name"] for case in probe.cases), flush=True)


if __name__ == "__main__":
    main()
