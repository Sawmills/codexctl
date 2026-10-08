#!/usr/bin/env python3
"""A real Codex binary through `codexctl relay` against a local mock upstream.

Run it once per Codex version that lanes use (0.161 reads the rate-limit message,
0.162.0-alpha.8 and later read error.headers). No network and no credentials.
"""
import argparse
import http.server
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import threading
import time

USAGE = {"input_tokens": 1, "input_tokens_details": None, "output_tokens": 1,
         "output_tokens_details": None, "total_tokens": 2}
OK = [
    {"type": "response.created", "response": {"id": "r1"}},
    {"type": "response.output_item.done", "item": {"type": "message", "role": "assistant", "id": "m1",
     "content": [{"type": "output_text", "text": "RELAY-E2E-OK"}]}},
    {"type": "response.completed", "response": {"id": "r1", "usage": USAGE}},
]
OVERLOADED = [
    {"type": "response.created", "response": {"id": "r1"}},
    {"type": "response.failed", "response": {"id": "r1", "error": {
        "code": "server_is_overloaded",
        "message": "Selected model is at capacity. Please try a different model."}}},
]
RATE = b'{"error":{"type":"rate_limit_exceeded","message":"synthetic"}}'


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def mock_upstream(script):
    calls = []

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            self.rfile.read(int(self.headers.get("content-length", 0)))
            step = script.pop(0) if script else "ok"
            calls.append(step)
            if step == "rate":
                self.send_response(429)
                body = RATE
                self.send_header("content-type", "application/json")
            else:
                self.send_response(200)
                events = OVERLOADED if step == "overloaded" else OK
                body = "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()
                self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, calls


def scenario(args, home, script, overloaded_budget, expect_ok, expect_calls):
    upstream, calls = mock_upstream(list(script))
    relay_port = free_port()
    relay = subprocess.Popen(
        [str(args.codexctl), "relay", "serve", "--listen", f"127.0.0.1:{relay_port}",
         "--upstream", f"http://127.0.0.1:{upstream.server_port}",
         "--overloaded-budget-secs", str(overloaded_budget)],
        stderr=subprocess.PIPE, text=True)
    try:
        time.sleep(1)
        provider = (f'{{name="relaytest",base_url="http://127.0.0.1:{relay_port}/backend-api/codex",'
                    'wire_api="responses",requires_openai_auth=false,request_max_retries=0,stream_max_retries=12}')
        result = subprocess.run(
            [args.codex, "exec", "--skip-git-repo-check", "-m", "gpt-6.1-sol",
             "-c", 'model_provider="relaytest"', "-c", f"model_providers.relaytest={provider}", "say hi"],
            env={**os.environ, "CODEX_HOME": str(home)}, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
    finally:
        relay.terminate()
        log = relay.communicate(timeout=40)[1]
        upstream.shutdown()
    ok = "RELAY-E2E-OK" in result.stdout
    if ok != expect_ok or calls != expect_calls:
        raise SystemExit(f"FAIL {script}: ok={ok} calls={calls}\n{result.stderr[-800:]}\n{log}")
    outcomes = [json.loads(line)["outcome"] for line in log.splitlines() if line.startswith("{")]
    print(f"PASS {','.join(script)}: ok={ok} calls={len(calls)} relay={outcomes}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codexctl", type=pathlib.Path, required=True)
    parser.add_argument("--codex", default="codex")
    args = parser.parse_args()
    print(subprocess.run([args.codex, "--version"], capture_output=True, text=True).stdout.strip())
    with tempfile.TemporaryDirectory(prefix="codexctl-relay-e2e-") as home:
        home = pathlib.Path(home)
        scenario(args, home, ["rate", "ok"], 600, True, ["rate", "ok"])
        scenario(args, home, ["overloaded", "overloaded", "ok"], 600, True, ["overloaded", "overloaded", "ok"])
        # A 2 s budget fits the first advice (1 to 2 s) but never the second
        # (2 s or more after at least 1 s), so Codex stops after two calls.
        scenario(args, home, ["overloaded"] * 4, 2, False, ["overloaded"] * 2)


if __name__ == "__main__":
    main()
