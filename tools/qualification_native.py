#!/usr/bin/env python3
"""Opt-in, real native clients with scripted loopback models; never owner state.

Requires Python 3.11+, websockets 15.0.1, Claude 2.1.300, standalone Codex
0.165.0 (experimental native queue), and OpenCode 1.18.35. See
docs/compatibility.md for the evidence boundary and build commands.
"""
import argparse
import asyncio
import errno
import hashlib
import http.server
import json
import os
from pathlib import Path
import platform
import shutil
import signal
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import urllib.error
import uuid


def uid():
    return str(uuid.uuid4())


def require(value, message):
    if not value:
        raise RuntimeError(message)
    return value


def wait_for(label, inspect, seconds=60):
    deadline = time.monotonic() + seconds
    last = None
    while time.monotonic() < deadline:
        try:
            value = inspect()
            if value:
                return value
        except (OSError, RuntimeError, ValueError, StopIteration) as error:
            last = str(error)
        time.sleep(.1)
    raise TimeoutError(f"{label}: {last}")


def rpc(state, request):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(5)
        stream.connect(str(state / "bridge.sock"))
        stream.sendall((json.dumps(request) + "\n").encode())
        response = json.loads(stream.makefile().readline(1024 * 1024))
    require(response.get("ok"), f"Broker {request['op']}: {response.get('error')}")
    return response["result"]


class Processes:
    def __init__(self, root):
        self.root, self.children, self.logs = root, [], []
        self.stopped, self.cleanup_errors = set(), []

    def start(self, label, argv, env, cwd, stdin=subprocess.DEVNULL):
        count = sum(existing == label or existing.startswith(label + "-") for existing, _ in self.children)
        if count:
            label += "-" + str(count + 1)
        logs = [open(self.root / f"{label}.{side}.log", "w") for side in ("out", "err")]
        self.logs.extend(logs)
        child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=stdin,
                                 stdout=logs[0], stderr=logs[1], text=True,
                                 start_new_session=True)
        self.children.append((label, child))
        return child

    def stop(self, child):
        if child in self.stopped:
            return
        # Signal before reaping, including when the leader already exited. Its
        # unreaped PID reserves our group ID throughout cleanup. Never poll/wait
        # these tracked children elsewhere, or signal a group twice after reaping.
        errors = []
        for sig in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(child.pid, sig)
            except ProcessLookupError:
                pass
            except OSError as error:
                # Darwin excludes zombies from killpg's target count and can
                # return EPERM for a zombie-only group. Never suppress EPERM
                # while a live member exists or membership cannot be verified.
                # https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c
                benign = False
                if error.errno == errno.EPERM and sys.platform == "darwin":
                    try:
                        members = self.group_members(child.pid)
                        benign = any(pid == child.pid and state.startswith("Z") for pid, state in members) and all(state.startswith("Z") for _, state in members)
                    except Exception as inspect_error:
                        errors.append(f"pid={child.pid} membership: {inspect_error}")
                if not benign:
                    errors.append(f"pid={child.pid} signal={sig.name} {type(error).__name__} errno={error.errno}")
            if sig == signal.SIGTERM:
                time.sleep(1)
        try:
            child.wait(timeout=5)
            self.stopped.add(child)
        except Exception as error:
            errors.append(f"pid={child.pid} wait: {type(error).__name__}: {error}")
        if errors:
            raise RuntimeError("; ".join(errors))

    @staticmethod
    def group_members(group):
        # Metadata only, with a fixed system PATH; no process arguments or owner
        # configuration. The unreaped leader still reserves this exact group ID.
        result = subprocess.run(["ps", "-axo", "pid=,pgid=,stat="], env={"PATH": "/usr/bin:/bin", "LANG": "C"},
                                capture_output=True, text=True, timeout=5, check=True)
        members = []
        for line in result.stdout.splitlines():
            fields = line.split()
            require(len(fields) == 3, "Cannot parse process-group membership")
            pid, pgid = int(fields[0]), int(fields[1])
            if pgid == group:
                members.append((pid, fields[2]))
        return members

    def close(self):
        for label, child in reversed(self.children):
            try:
                self.stop(child)
            except Exception as error:
                self.cleanup_errors.append(f"{label}: {error}")
        for log in self.logs:
            try:
                log.close()
            except Exception as error:
                self.cleanup_errors.append(str(error))
        return {label: child.returncode for label, child in self.children}


def private_env(root):
    for name in ("home", "config", "data", "cache", "state-home", "tmp", "work", "codex", "claude"):
        (root / name).mkdir(mode=0o700)
    return {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(root / "home"),
        "XDG_CONFIG_HOME": str(root / "config"), "XDG_DATA_HOME": str(root / "data"),
        "XDG_CACHE_HOME": str(root / "cache"), "XDG_STATE_HOME": str(root / "state-home"),
        "TMPDIR": str(root / "tmp"), "CODEX_HOME": str(root / "codex"),
        "CLAUDE_CONFIG_DIR": str(root / "claude"), "LANG": "C.UTF-8",
        "DISABLE_AUTOUPDATER": "1", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "OPENCODE_DISABLE_DEFAULT_PLUGINS": "true", "OPENCODE_DISABLE_MODELS_FETCH": "true",
        "OPENCODE_DISABLE_AUTOUPDATE": "true", "OPENCODE_DISABLE_SHARE": "true",
        "OPENCODE_DISABLE_MCP_OAUTH": "true", "DO_NOT_TRACK": "1",
    }


def http_json(base, path, body=None, method=None, timeout=60):
    request = urllib.request.Request(base + path, method=method,
                                    data=None if body is None else json.dumps(body).encode(),
                                    headers={"Content-Type": "application/json"})
    # Cold native dependency/bootstrap work may exceed 20 seconds. Match the
    # existing SDK harness's bounded 60-second request timeout; never retry a
    # timed-out mutation, because the native session may already exist.
    with urllib.request.urlopen(request, timeout=timeout) as response:
        payload = response.read()
        return json.loads(payload) if payload else None


class Fixture:
    """Only emits scripted calls through tools actually exposed by native MCP.

    The provider never sends itself. Main-thread RPC
    observes those effects independently, after actual native tool execution.
    """
    def __init__(self):
        self.active = None
        self.routes, self.stages, self.receipts, self.errors = {}, {}, [], []
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    payload, content_type = fixture.respond(self.path, body)
                    self.send_response(200)
                    self.send_header("Content-Type", content_type)
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except Exception as error:
                    fixture.errors.append(str(error))
                    self.send_error(500, "Qualification fixture failed")

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever)
        try:
            self.thread.start()
        except BaseException:
            self.server.server_close()
            raise
        self.origin = f"http://127.0.0.1:{self.server.server_port}"

    def action(self, path, body):
        text = json.dumps(body.get("messages", body.get("input", [])))
        if not self.active or self.active["marker"] not in text:
            return None
        row = self.active
        if path.startswith("/codex/pending/"):
            path = path.replace("/codex/pending/", "/codex/" + self.codex_route + "/")
        systems = "\n".join(json.dumps(m.get("content")) for m in body.get("messages", []) if m.get("role") == "system")
        route = next((name for name, info in self.routes.items()
                      if (path.startswith('/' + info['provider'] + '/' + name + '/')
                          or path.startswith('/opencode/') and info['provider'] == 'opencode' and 'identity is ' + name in systems)), None)
        if route is None:
            return None  # e.g. title/small-model request without session context
        info = self.routes[route]
        tools = body.get("tools", [])
        if not tools:
            return None
        original = row["marker"]
        # The original sender receives the reply through its actual native adapter.
        is_reply = route == row["sender"]
        wanted_body = "Reply " + original if is_reply else original
        records = rpc(info["state"], {"op": "history", "agent": route, "limit": 100})
        message = next((m for m in records if m["body"] == wanted_body and m["recipient"] == route), None)
        require(message, "Model request had marker but no exact broker recipient record")
        step = self.stages.get((route, original), 0)
        if step != 0:
            return None
        self.stages[(route, original)] = 1
        # Record actual model input in each direction, without a receipt tool call.
        self.receipts.append({"route": route, "marker": original, "message": message["id"],
            "sender_label_visible": message["sender"] in text,
            "correlation_visible": message["id"] in text, "tool": None if is_reply else "send_message"})
        if is_reply:
            return None
        suffix, args = "send_message", {"from": route, "to": message["sender"],
            "body": "Reply " + original, "id": row["reply_id"], "parent": message["id"]}
        if info["provider"] == "codex":
            namespace = next(t for t in tools if t.get("name") == "mcp__bridge")
            found = next(t for t in namespace["tools"] if t["name"].endswith(suffix))
            name = found["name"]
        else:
            name = next(t.get("name", t.get("function", {}).get("name", "")) for t in tools
                        if t.get("name", t.get("function", {}).get("name", "")).endswith(suffix))
        return {"name": name, "arguments": args, "id": "call_" + uid().replace("-", "")}

    def respond(self, path, body):
        call = self.action(path, body)
        if path.startswith("/claude/"):
            message = {"id": "msg_" + uid(), "type": "message", "role": "assistant",
                "model": body["model"], "content": [], "stop_reason": None,
                "stop_sequence": None, "usage": {"input_tokens": 1, "output_tokens": 1}}
            block = {"type": "tool_use", "id": call["id"], "name": call["name"], "input": {}} if call else {"type": "text", "text": ""}
            delta = {"type": "input_json_delta", "partial_json": json.dumps(call["arguments"])} if call else {"type": "text_delta", "text": "Qualification fixture complete."}
            events = [("message_start", {"message": message}), ("content_block_start", {"index": 0, "content_block": block}),
                ("content_block_delta", {"index": 0, "delta": delta}), ("content_block_stop", {"index": 0}),
                ("message_delta", {"delta": {"stop_reason": "tool_use" if call else "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 1}}), ("message_stop", {})]
            return self.sse(events), "text/event-stream"
        if path.startswith("/codex/"):
            item = {"id": "msg_" + uid(), "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "Qualification fixture complete.", "annotations": []}]}
            if call:
                item = {"type": "function_call", "id": call["id"], "call_id": call["id"],
                    "name": call["name"], "namespace": "mcp__bridge", "arguments": json.dumps(call["arguments"]), "status": "completed"}
            response = {"id": "resp_" + uid(), "object": "response", "created_at": int(time.time()),
                "model": "fixture", "status": "completed", "output": [item], "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
            events = [("response.created", {"response": dict(response, status="in_progress", output=[])}),
                ("response.output_item.added", {"output_index": 0, "item": dict(item, status="in_progress", **({"arguments": ""} if call else {"content": []}))})]
            if call:
                events.append(("response.function_call_arguments.delta", {"item_id": item["id"], "output_index": 0, "delta": item["arguments"]}))
            else:
                events.extend([("response.content_part.added", {"item_id": item["id"], "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}),
                    ("response.output_text.delta", {"item_id": item["id"], "output_index": 0, "content_index": 0, "delta": "Qualification fixture complete."})])
            events.extend([("response.output_item.done", {"output_index": 0, "item": item}), ("response.completed", {"response": response})])
            return self.sse(events), "text/event-stream"
        tool = {"id": call["id"], "type": "function", "function": {"name": call["name"], "arguments": json.dumps(call["arguments"])}} if call else None
        content = None if tool else "Qualification fixture complete."
        response = {"id": "chatcmpl-fixture", "object": "chat.completion", "created": int(time.time()), "model": "fixture",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content, **({"tool_calls": [tool]} if tool else {})}, "finish_reason": "tool_calls" if tool else "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}
        if not body.get("stream"):
            return json.dumps(response).encode(), "application/json"
        chunks = [{"role": "assistant", **({"tool_calls": [{"index": 0, **tool}]} if tool else {"content": content})}, {}]
        payload = "".join("data: " + json.dumps(dict(response, object="chat.completion.chunk", choices=[{"index": 0, "delta": delta, "finish_reason": None if i == 0 else response["choices"][0]["finish_reason"]}])) + "\n\n" for i, delta in enumerate(chunks))
        return (payload + "data: [DONE]\n\n").encode(), "text/event-stream"

    @staticmethod
    def sse(events):
        return "".join("event: " + event + "\ndata: " + json.dumps(dict(data, type=event)) + "\n\n" for event, data in events).encode()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        require(not self.thread.is_alive(), "Fixture HTTP thread leaked")


class Qualification:
    def __init__(self, args, root, evidence):
        self.args, self.root, self.evidence = args, root, evidence
        self.processes = Processes(root)
        # A disk-backed cache must not put the native workspace beneath an owner
        # home: OpenCode searches project ancestors for configuration.
        self.workroot = Path(tempfile.mkdtemp(prefix="abqw-", dir="/tmp"))
        self.envs, self.states, self.brokers, self.oc = {}, {}, {}, {}
        self.codex_socket = None
        try:
            self.workroot.chmod(0o700)
            self.fixture = Fixture()
        except BaseException:
            shutil.rmtree(self.workroot)
            raise

    def work(self, side):
        return self.workroot / side

    def command(self, argv, env, cwd):
        result = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, timeout=120)
        require(result.returncode == 0, f"{argv[1:]} exit {result.returncode}: {result.stderr}")
        return result.stdout

    def broker(self, side):
        env, root = self.envs[side], self.root / side
        child = self.processes.start("broker-" + side, [self.args.binary, "--state-dir", str(self.states[side]), "serve"], env, self.work(side))
        self.brokers[side] = child
        status = wait_for("broker startup", lambda: rpc(self.states[side], {"op": "status"}))
        require(status["build_commit"] == self.evidence["source_commit"], "Binary build_commit differs from source HEAD; rebuild with AGENT_BRIDGE_BUILD_COMMIT")

    def opencode(self, side):
        root, env, state = self.root / side, self.envs[side], self.states[side]
        config_dir = Path(env["XDG_CONFIG_HOME"]) / "opencode"
        config_dir.mkdir(mode=0o700)
        config = {"$schema": "https://opencode.ai/config.json", "enabled_providers": ["fixture"], "model": "fixture/fixture", "small_model": "fixture/fixture",
            "provider": {"fixture": {"npm": "@ai-sdk/openai-compatible", "name": "Fixture", "options": {"baseURL": self.fixture.origin + "/opencode/v1", "apiKey": "local-fixture-only"},
                "models": {"fixture": {"name": "Fixture", "limit": {"context": 32768, "output": 2048}, "tool_call": True}}}}}
        (config_dir / "opencode.json").write_text(json.dumps(config))
        self.command([self.args.binary, "--state-dir", str(state), "opencode", "install"], env, self.work(side))
        with socket.socket() as reserve:
            reserve.bind(("127.0.0.1", 0))
            port = reserve.getsockname()[1]
        argv = [self.args.opencode, "serve", "--hostname", "127.0.0.1", "--port", str(port)]
        native = self.processes.start("opencode-" + side, argv, env, self.work(side))
        base = f"http://127.0.0.1:{port}"
        wait_for("OpenCode startup", lambda: http_json(base, "/global/health", timeout=2))
        # Global health can precede lazy project/plugin dependency bootstrap.
        # Exercise the same project API with a retriable read before the first
        # mutation. A timed-out create must never be issued a second time.
        wait_for("OpenCode project bootstrap", lambda: isinstance(http_json(base, "/session", timeout=2), list), 180)
        self.oc[side] = {"base": base, "process": native, "argv": argv}
        return self.oc_session(side, "Qualification " + side)

    def oc_session(self, side, title):
        session = http_json(self.oc[side]["base"], "/session", {"title": title})["id"]
        return self.route("opencode", session, self.states[side])

    def route(self, provider, session, state):
        name = provider + "-" + session
        wait_for("live " + name, lambda: next((a for a in rpc(state, {"op": "list", "all": False}) if a["name"] == name), None))
        self.fixture.routes[name] = {"provider": provider, "session": session, "state": state}
        return name

    def claude(self):
        root, env, state = self.root / "a", dict(self.envs["a"]), self.states["a"]
        session = uid()
        name = "claude-" + session
        env.update(ANTHROPIC_API_KEY="local-fixture-only", ANTHROPIC_BASE_URL=self.fixture.origin + "/claude/" + name)
        endpoint = root / "tmp/inbox.sock"
        mcp = root / "claude-mcp.json"
        mcp.write_text(json.dumps({"mcpServers": {"bridge": {"command": self.args.binary, "args": ["--state-dir", str(state), "mcp"]}}}))
        argv = [self.args.claude, "--bare", "--setting-sources", "", "--strict-mcp-config", "--mcp-config", str(mcp), "--tools", "",
            "--allowedTools", "mcp__bridge__send_message", "--print", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
            "--replay-user-messages", "--model", "claude-fixture", "--session-id", session, "--messaging-socket-path", str(endpoint)]
        native = self.processes.start("claude", argv, env, self.work("a"), stdin=subprocess.PIPE)
        native.stdin.write(json.dumps({"type": "user", "message": {"role": "user", "content": "Initialize disposable qualification session."}}) + "\n")
        native.stdin.flush()
        wait_for("Claude inbox", endpoint.exists)
        # --bare intentionally skips owner hooks; registration is explicit.
        rpc(state, {"op": "register", "agent": {"name": name, "provider": "claude", "session": session, "endpoint": str(endpoint), "enabled": True}, "replace": False})
        return self.route("claude", session, state)

    async def codex(self):
        from websockets.asyncio.client import unix_connect
        root, env = self.root / "a", self.envs["a"]
        shared = Path(f"/tmp/codex-daemon-{os.getuid()}")
        shared.mkdir(mode=0o700, exist_ok=True)
        meta = shared.lstat()
        require(stat.S_ISDIR(meta.st_mode) and meta.st_uid == os.getuid() and stat.S_IMODE(meta.st_mode) & 0o077 == 0, "Codex socket directory is not private")
        self.codex_socket = shared / ("agb7-" + uid() + ".sock")
        config = Path(env["CODEX_HOME"]) / "config.toml"
        config.write_text('[mcp_servers.bridge]\nenabled_tools = ["send_message"]\ncommand = ' + json.dumps(self.args.binary) + '\nargs = ["--state-dir", ' + json.dumps(str(self.states['a'])) + ', "mcp"]\n[mcp_servers.bridge.tools.send_message]\napproval_mode = "approve"\n')
        # A unique provider endpoint is filled after thread/start identifies the route.
        argv = [self.args.codex, "app-server", "--listen", "unix://" + str(self.codex_socket)]
        for setting in ['model_provider="fixture"', 'model="fixture"', 'model_providers.fixture.name="fixture"',
                        'model_providers.fixture.base_url=' + json.dumps(self.fixture.origin + '/codex/pending/v1'),
                        'model_providers.fixture.wire_api="responses"', 'model_providers.fixture.requires_openai_auth=false', 'model_providers.fixture.supports_websockets=false']:
            argv += ["-c", setting]
        self.processes.start("codex", argv, env, self.work("a"))
        await asyncio.to_thread(wait_for, "Codex socket", self.codex_socket.exists)
        self.ws = await unix_connect(str(self.codex_socket), uri="ws://localhost/")
        self.codex_seq = 0
        await self.crpc("initialize", {"clientInfo": {"name": "agb7_qualification", "version": "1"}, "capabilities": {"experimentalApi": True}})
        await self.ws.send(json.dumps({"method": "initialized"}))
        result = await self.crpc("thread/start", {"cwd": str(self.work("a")), "model": "fixture", "approvalPolicy": "never", "sandbox": "read-only"})
        session = result["thread"]["id"]
        self.codex_name = "codex-" + session
        # Fixture resolves the one native Codex destination by its provider path.
        self.fixture.codex_route = self.codex_name
        await self.crpc("thread/name/set", {"threadId": session, "name": "Qualification Codex"})
        return await asyncio.to_thread(self.route, "codex", session, self.states["a"])

    async def crpc(self, method, params):
        self.codex_seq += 1
        ident = self.codex_seq
        await self.ws.send(json.dumps({"id": ident, "method": method, "params": params}))
        async with asyncio.timeout(20):
            while True:
                response = json.loads(await self.ws.recv())
                if response.get("id") == ident:
                    require("error" not in response, f"Codex {method}: {response.get('error')}")
                    return response["result"]

    def roundtrip(self, sender, recipient, remote=None):
        source, dest = self.fixture.routes[sender], self.fixture.routes[recipient]
        row = {"sender": sender, "recipient": recipient, "marker": "MATRIX-" + uid(), "id": uid(), "reply_id": uid(), "two_broker": remote is not None}
        self.fixture.active = row
        print(f"Native matrix: {source['provider']} -> {dest['provider']} ({'two brokers' if remote else 'local'})", flush=True)
        target = recipient if remote is None else remote + "/" + recipient
        rpc(source["state"], {"op": "send", "id": row["id"], "sender": sender, "recipient": target, "body": row["marker"], "parent": None})
        def complete():
            original = rpc(source["state"], {"op": "get", "id": row["id"]})
            replies = rpc(source["state"], {"op": "history", "agent": sender, "limit": 100})
            reply = next((m for m in replies if m["body"] == "Reply " + row["marker"] and m["recipient"] == sender), None)
            observed = {r["route"] for r in self.fixture.receipts if r["marker"] == row["marker"]}
            source_outcome = "unknown" if dest["provider"] == "claude" else "delivered"
            reply_outcome = "unknown" if source["provider"] == "claude" else "delivered"
            return (original, reply) if original["state"] == source_outcome and reply and reply["state"] == reply_outcome and observed == {sender, recipient} else None
        original, reply = wait_for("native model input and correlated reply " + sender + " -> " + target, complete, 90)
        require(reply["parent"] == row["id"] and reply["hops"] == 1 and reply["recipient_session"] == source["session"], "Reply correlation/session mismatch")
        records = rpc(dest["state"], {"op": "history", "agent": recipient, "limit": 100})
        incoming = next(m for m in records if m["body"] == row["marker"] and m["recipient"] == recipient)
        require(incoming["recipient_session"] == dest["session"], "Destination native identity mismatch")
        receipts = [r for r in self.fixture.receipts if r["marker"] == row["marker"]]
        require(len(receipts) == 2 and sum(r["tool"] == "send_message" for r in receipts) == 1 and all(r["sender_label_visible"] and r["correlation_visible"] for r in receipts), "Missing actual model-visible sender/reference or reply execution")
        for route, marker in [(recipient, row["marker"]), (sender, "Reply " + row["marker"])]:
            info = self.fixture.routes[route]
            if info["provider"] == "opencode":
                self.oc_exact_message(route, marker)
        row.update(incoming_id=incoming["id"], reply_local_id=reply["id"], parent=reply["parent"], original_state=original["state"], reply_state=reply["state"],
                   source_model_receipt=True, destination_model_receipt=True, scripted_mcp_calls=1,
                   task_completion="not established by native acceptance or reply; scripted tools only")
        self.evidence["pairs"].append(row)
        self.fixture.active = None

    def oc_exact_message(self, route, marker):
        info = self.fixture.routes[route]
        side = "a" if info["state"] == self.states["a"] else "b"
        messages = http_json(self.oc[side]["base"], "/session/" + info["session"] + "/message")
        found = [m for m in messages if m["info"]["role"] == "user" and any(p.get("type") == "text" and marker in p["text"] for p in m["parts"])]
        require(len(found) == 1, "Native history must contain exactly one matching user message")
        parts = [p for p in found[0]["parts"] if p.get("type") == "text"]
        require(len(parts) == 1 and "<agent-bridge-session-context>" not in parts[0]["text"], "Native message text duplicated or accumulated setup context")
        return found[0]["info"]["id"]

    @staticmethod
    def lease_ended(state, session):
        with sqlite3.connect(f"file:{state / 'messages.sqlite3'}?mode=ro", uri=True) as db:
            row = db.execute("SELECT expires FROM opencode_leases WHERE session=?", (session,)).fetchone()
        return row is None or row[0] <= int(time.time() * 1000)

    def lifecycle(self, route):
        info, state, native = self.fixture.routes[route], self.states["a"], self.oc["a"]
        session = info["session"]
        path = "/session/" + session
        http_json(native["base"], path, {"title": "Qualification renamed"}, "PATCH")
        wait_for("native rename discovery", lambda: any(a["name"] == route and a.get("display_name") == "Qualification renamed" for a in rpc(state, {"op": "list", "all": False})))
        with sqlite3.connect(f"file:{state / 'messages.sqlite3'}?mode=ro", uri=True) as db:
            old_generation = db.execute("SELECT generation FROM opencode_leases WHERE session=?", (session,)).fetchone()[0]
        identity = rpc(state, {"op": "identity"})
        def durable_grants():
            # Live enabled/name/machine labels may change during restart; the
            # security contract is the exact peer/provider/session permissions.
            return [{key: row[key] for key in ("peer_id", "provider", "session", "permissions", "shared")}
                    for row in rpc(state, {"op": "shared_sessions"})]
        grants = durable_grants()
        history_id = self.evidence["pairs"][0]["id"]
        rpc(state, {"op": "pause", "paused": True})
        self.processes.stop(self.brokers["a"])
        self.broker("a")
        require(rpc(state, {"op": "status"})["paused"], "Broker restart lost pause")
        require(rpc(state, {"op": "identity"}) == identity, "Broker restart changed identity")
        require(durable_grants() == grants, "Broker restart lost exact session grants")
        require(rpc(state, {"op": "get", "id": history_id})["state"] == self.evidence["pairs"][0]["original_state"], "Broker restart lost retained history")
        rpc(state, {"op": "pause", "paused": False})
        self.processes.stop(native["process"])
        wait_for("closed plugin expires native routes", lambda: not any(a["name"] == route for a in rpc(state, {"op": "list", "all": False})))
        # A broker restart disables routes before their old leases expire. A new
        # plugin must respect that live-generation fence, rather than using the
        # disabled presentation flag as evidence that replacement is allowed.
        wait_for("closed plugin generation lease ends", lambda: self.lease_ended(state, session))
        native["process"] = self.processes.start("opencode-a-restarted", native["argv"], self.envs["a"], self.work("a"))
        wait_for("native restart health", lambda: http_json(native["base"], "/global/health", timeout=2))
        http_json(native["base"], path)  # history lookup does not activate a route
        require(not any(a["name"] == route for a in rpc(state, {"op": "list", "all": False})), "Stored history lookup activated native route")
        http_json(native["base"], path + "/prompt_async", {"parts": [{"type": "text", "text": "Explicit disposable resume after reload."}]})
        self.route("opencode", session, state)
        with sqlite3.connect(f"file:{state / 'messages.sqlite3'}?mode=ro", uri=True) as db:
            new_generation = db.execute("SELECT generation FROM opencode_leases WHERE session=?", (session,)).fetchone()[0]
        require(new_generation != old_generation, "Plugin replacement reused a stale generation")
        # All previously inserted native user records survive restart exactly once.
        for row in self.evidence["pairs"]:
            if row["recipient"] == route:
                self.oc_exact_message(route, row["marker"])
            if row["sender"] == route:
                self.oc_exact_message(route, "Reply " + row["marker"])
        http_json(native["base"], path, method="DELETE")
        wait_for("native delete removes registration", lambda: not any(a["name"] == route for a in rpc(state, {"op": "list", "all": False})))
        self.evidence["lifecycle"] = {"native_rename": True, "broker_restart_retained_identity_history_grants_pause": True,
            "plugin_close": True, "history_lookup_inactive": True, "explicit_resume_same_native_id": True,
            "replacement_new_generation": True, "native_history_no_duplicates_after_restart": True, "native_delete_unregister": True}

    def pair_brokers(self, first, second):
        a, b = self.states["a"], self.states["b"]
        for local, other, alias in [(a, b, "b"), (b, a, "a")]:
            peer = rpc(other, {"op": "identity"})["peer_id"]
            rpc(local, {"op": "peer_pair", "peer_id": peer, "fingerprint": peer, "alias": alias})
            rpc(local, {"op": "network_configure", "enabled": True, "listen": "/ip4/127.0.0.1/udp/0/quic-v1"})
        for local, other, alias in [(a, b, "b"), (b, a, "a")]:
            address = wait_for("QUIC listen", lambda: rpc(other, {"op": "status"})["network"].get("listening", []))[0]
            rpc(local, {"op": "peer_address", "peer": alias, "address": address})
        wait_for("two brokers connected", lambda: any(c["state"] == "connected" for c in rpc(a, {"op": "peer_connections"})))
        require(rpc(a, {"op": "remote_list"}) == [] and rpc(b, {"op": "remote_list"}) == [], "Native sessions shared by default")
        for state, alias, name in [(a, "b", first), (b, "a", second)]:
            if not any(row["name"] == name for row in rpc(state, {"op": "list", "all": False})):
                # A long local matrix may outlive an idle native project's live
                # registration. The driver explicitly resumes its exact existing
                # disposable conversation before choosing it for owner sharing;
                # neither the observer nor an incoming peer creates/resumes it.
                info = self.fixture.routes[name]
                wait_for("old sharing generation ends", lambda: self.lease_ended(state, info["session"]))
                side = "a" if state == a else "b"
                http_json(self.oc[side]["base"], "/session/" + info["session"] + "/prompt_async",
                          {"parts": [{"type": "text", "text": "Explicitly resume this disposable conversation for sharing qualification."}]})
                self.route("opencode", info["session"], state)
            rpc(state, {"op": "peer_share", "peer": alias, "agent": name, "shared": True})
        wait_for("explicit destination directory", lambda: any(r["name"] == second for r in rpc(a, {"op": "remote_list"})))
        wait_for("explicit source directory", lambda: any(r["name"] == first for r in rpc(b, {"op": "remote_list"})))
        self.evidence["two_broker"] = {"transport": "same-host real loopback QUIC", "default_private": True, "explicit_independent_session_grants": True, "physical_cross_machine": "outstanding"}

    async def run(self):
        for side in ("a", "b"):
            root = self.root / side
            root.mkdir(mode=0o700)
            self.work(side).mkdir(mode=0o700)
            self.envs[side] = private_env(root)
            self.states[side] = root / "broker"
            self.broker(side)
        oc_a, oc_b = self.opencode("a"), self.opencode("b")
        oc_a2 = self.oc_session("a", "Qualification second local session")
        claude = self.claude()
        codex = await self.codex()
        for sender, recipient in [(claude, oc_a), (oc_a, claude), (codex, oc_a), (oc_a, codex), (oc_a, oc_a2), (oc_a2, oc_a)]:
            await asyncio.to_thread(self.roundtrip, sender, recipient)
        self.pair_brokers(oc_a, oc_b)
        for sender, recipient, peer in [(oc_a, oc_b, "b"), (oc_b, oc_a, "a")]:
            await asyncio.to_thread(self.roundtrip, sender, recipient, peer)
        await asyncio.to_thread(self.lifecycle, oc_a)
        require(not self.fixture.errors, f"Fixture errors: {self.fixture.errors}")
        self.evidence["model_receipts"] = self.fixture.receipts

    async def close(self):
        self.evidence["model_receipts"] = self.fixture.receipts
        self.evidence["fixture_errors"] = self.fixture.errors
        errors = []
        if hasattr(self, "ws"):
            try:
                await self.ws.close()
            except Exception as error:
                errors.append(str(error))
        exits = self.processes.close()
        errors.extend(self.processes.cleanup_errors)
        for clean in (self.fixture.close, self.remove_socket, lambda: shutil.rmtree(self.workroot)):
            try:
                clean()
            except Exception as error:
                errors.append(str(error))
        self.evidence["cleanup"] = {"children_exited": exits, "http_thread_alive": self.fixture.thread.is_alive(),
            "owned_codex_socket_exists": bool(self.codex_socket and self.codex_socket.exists()), "errors": errors}
        if errors or any(code is None for code in exits.values()):
            self.evidence.update(result="failed", error="Qualification cleanup failed")

    def remove_socket(self):
        if self.codex_socket and self.codex_socket.exists():
            meta = self.codex_socket.lstat()
            require(stat.S_ISSOCK(meta.st_mode) and meta.st_uid == os.getuid(), "Refuse unexpected owned socket cleanup")
            self.codex_socket.unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=lambda v: str(Path(v).resolve()))
    for name in ("claude", "codex", "opencode"):
        parser.add_argument("--" + name, default=shutil.which(name))
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--scratch-parent", type=Path, default=Path(tempfile.gettempdir()),
                        help="Private run directory is created here; use disk storage if tmpfs quota is small")
    args = parser.parse_args()
    import websockets
    require(websockets.__version__ == "15.0.1", "Native runner requires websockets 15.0.1")
    require(not args.evidence.exists(), "Evidence file exists; choose a fresh path")
    repo = Path(__file__).resolve().parent.parent
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
    evidence = {"source_commit": commit, "binary_sha256": hashlib.sha256(Path(args.binary).read_bytes()).hexdigest(),
        "os": platform.platform(), "architecture": platform.machine(), "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "model": "scripted localhost fixture, no external model judgment", "websockets": websockets.__version__,
        "python": platform.python_version(), "codex_matrix": "unheld-turn workflows; separate active-turn qualification in AGB-13",
        "pairs": [], "outstanding": ["native macOS", "physical cross-machine", "interactive rendering", "external model judgment"]}
    evidence["checkout_status"] = subprocess.check_output(["git", "status", "--porcelain"], cwd=repo, text=True).splitlines()
    sources = [p for folder in ("src", "plugins/opencode") for p in (repo / folder).rglob("*")
               if p.is_file() and p.suffix in (".rs", ".ts") and "node_modules" not in p.parts]
    sources += [repo / p for p in ("Cargo.toml", "Cargo.lock", "build.rs", "plugins/opencode/bun.lock", "tools/qualification_native.py", "tools/test_qualification.py")]
    evidence["source_sha256"] = {str(p.relative_to(repo)): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(sources)}
    subprocess.run(["git", "diff", "--exit-code", "HEAD", "--", "src", "Cargo.toml", "Cargo.lock", "build.rs", "plugins/opencode/index.ts", "plugins/opencode/journal.ts", "plugins/opencode/peer.ts", "plugins/opencode/rpc.ts"], cwd=repo, check=True, stdout=subprocess.DEVNULL)
    for name, expected in [("claude", "2.1.300"), ("codex", "0.165.0"), ("opencode", "1.18.35")]:
        executable = require(getattr(args, name), name + " executable unavailable")
        result = subprocess.run([executable, "--version"], capture_output=True, text=True, timeout=15, check=True)
        actual = result.stdout.strip().split()[1 if name == "codex" else 0]
        require(expected == actual, f"Unsupported {name}: {result.stdout.strip()}")
        evidence[name] = {"version": result.stdout.strip(), "executable": str(Path(executable).resolve())}
    root = Path(tempfile.mkdtemp(prefix="abq-", dir=args.scratch_parent)).resolve()
    root.chmod(0o700)
    evidence["disposable_root"] = str(root)
    async def run():
        harness = None
        try:
            harness = Qualification(args, root, evidence)
            await harness.run()
            evidence["result"] = "passed"
        except Exception as error:
            evidence.update(result="failed", error=str(error))
        except BaseException:
            evidence.update(result="interrupted")
            raise
        finally:
            if harness is not None:
                await harness.close()
    try:
        asyncio.run(run())
    finally:
        args.evidence.parent.mkdir(parents=True, exist_ok=True)
        args.evidence.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps({"result": evidence.get("result", "failed"), "evidence": str(args.evidence), "pairs": len(evidence["pairs"]), "disposable_root": str(root)}))
    return 0 if evidence.get("result") == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
