#!/usr/bin/env python3
"""Disposable local monitor acceptance: messages, boards and local/P2P traffic.

Creates private temporary broker state only. No native sessions, model calls, SSH,
service installation, or changes to host networking/browser profiles. --browser
uses an already installed Python Playwright/browser; no downloads are performed.
"""
import argparse
from contextlib import ExitStack
import hashlib
import http.client as http_client
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import subprocess
import tempfile
import time
from urllib.parse import urlencode, urlsplit
import uuid


def wait(label, check, seconds=35):
    until = time.monotonic() + seconds
    last = None
    while time.monotonic() < until:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, RuntimeError, KeyError) as error:
            last = str(error)
        time.sleep(.08)
    raise AssertionError(f"Timed out: {label}; last error: {last}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--browser", action="store_true")
    args = parser.parse_args()
    output = (args.output or Path(tempfile.mkdtemp(prefix="ab-monitor-qa-", dir="/tmp"))).resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(output, 0o700)
    binary = output / "frozen-agent-bridge"
    shutil.copy2(args.binary.resolve(strict=True), binary)
    binary.chmod(0o700)
    with binary.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    results = {"scope": "isolated local brokers and real loopback P2P; no model calls",
               "binary_sha256": digest, "checks": [], "browser_requested": args.browser}
    processes, files, environments = [], [], {}

    def record(name, **details):
        results["checks"].append({"check": name, "passed": True, **details})
        print(f"PASS: {name}", flush=True)

    def rpc(state, op, **fields):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(30)
            connection.connect(str(state / "bridge.sock"))
            connection.sendall(json.dumps({"op": op, **fields}).encode() + b"\n")
            response = connection.makefile("rb").readline(2 * 1024 * 1024)
        value = json.loads(response)
        if not value["ok"]:
            raise RuntimeError(value.get("error", "RPC rejected"))
        return value.get("result")

    def start(name):
        state = output / name
        state.mkdir(mode=0o700)
        home = output / (name + "-home")
        home.mkdir(mode=0o700)
        environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                       "HOME": str(home), "LANG": "C.UTF-8"}
        for variable, relative in [("XDG_CONFIG_HOME", ".config"),
                                   ("XDG_DATA_HOME", ".local/share"),
                                   ("XDG_CACHE_HOME", ".cache"),
                                   ("XDG_STATE_HOME", ".local/state"),
                                   ("CODEX_HOME", "codex"),
                                   ("CLAUDE_CONFIG_DIR", "claude"),
                                   ("OPENCODE_CONFIG_DIR", "opencode")]:
            directory = home / relative
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            environment[variable] = str(directory)
        environments[name] = environment
        log = (output / f"{name}.log").open("wb")
        files.append(log)
        process = subprocess.Popen([str(binary), "--state-dir", str(state), "serve",
                                    "--no-discovery", "--codex-bin", "/missing/codex"],
                                   cwd=output, env=environment, stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=log)
        processes.append(process)
        wait(f"{name} broker", lambda: rpc(state, "status"))
        return state

    def http(url, path, token=None, headers=None, method="GET"):
        address = urlsplit(url)
        connection = http_client.HTTPConnection(address.hostname, address.port, timeout=5)
        request_headers = dict(headers or {})
        if token:
            request_headers["Authorization"] = f"Bearer {token}"
        connection.request(method, path, headers=request_headers)
        response = connection.getresponse()
        body = response.read()
        status, response_headers = response.status, dict(response.getheaders())
        connection.close()
        return status, {key.lower(): value for key, value in response_headers.items()}, body

    def api(url, token, path):
        status, _, body = http(url, path, token)
        assert status == 200, (path, status, body[:200])
        return json.loads(body)

    def messages(url, token, **query):
        return api(url, token, "/api/messages?" + urlencode(query))["messages"]

    def send(state, recipient, body, sender="user", parent=None):
        message_id = str(uuid.uuid4())
        rpc(state, "send", id=message_id, sender=sender, recipient=recipient, body=body, parent=parent)
        return message_id

    def received(state, body):
        return wait("inbox message", lambda: next((message for message in rpc(
            state, "history", agent=None, limit=100) if message["body"] == body), None))

    def opened_stream(url, token):
        address = urlsplit(url)
        connection = http_client.HTTPConnection(address.hostname, address.port, timeout=5)
        connection.request("GET", "/api/events", headers={"Authorization": f"Bearer {token}"})
        response = connection.getresponse()
        assert response.status == 200, response.status
        assert response.getheader("Content-Type").startswith("text/event-stream")
        return connection, response

    def next_event(response):
        lines = []
        while True:
            line = response.readline(8192)
            if not line:
                return None
            if line == b"\n" and lines:
                data = next((line[5:].strip() for line in lines if line.startswith(b"data:")), None)
                if data:
                    return json.loads(data)
                lines = []
            else:
                lines.append(line)

    def board_readback(state, name):
        """Observe the browser's persisted result through real CLI and MCP paths."""
        expected = rpc(state, "named_board_read", board=name)
        raw = subprocess.run([str(binary), "--state-dir", str(state), "board", name, "--raw"],
                             cwd=output, env=environments[state.name], capture_output=True, timeout=10)
        assert raw.returncode == 0, "CLI board read failed"
        assert raw.stdout == expected["markdown"].encode("utf-8"), "CLI board source differs"
        with (output / "board-mcp.stderr").open("wb") as log:
            child = subprocess.Popen([str(binary), "--state-dir", str(state), "mcp"],
                                     cwd=output, env=environments[state.name], stdin=subprocess.PIPE,
                                     stdout=subprocess.PIPE, stderr=log)
            selector = selectors.DefaultSelector()
            os.set_blocking(child.stdout.fileno(), False)
            selector.register(child.stdout, selectors.EVENT_READ)
            pending = bytearray()
            try:
                def write(frame):
                    child.stdin.write(json.dumps(frame).encode() + b"\n")
                    child.stdin.flush()

                def request(identifier, method, params):
                    write({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params})
                    deadline = time.monotonic() + 10
                    for _ in range(64):
                        while b"\n" not in pending:
                            remaining = deadline - time.monotonic()
                            assert remaining > 0 and selector.select(remaining), "MCP read timed out"
                            try:
                                chunk = os.read(child.stdout.fileno(), 65536)
                            except BlockingIOError:
                                continue
                            assert chunk, "MCP closed before a complete response"
                            pending.extend(chunk)
                            assert len(pending) <= 2 * 1024 * 1024, "MCP response exceeded bound"
                        line, _, rest = pending.partition(b"\n")
                        pending[:] = rest
                        response = json.loads(line)
                        if type(response.get("id")) is int and response["id"] == identifier:
                            assert "error" not in response, "MCP request failed"
                            return response["result"]
                        assert response.get("id") is None, "Uncorrelated MCP response"
                    raise AssertionError("MCP notification budget exceeded")

                request(1, "initialize", {"protocolVersion": "2025-11-25", "capabilities": {},
                                           "clientInfo": {"name": "monitor-board-acceptance", "version": "1"}})
                write({"jsonrpc": "2.0", "method": "notifications/initialized"})
                result = request(2, "tools/call", {"name": "read_board", "arguments": {"board": name}})
                assert not result.get("isError"), "MCP board tool failed"
                observed = result["structuredContent"]["result"]
                assert observed["name"] == name and observed["revision"] == expected["revision"]
                assert observed["markdown"] == expected["markdown"], "MCP board source differs"
            finally:
                selector.close()
                child.stdin.close()
                try:
                    code = child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
                    raise AssertionError("MCP process did not exit after EOF")
                finally:
                    child.stdout.close()
                assert code == 0, "MCP process exited unsuccessfully"
        record("browser response source and revision are identical through actual CLI and persistent MCP read_board")

    try:
        a, b = start("a"), start("b")
        assert not rpc(a, "monitor_status")["enabled"]
        assert not rpc(a, "status")["network"]["enabled"]
        credentials = rpc(a, "monitor_enable", port=0)
        url, token = credentials["url"], credentials["access_token"]
        assert urlsplit(url).hostname == "127.0.0.1"
        assert len(token) >= 40
        assert token not in url and "?" not in url and "#" not in url
        assert "access_token" not in rpc(a, "monitor_status")
        assert not rpc(a, "status")["network"]["enabled"]
        record("monitor default-off and independent of peer networking; credentials stay out of URL/status")

        for path in ["/", "/style.css", "/app.js"]:
            status, headers, body = http(url, path)
            assert status == 200 and body
            assert headers.get("cache-control") == "no-store"
            assert headers.get("referrer-policy") == "no-referrer"
            assert headers.get("x-content-type-options") == "nosniff"
            assert "frame-ancestors 'none'" in headers.get("content-security-policy", "")
        for path in ["/api/status", "/api/messages", "/api/events"]:
            status, headers, _ = http(url, path)
            assert status in (401, 403)
            assert headers.get("cache-control") == "no-store"
            assert "access-control-allow-origin" not in headers
        assert http(url, "/api/messages?access_token=" + token)[0] in (400, 401, 403)
        for path in ["/", "/api/messages"]:
            for headers in [{"Host": "attacker.example"}, {"Origin": "https://attacker.example"},
                            {"Origin": "null"}, {"Sec-Fetch-Site": "cross-site"}]:
                assert http(url, path, token, headers)[0] in (400, 403)
        assert http(url, "/api/messages", token, {"Origin": url.rstrip("/")})[0] == 200
        for method in ["POST", "PUT", "PATCH", "DELETE"]:
            assert http(url, "/api/messages", token, method=method)[0] in (403, 405)
        for query in ["limit=101", "limit=0", "unknown=1", "since=no", "before=bad"]:
            assert http(url, "/api/messages?" + query, token)[0] == 400, query
        record("embedded assets and authenticated message API reject foreign Host/origin/site, URL secrets and invalid queries")

        # A request addressed to the machine's non-loopback IP cannot reach the listener.
        route = (subprocess.run(["ip", "-j", "route", "get", "192.0.2.1"],
                                capture_output=True, text=True) if shutil.which("ip") else None)
        if route is not None and route.returncode == 0:
            host_ip = json.loads(route.stdout)[0].get("prefsrc")
            if host_ip and not host_ip.startswith("127."):
                try:
                    probe = socket.create_connection((host_ip, urlsplit(url).port), timeout=1)
                except OSError:
                    record("non-loopback machine address cannot reach monitor listener")
                else:
                    probe.close()
                    raise AssertionError("Monitor reachable on non-loopback interface")

        rpc(a, "machine_set_name", name="monitor-a")
        rpc(b, "machine_set_name", name="monitor-b")
        for name in ["speaker", "listener"]:
            rpc(a, "register", agent={"name": name, "provider": "inbox", "session": name + "-original",
                                      "endpoint": "", "enabled": True}, replace=False)
        local_body = "LOCAL_MONITOR_OK\n<img src=x onerror=window.monitorPwned=1> & literal text"
        local_id = send(a, "listener", local_body, sender="speaker")
        wait("local submitted", lambda: rpc(a, "get", id=local_id)["state"] == "submitted")
        rpc(a, "ack", id=local_id, agent="listener")
        row = next(row for row in messages(url, token) if row["id"] == local_id)
        assert row["direction"] == "local"
        assert row["source"]["session"] == "speaker-original"
        assert row["destination"]["session"] == "listener-original"
        assert row["state"] == "acknowledged"
        assert row["source"]["machine_name"] == "monitor-a"
        rpc(a, "register", agent={"name": "listener", "provider": "inbox", "session": "listener-replaced",
                                   "endpoint": "", "enabled": True}, replace=True)
        row = api(url, token, "/api/messages/" + local_id)["message"]
        assert row["destination"]["session"] == "listener-original"
        assert row["body"] == local_body
        assert any(message["id"] == local_id for message in messages(url, token, agent="speaker"))
        assert not messages(url, token, agent="does-not-exist")
        assert any(message["id"] == local_id for message in messages(url, token, machine="monitor-a", state="acknowledged"))
        assert not messages(url, token, since=row["created"] + 1)
        assert not messages(url, token, until=row["created"] - 1)
        record("local history shows exact sessions/body/state and filters; replacement cannot rewrite historical session")

        # Owner approval is simulated only for private disposable inbox brokers.
        ids = [rpc(state, "identity")["peer_id"] for state in [a, b]]
        for state, other, alias in [(a, ids[1], "bob"), (b, ids[0], "alice")]:
            rpc(state, "peer_pair", peer_id=other, fingerprint=other, alias=alias)
            rpc(state, "peer_share", peer=alias, agent="user", shared=True)
            rpc(state, "network_configure", enabled=True, listen="/ip4/127.0.0.1/udp/0/quic-v1")
        addresses = [wait("peer listener", lambda state=state: rpc(state, "status")["network"].get("listening"))
                     for state in [a, b]]
        rpc(a, "peer_address", peer="bob", address=addresses[1][0])
        rpc(b, "peer_address", peer="alice", address=addresses[0][0])
        wait("remote directory", lambda: rpc(a, "remote_list"))
        outbound_id = send(a, "bob/user", "P2P_MONITOR_OUTBOUND")
        inbound_b = received(b, "P2P_MONITOR_OUTBOUND")
        rpc(b, "ack", id=inbound_b["id"], agent="user")
        wait("remote ACK", lambda: rpc(a, "get", id=outbound_id)["state"] == "acknowledged")
        reply_id = send(b, "alice/user", "P2P_MONITOR_REPLY", parent=inbound_b["id"])
        inbound_a = received(a, "P2P_MONITOR_REPLY")
        rpc(a, "ack", id=inbound_a["id"], agent="user")
        rows = messages(url, token)
        outgoing = next(message for message in rows if message["id"] == outbound_id)
        incoming = next(message for message in rows if message["id"] == inbound_a["id"])
        assert outgoing["direction"] == "outbound" and incoming["direction"] == "inbound"
        assert outgoing["source"]["peer_id"] == ids[0]
        assert outgoing["destination"]["peer_id"] == ids[1]
        assert incoming["source"]["session"] == "user" and incoming["destination"]["session"] == "user"
        assert incoming["parent"]["id"] == outbound_id
        assert incoming["parent"]["local_id"] == outbound_id
        detail = api(url, token, "/api/messages/" + outbound_id)
        assert any(message["id"] == inbound_a["id"] for message in detail["related"])
        record("real P2P inbox exchange and ACK/reply correlation show pinned machine/session provenance")

        # Streams contain only invalidation revisions, never messages or auth material.
        stream, response = opened_stream(url, token)
        original = next_event(response)
        changed_id = send(a, "user", "SSE_MONITOR_OK", sender="speaker")
        changed = next_event(response)
        assert original != changed
        assert set(changed) == {"revision"}
        assert "SSE_MONITOR_OK" not in json.dumps(changed) and token not in json.dumps(changed)
        response.close()
        stream.close()
        record("authenticated live stream reports changes without leaking message bodies")

        if args.browser:
            from playwright.sync_api import sync_playwright
            from monitor_board_qa import board_browser_checks
            with sync_playwright() as playwright, ExitStack() as cleanup:
                browser_home = output / "browser-home"
                browser_home.mkdir(mode=0o700)
                browser = playwright.chromium.launch(headless=True, env={
                    "HOME": str(browser_home), "TMPDIR": str(browser_home),
                    "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8"})
                cleanup.callback(browser.close)
                context = browser.new_context(viewport={"width": 1440, "height": 1000})
                cleanup.callback(context.close)
                page = context.new_page()
                external_requests, errors = [], []
                page.on("request", lambda request: external_requests.append(request.url)
                        if not request.url.startswith(url.rstrip("/")) else None)
                page.on("pageerror", lambda error: errors.append(str(error)))
                page.goto(url)
                page.locator("#access-token").fill(token)
                page.locator("#login-form button[type=submit]").click()
                page.locator("#workspace").wait_for(state="visible")
                page.get_by_text("LOCAL_MONITOR_OK", exact=False).first.wait_for()
                assert page.evaluate("typeof window.monitorPwned") == "undefined"
                assert page.evaluate("localStorage.length + sessionStorage.length") == 0
                assert token not in page.url
                live_id = send(a, "user", "BROWSER_LIVE_MONITOR_OK", sender="speaker")
                page.get_by_text("BROWSER_LIVE_MONITOR_OK", exact=False).first.wait_for(timeout=15000)
                page.locator(f'.message-row[data-id="{local_id}"]').click()
                page.locator("#detail-content pre.body-text").get_by_text(local_body, exact=True).wait_for()
                assert "listener-original" in page.locator("#detail-content").inner_text()
                assert "listener-replaced" not in page.locator("#detail-content").inner_text()
                page.locator(f'.message-row[data-id="{outbound_id}"]').click()
                page.locator("#detail-content pre.body-text").get_by_text("P2P_MONITOR_OUTBOUND", exact=True).wait_for()
                page.locator("#detail-content .relation").filter(has_text=inbound_a["id"]).click()
                page.locator("#detail-content pre.body-text").get_by_text("P2P_MONITOR_REPLY", exact=True).wait_for()
                page.locator("#detail-content .relation").filter(has_text=outbound_id).click()
                page.locator("#detail-content pre.body-text").get_by_text("P2P_MONITOR_OUTBOUND", exact=True).wait_for()
                page.locator("#agent-filter").fill("speaker")
                page.locator('#filters button[type="submit"]').click()
                wait("browser filters exclude remote row", lambda: page.locator(f'.message-row[data-id="{outbound_id}"]').count() == 0)
                assert page.locator(f'.message-row[data-id="{local_id}"]').count() == 1
                page.locator("#reset-filters").click()
                page.locator(f'.message-row[data-id="{outbound_id}"]').wait_for()
                page.locator(f'.message-row[data-id="{inbound_a["id"]}"]').click()
                page.locator("#detail-content pre.body-text").get_by_text("P2P_MONITOR_REPLY", exact=True).wait_for()
                page.screenshot(path=str(output / "monitor-desktop.png"), full_page=True)
                page.set_viewport_size({"width": 390, "height": 844})
                page.screenshot(path=str(output / "monitor-mobile.png"), full_page=True)
                board_browser_checks(page=page, browser=browser, context=context,
                                     rpc=rpc, state=a, second_state=b, url=url,
                                     token=token, record=record, output=output)
                board_readback(a, "alpha")
                page.locator("#lock-button").click()
                page.locator("#access-token").wait_for(state="visible")
                assert not page.locator("#workspace").is_visible()
                assert page.locator("#access-token").input_value() == ""
                assert not external_requests, external_requests
                assert not errors, errors
            record("real browser login/live update/full history/session pinning/reply navigation/filter/logout work safely with embedded assets and memory-only auth")

        stream, response = opened_stream(url, token)
        next_event(response)
        rpc(a, "monitor_disable")
        try:
            disconnected = next_event(response) is None
        except socket.timeout as error:
            raise AssertionError("Disabled monitor left a live idle stream") from error
        except (OSError, http_client.HTTPException):
            disconnected = True
        assert disconnected, "Disabled monitor left stream alive"
        response.close()
        stream.close()
        newer = rpc(a, "monitor_enable", port=0)
        assert newer["access_token"] != token
        assert http(newer["url"], "/api/messages", token)[0] in (401, 403)
        assert api(newer["url"], newer["access_token"], "/api/messages")
        rpc(a, "monitor_disable")
        assert rpc(a, "status")["network"]["enabled"]
        record("disable terminates live streams, rotates access and leaves peer networking enabled")
        for log in [output / "a.log", output / "b.log"]:
            content = log.read_text()
            assert token not in content and newer["access_token"] not in content
            assert local_body not in content and "P2P_MONITOR_OUTBOUND" not in content
        record("broker logs contain neither browser credentials nor message bodies")
        results["complete"] = True
        return 0
    finally:
        for process in processes:
            process.terminate()
        for process in processes:
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        for file in files:
            file.close()
        results.setdefault("complete", False)
        (output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
        print(f"Evidence: {output}", flush=True)


if __name__ == "__main__":
    raise SystemExit(main())
