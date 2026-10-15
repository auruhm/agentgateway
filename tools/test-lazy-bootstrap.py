#!/usr/bin/env python3
"""Изолированная MCP-проверка candidate binary; только stdlib и случайные loopback-порты."""
import argparse

import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def decode(data):
    text = data.decode()
    if text.startswith("{"):
        return json.loads(text)
    events = [json.loads(line[5:].strip()) for line in text.splitlines() if line.startswith("data:")]
    return events[-1] if events else None


def run_case(binary, stateful, version, call_first=False):
    modern = version == "2026-07-28"
    methods = []
    violations = []
    slow = threading.Event()
    slow.set()
    initialized = threading.Event()

    class Backend(http.server.BaseHTTPRequestHandler):
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
            self.wfile.write(data)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            method = body["method"]
            methods.append(method)
            if slow.is_set():
                violations.append("backend contacted during local bootstrap")
                time.sleep(3)
            if method == "server/discover" and modern:
                result = {"resultType": "complete", "supportedVersions": [version], "capabilities": {"tools": {}},
                          "serverInfo": {"name": "strict", "version": "1"}, "ttlMs": 0, "cacheScope": "private"}
            elif method == "initialize":
                result = {"protocolVersion": version, "capabilities": {"tools": {}},
                          "serverInfo": {"name": "strict", "version": "1"}}
            elif method == "notifications/initialized":
                initialized.set()
                self.reply(202)
                return
            else:
                if not modern and (not initialized.is_set() or self.headers.get("Mcp-Session-Id") != "strict-session"):
                    violations.append("request before handshake/session propagation")
                if modern and self.headers.get("mcp-protocol-version") != version:
                    violations.append("modern protocol header missing")
                if method == "tools/list":
                    result = {"tools": [{"name": "echo", "description": "Echo input",
                                         "inputSchema": {"type": "object"}}]}
                elif method == "tools/call":
                    result = {"content": [{"type": "text", "text": "called once"}]}
                else:
                    violations.append(f"unexpected method {method}")
                    self.reply(400)
                    return
            self.reply(200, {"jsonrpc": "2.0", "id": body["id"], "result": result}, method == "initialize")

        def do_DELETE(self):
            self.reply(202)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Backend)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    port, admin, stats, readiness = [free_port() for _ in range(4)]
    config = {"config": {"adminAddr": f"127.0.0.1:{admin}", "statsAddr": f"127.0.0.1:{stats}",
                         "readinessAddr": f"127.0.0.1:{readiness}"},
              "binds": [{"port": port, "listeners": [{"routes": [{"backends": [{"mcp": {
                  "statefulMode": "stateful" if stateful else "stateless", "lazyTools": True,
                  "targets": [{"name": "strict", "mcp": {"host": "127.0.0.1", "port": server.server_port, "path": "/mcp"}}]
              }}]}]}]}]}
    process = None
    timings = {}
    try:
        with tempfile.TemporaryDirectory(prefix="lazy-mcp-") as directory:
            path = Path(directory) / "config.json"
            path.write_text(json.dumps(config))
            with (Path(directory) / "gateway.log").open("w+") as log:
                env = os.environ.copy()
                # Фиксируем обычный 2-МиБ Tokio worker stack: большой inherited stack скрывает
                # регрессию inline stateless -> search -> deferred handshake polling.
                env.update(ADMIN_ADDR=f"127.0.0.1:{admin}", STATS_ADDR=f"127.0.0.1:{stats}",
                           READINESS_ADDR=f"127.0.0.1:{readiness}", RUST_MIN_STACK="2097152")
                process = subprocess.Popen([str(binary), "-f", str(path)], stdout=log, stderr=log, env=env)
                try:
                    for _ in range(300):
                        if process.poll() is not None:
                            log.seek(0)
                            raise AssertionError(log.read())
                        try:
                            with socket.create_connection(("127.0.0.1", port), timeout=.1):
                                break
                        except OSError:
                            time.sleep(.05)
                    else:
                        raise AssertionError("gateway listener did not start")
                    session = None

                    def request(method, params=None, request_id=1):
                        nonlocal session
                        body = {"jsonrpc": "2.0", "method": method}
                        if request_id is not None:
                            body["id"] = request_id
                        if params is not None:
                            body["params"] = params
                        if modern and request_id is not None:
                            body.setdefault("params", {})["_meta"] = {
                                "io.modelcontextprotocol/protocolVersion": version,
                                "io.modelcontextprotocol/clientInfo": {"name": "integration", "version": "1"},
                                "io.modelcontextprotocol/clientCapabilities": {}}
                        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
                                   "mcp-protocol-version": version}
                        if modern:
                            headers["mcp-method"] = method
                            if method == "tools/call":
                                headers["mcp-name"] = params["name"]
                        if session:
                            headers["Mcp-Session-Id"] = session
                        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
                        start = time.monotonic()
                        try:
                            connection.request("POST", "/mcp", json.dumps(body), headers)
                            response = connection.getresponse()
                            session = response.getheader("Mcp-Session-Id") or session
                            raw = response.read()
                            timings[method] = round((time.monotonic() - start) * 1000, 2)
                            assert response.status in (200, 202), (method, response.status, raw)
                            result = decode(raw) if raw else None
                            assert result is None or "error" not in result, result
                            return result
                        finally:
                            connection.close()

                    if not modern:
                        result = request("initialize", {"protocolVersion": version, "capabilities": {},
                                                       "clientInfo": {"name": "integration", "version": "1"}})
                        assert result["result"]["protocolVersion"] == version, result
                        assert result["result"]["capabilities"] == {"tools": {}}, result
                        request("notifications/initialized", request_id=None)
                    else:
                        request("server/discover")
                    result = request("tools/list")
                    assert [tool["name"] for tool in result["result"]["tools"]] == ["mcp_search", "mcp_call"], result
                    if not modern:
                        request("ping")
                    assert methods == [], methods
                    assert max(timings.values()) < 500, timings
                    slow.clear()
                    if not call_first:
                        result = request("tools/call", {"name": "mcp_search", "arguments": {"query": "echo"}})
                        assert "echo" in json.dumps(result), result
                    result = request("tools/call", {"name": "mcp_call", "arguments": {"name": "echo", "arguments": {}}})
                    assert "called once" in json.dumps(result), result
                    # В stateless legacy каждый реальный HTTP-запрос имеет собственный handshake.
                    expected = (["server/discover", "tools/list", "server/discover", "tools/call"] if modern else
                                ["initialize", "notifications/initialized", "tools/list"] +
                                ([] if stateful else ["initialize", "notifications/initialized"]) + ["tools/call"])
                    if call_first:
                        expected = ["server/discover", "tools/call"] if modern else ["initialize", "notifications/initialized", "tools/call"]
                    assert methods == expected, (methods, expected)
                    assert not violations, violations
                    sample = subprocess.check_output(["ps", "-p", str(process.pid), "-o", "pid=,%cpu=,rss="], text=True).strip()
                    print(json.dumps({"stateful": stateful, "version": version, "call_first": call_first, "timings_ms": timings,
                                      "backend_methods": methods, "ps_pid_cpu_rss_kib": sample}), flush=True)
                    if session:
                        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
                        connection.request("DELETE", "/mcp", headers={"Mcp-Session-Id": session, "mcp-protocol-version": version})
                        connection.getresponse().read()
                        connection.close()
                except BaseException:
                    print(json.dumps({"failed_case": {"stateful": stateful, "version": version,
                                      "call_first": call_first}, "backend_methods": methods,
                                      "violations": violations}), flush=True)
                    log.flush()
                    log.seek(0)
                    print(log.read()[-8000:])
                    raise
                finally:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    assert process is not None and process.poll() is not None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    for stateful, version in [(True, "2025-06-18"), (False, "2025-06-18"), (False, "2026-07-28"), (True, "2026-07-28")]:
        for call_first in (False, True):
            run_case(binary, stateful, version, call_first)
    print("PASS: local bootstrap, ZERO early requests, ordered deferred handshake, no call replay; all gateway processes stopped")


if __name__ == "__main__":
    main()
