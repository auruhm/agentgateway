#!/usr/bin/env python3
"""Регрессии review P1: ping-before-init, divergent versions, modern->legacy без replay.

Только stdlib, изолированные loopback-порты. Binary передаётся явно; сборку скрипт не запускает.
"""
import argparse
import http.client
import http.server
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest

MODERN = "2026-07-28"
BINARY = None


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def terminal(raw):
    text = raw.decode()
    if not text:
        return None
    if text.startswith("{"):
        return json.loads(text)
    messages = [json.loads(line[5:]) for line in text.splitlines() if line.startswith("data:")]
    return messages[-1]


class State:
    def __init__(self, version, events_file, ping=False, deny_discovery=False, call_error=False):
        self.version, self.events_file = version, events_file
        self.ping, self.deny_discovery, self.call_error = ping, deny_discovery, call_error
        self.initialized = False
        self.pending = None
        self.events = queue.Queue()
        self.pong = threading.Event()
        self.errors = []

    def record(self, value):
        with self.events_file.open("a") as output:
            output.write(json.dumps(value) + "\n")

    def process(self, body, headers=None):
        method = body.get("method", "pong")
        self.record(method)
        if method == "pong":
            if body.get("id") != "init-ping" or body.get("result") != {}:
                self.errors.append("incorrect pong id/result")
            self.pong.set()
            return None
        result = None
        if method == "server/discover":
            if self.version != MODERN:
                return {"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32601, "message": "legacy only"}}
            result = {"resultType": "complete", "supportedVersions": [MODERN], "capabilities": {"tools": {}},
                      "serverInfo": {"name": "strict-modern", "version": "1"}, "ttlMs": 0, "cacheScope": "private"}
        elif method == "initialize":
            self.pong.clear()
            result = {"protocolVersion": self.version, "capabilities": {"tools": {}},
                      "serverInfo": {"name": "strict-legacy", "version": "1"}}
        elif method == "notifications/initialized":
            self.initialized = True
            return None
        else:
            meta = body.get("params", {}).get("_meta", {})
            if headers is not None and headers.get("mcp-protocol-version") != self.version:
                self.errors.append("wrong negotiated backend HTTP version")
            if self.version == MODERN:
                if meta.get("io.modelcontextprotocol/protocolVersion") != MODERN:
                    self.errors.append("missing/wrong modern envelope")
                if "io.modelcontextprotocol/clientCapabilities" not in meta:
                    self.errors.append("missing clientCapabilities")
            else:
                if not self.initialized:
                    self.errors.append("semantic request before initialized")
                if any(key.startswith("io.modelcontextprotocol/") for key in meta):
                    self.errors.append("modern envelope leaked to legacy")
                if headers is not None and headers.get("Mcp-Session-Id") != "strict-session":
                    self.errors.append("legacy session id lost")
            if method == "tools/list":
                result = {"tools": [{"name": "echo", "inputSchema": {"type": "object"}}]}
            elif method == "tools/call":
                if self.call_error:
                    return {"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32603, "message": "call already had side effect"}}
                result = {"content": [{"type": "text", "text": "exactly once"}]}
            else:
                self.errors.append("unexpected backend method " + method)
                return {"jsonrpc": "2.0", "id": body["id"], "error": {"code": -32601, "message": method}}
        return {"jsonrpc": "2.0", "id": body["id"], "result": result}


def stdio_backend(events_file, version):
    state = State(version, Path(events_file), ping=True)
    state.record("spawn")
    for line in sys.stdin:
        body = json.loads(line)
        result = state.process(body)
        if body.get("method") == "initialize":
            state.pending = result
            result = {"jsonrpc": "2.0", "id": "init-ping", "method": "ping"}
        elif "method" not in body and state.pending is not None:
            result, state.pending = state.pending, None
        if result is not None:
            print(json.dumps(result), flush=True)
        if state.errors:
            state.record({"violations": state.errors})
            return 1
    return 0


class Fixture:
    def __init__(self, directory, transport, backend_version, client_version, ping=False,
                 stateful=True, deny_discovery=False, call_error=False):
        self.directory, self.transport, self.client_version = Path(directory), transport, client_version
        self.session = None
        self.events_file = self.directory / "events.jsonl"
        self.state = State(backend_version, self.events_file, ping, deny_discovery, call_error)
        self.process, self.server, self.thread = None, None, None
        state = self.state

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def reply(self, status, body=None, session=False):
                data = json.dumps(body).encode() if body is not None else b""
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                if session:
                    self.send_header("Mcp-Session-Id", "strict-session")
                self.end_headers()
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def chunk(self, data):
                data = data.encode()
                self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
                self.wfile.flush()

            def start_stream(self, session=False):
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Transfer-Encoding", "chunked")
                if session:
                    self.send_header("Mcp-Session-Id", "strict-session")
                self.end_headers()

            def do_GET(self):
                state.record("GET")
                self.start_stream()
                self.chunk("event: endpoint\ndata: /messages\n\n")
                try:
                    while True:
                        message = state.events.get(timeout=15)
                        if message is None:
                            break
                        self.chunk("data: " + json.dumps(message) + "\n\n")
                    self.wfile.write(b"0\r\n\r\n")
                except (OSError, queue.Empty):
                    pass

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                if body.get("method") == "server/discover" and state.deny_discovery:
                    state.record("server/discover")
                    self.reply(403)
                    return
                result = state.process(body, self.headers if transport == "http" else None)
                if transport == "sse":
                    if body.get("method") == "initialize" and state.ping:
                        state.pending = result
                        result = {"jsonrpc": "2.0", "id": "init-ping", "method": "ping"}
                    elif "method" not in body and state.pending is not None:
                        result, state.pending = state.pending, None
                    self.reply(202)
                    if result is not None:
                        state.events.put(result)
                    return
                if body.get("method") == "initialize" and state.ping:
                    self.start_stream(session=True)
                    self.chunk('data: {"jsonrpc":"2.0","id":"init-ping","method":"ping"}\n\n')
                    if not state.pong.wait(3):
                        state.errors.append("backend never received pong before initialize result")
                    try:
                        self.chunk("data: " + json.dumps(result) + "\n\n")
                        self.wfile.write(b"0\r\n\r\n")
                    except OSError:
                        pass
                else:
                    self.reply(200 if result is not None else 202, result, body.get("method") == "initialize")

            def do_DELETE(self):
                state.record("DELETE")
                self.reply(202)

        self.port = free_port()
        admin, stats, ready = [free_port() for _ in range(3)]
        if transport == "stdio":
            target = {"name": "strict", "stdio": {"cmd": sys.executable,
                       "args": [str(Path(__file__).resolve()), "--stdio-backend", str(self.events_file), backend_version]}}
        else:
            self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            self.server.daemon_threads = True
            self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
            self.thread.start()
            target = {"name": "strict", "sse" if transport == "sse" else "mcp": {
                "host": "127.0.0.1", "port": self.server.server_port, "path": "/sse" if transport == "sse" else "/mcp"}}
        config = {"config": {"adminAddr": f"127.0.0.1:{admin}", "statsAddr": f"127.0.0.1:{stats}", "readinessAddr": f"127.0.0.1:{ready}"},
                  "binds": [{"port": self.port, "listeners": [{"routes": [{"backends": [{"mcp": {
                      "lazyTools": True, "statefulMode": "stateful" if stateful else "stateless", "targets": [target]
                  }}]}]}]}]}
        path = self.directory / "config.json"
        path.write_text(json.dumps(config))
        self.log = (self.directory / "gateway.log").open("w+")
        env = os.environ.copy()
        env.update(ADMIN_ADDR=f"127.0.0.1:{admin}", STATS_ADDR=f"127.0.0.1:{stats}", READINESS_ADDR=f"127.0.0.1:{ready}")
        self.command, self.env = [str(BINARY), "-f", str(path)], env

    def __enter__(self):
        self.process = subprocess.Popen(self.command, stdout=self.log, stderr=self.log, env=self.env)
        try:
            for _ in range(300):
                if self.process.poll() is not None:
                    self.log.seek(0)
                    raise AssertionError(self.log.read())
                try:
                    with socket.create_connection(("127.0.0.1", self.port), timeout=.1):
                        return self
                except OSError:
                    time.sleep(.05)
            raise AssertionError("gateway did not start")
        except BaseException:
            self.__exit__(*sys.exc_info())
            raise

    def __exit__(self, *_):
        if self.process is not None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.state.events.put(None)
        if self.server is not None:
            self.server.shutdown()
            self.server.server_close()
            self.thread.join(timeout=2)
        self.log.close()

    def request(self, method, params=None, allow_error=False, notification=False):
        body = {"jsonrpc": "2.0", "method": method}
        if not notification:
            body["id"] = 1
        if params is not None:
            body["params"] = params
        if self.client_version == MODERN:
            body.setdefault("params", {})["_meta"] = {
                "io.modelcontextprotocol/protocolVersion": MODERN,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {"name": "review-regression", "version": "1"}}
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream", "mcp-protocol-version": self.client_version}
        if self.client_version == MODERN:
            headers["mcp-method"] = method
            if method == "tools/call":
                headers["mcp-name"] = params["name"]
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=2)
        try:
            connection.request("POST", "/mcp", json.dumps(body), headers)
            response = connection.getresponse()
            self.session = response.getheader("Mcp-Session-Id") or self.session
            data = response.read()
            message = terminal(data)
            if allow_error and message is None and response.status >= 400:
                message = {"error": {"httpStatus": response.status}}
            if not allow_error:
                if notification:
                    assert response.status == 202, (response.status, message)
                else:
                    assert response.status == 200 and "error" not in message, (response.status, message)
            return message
        finally:
            connection.close()

    def methods(self):
        return [json.loads(line) for line in self.events_file.read_text().splitlines()] if self.events_file.exists() else []

    def bootstrap(self):
        start = time.monotonic()
        if self.client_version == MODERN:
            self.request("server/discover")
        else:
            self.request("initialize", {"protocolVersion": self.client_version, "capabilities": {}, "clientInfo": {"name": "review", "version": "1"}})
            self.request("notifications/initialized", notification=True)
        self.request("tools/list")
        assert time.monotonic() - start < .5, "bootstrap was not local/fast"
        assert self.methods() == [], self.methods()


class NegotiationTests(unittest.TestCase):
    def exercise(self, transport="http", backend="2025-03-26", client="2025-06-18", ping=False,
                 stateful=True, deny=False, call_error=False):
        with tempfile.TemporaryDirectory(prefix="lazy-review-") as directory:
            with Fixture(directory, transport, backend, client, ping, stateful, deny, call_error) as fixture:
                fixture.bootstrap()
                message = fixture.request("tools/call", {"name": "mcp_call", "arguments": {"name": "echo", "arguments": {}}}, allow_error=deny or call_error)
                methods = fixture.methods()
                self.assertFalse(fixture.state.errors, fixture.state.errors)
                self.assertFalse(any(isinstance(item, dict) for item in methods), methods)
                if deny:
                    self.assertIn("error", message)
                    self.assertEqual(methods, ["server/discover"])
                else:
                    if call_error:
                        self.assertIn("error", message)
                    else:
                        self.assertIn("exactly once", json.dumps(message))
                        if client == MODERN:
                            self.assertEqual(message["result"]["resultType"], "complete")
                        else:
                            self.assertNotIn("resultType", message["result"])
                    self.assertEqual(methods.count("tools/call"), 1, methods)
                    self.assertNotIn("tools/list", methods, "call-first must not discover catalog")
                    if backend != MODERN:
                        self.assertLess(methods.index("initialize"), methods.index("notifications/initialized"))
                        self.assertLess(methods.index("notifications/initialized"), methods.index("tools/call"))
                    if ping:
                        self.assertLess(methods.index("initialize"), methods.index("pong"))
                        self.assertLess(methods.index("pong"), methods.index("notifications/initialized"))

    def test_ping_http(self):
        self.exercise(ping=True)

    def test_ping_sse(self):
        self.exercise(transport="sse", ping=True)

    def test_ping_stdio(self):
        self.exercise(transport="stdio", ping=True)

    def test_divergent_legacy_versions(self):
        self.exercise()

    def test_divergent_legacy_versions_stateless(self):
        self.exercise(stateful=False)

    def test_stateless_search_with_deferred_handshake_on_default_stack(self):
        with tempfile.TemporaryDirectory(prefix="lazy-stack-") as directory:
            fixture = Fixture(directory, "http", "2025-03-26", "2025-06-18", stateful=False)
            fixture.env["RUST_MIN_STACK"] = "2097152"
            with fixture:
                fixture.bootstrap()
                result = fixture.request("tools/call", {"name": "mcp_search", "arguments": {"query": "echo"}})
                self.assertIn("echo", json.dumps(result))
                self.assertFalse(fixture.state.errors, fixture.state.errors)
                methods = [method for method in fixture.methods() if method != "DELETE"]
                self.assertEqual(methods, ["initialize", "notifications/initialized", "tools/list"])

    def test_modern_to_legacy_http(self):
        self.exercise(client=MODERN, ping=True)

    def test_modern_to_legacy_sse(self):
        self.exercise(transport="sse", client=MODERN, ping=True)

    def test_modern_to_legacy_stdio(self):
        self.exercise(transport="stdio", client=MODERN, ping=True)

    def test_modern_native(self):
        self.exercise(client=MODERN, backend=MODERN)

    def test_discovery_auth_denial_must_not_fallback(self):
        self.exercise(client=MODERN, deny=True)

    def test_side_effecting_call_error_must_not_replay(self):
        self.exercise(client=MODERN, call_error=True)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--stdio-backend":
        sys.exit(stdio_backend(sys.argv[2], sys.argv[3]))
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("tests", nargs="*")
    args = parser.parse_args()
    BINARY = args.binary.resolve(strict=True)
    unittest.main(argv=[sys.argv[0]] + args.tests, verbosity=2)
