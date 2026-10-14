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
import shlex
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
    # The real backend can send metadata before output; it must not end the head.
    {"type": "response.metadata", "metadata": {}},
    {"type": "response.failed", "response": {"id": "r1", "error": {
        "code": "server_is_overloaded",
        "message": "Selected model is at capacity. Please try a different model."}}},
]
AFTER_OUTPUT_OVERLOADED = [
    {"type": "response.created", "response": {"id": "r1"}},
    {"type": "response.reasoning_summary_text.delta", "delta": "synthetic reasoning"},
    {"type": "response.failed", "response": {"id": "r1", "error": {
        "code": "server_is_overloaded",
        "message": "Selected model is at capacity. Please try a different model."}}},
]
RATE = b'{"error":{"type":"rate_limit_exceeded","message":"synthetic"}}'


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def mock_upstream(script, marker=None):
    calls = []
    thread_ids = []
    requests = []

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers.get("content-length", 0)))))
            step = script.pop(0) if script else "ok"
            calls.append(step)
            # The relay keys failure streaks on this header (per conversation).
            thread_ids.append(self.headers.get("thread-id"))
            if step == "rate":
                self.send_response(429)
                body = RATE
                self.send_header("content-type", "application/json")
            else:
                self.send_response(200)
                events = (OVERLOADED if step == "overloaded" else
                          AFTER_OUTPUT_OVERLOADED if step == "after_output" else OK)
                if step in ("advised_visible_output", "visible_output"):
                    events = [
                        {"type": "response.created", "response": {"id": "r-partial"}},
                        {"type": "response.output_item.done", "item": {
                            "type": "message", "role": "assistant", "id": "m-partial",
                            "content": [{"type": "output_text", "text": "VISIBLE-ONCE"}]}},
                        {"type": "response.output_item.done", "item": {
                            "type": "function_call", "id": "fc1", "call_id": "call-once",
                            "name": "exec_command", "arguments": json.dumps({
                                "cmd": f"printf 'TOOL-ONCE\\n' >> {shlex.quote(str(marker))}"})}},
                        {"type": "response.failed", "response": {"id": "r-partial", "error": {
                            "code": "server_is_overloaded", "message": "Synthetic capacity failure."}}},
                    ]
                    if step == "advised_visible_output":
                        events[-1]["response"]["error"].update({
                            # Match the relay's compatibility advice: 0.160
                            # treats server_is_overloaded as terminal.
                            "code": "rate_limit_exceeded",
                            "message": "Synthetic capacity failure. Please try again in 1s.",
                            "headers": {"retry-after": "1", "x-should-retry": "true"}})
                body = "".join(f"event: {e['type']}\ndata: {json.dumps(e)}\n\n" for e in events).encode()
                self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, calls, thread_ids, requests


def scenario(args, home, script, overloaded_budget, expect_ok, expect_calls, direct=False, visible=False):
    marker = home / "tool-once.txt"
    marker.unlink(missing_ok=True)
    upstream, calls, thread_ids, requests = mock_upstream(list(script), marker)
    relay_port = free_port()
    relay = None if direct else subprocess.Popen(
        [str(args.codexctl), "relay", "serve", "--listen", f"127.0.0.1:{relay_port}",
         "--upstream", f"http://127.0.0.1:{upstream.server_port}",
         "--overloaded-budget-secs", str(overloaded_budget)],
        stderr=subprocess.PIPE, text=True,
        env={**os.environ, "HOME": str(home), "CODEX_HOME": str(home)})
    try:
        if relay:
            time.sleep(1)
        port = upstream.server_port if direct else relay_port
        provider = (f'{{name="relaytest",base_url="http://127.0.0.1:{port}/backend-api/codex",'
                    'wire_api="responses",requires_openai_auth=false,request_max_retries=0,stream_max_retries=12}')
        result = subprocess.run(
            [args.codex, "exec", "--json", "--dangerously-bypass-approvals-and-sandbox",
             "--skip-git-repo-check", "-m", "gpt-6.1-sol",
             "-c", 'model_provider="relaytest"', "-c", f"model_providers.relaytest={provider}", "say hi"],
            env={**os.environ, "HOME": str(home), "CODEX_HOME": str(home)}, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=120)
    finally:
        log = ""
        if relay:
            relay.terminate()
            log = relay.communicate(timeout=40)[1]
        upstream.shutdown()
    ok = "RELAY-E2E-OK" in result.stdout
    if not thread_ids or None in thread_ids or len(set(thread_ids)) != 1:
        raise SystemExit(f"FAIL {script}: Codex thread-id headers {thread_ids}")
    if any(request.get("model") != "gpt-6.1-sol" for request in requests):
        raise SystemExit(f"FAIL {script}: retry changed the model")
    if ok != expect_ok or calls != expect_calls:
        raise SystemExit(f"FAIL {script}: ok={ok} calls={calls}\n{result.stdout[-4000:]}\n{result.stderr[-800:]}\n{log}")
    if visible:
        messages = [json.loads(line) for line in result.stdout.splitlines() if line.startswith("{")]
        displayed = [event["item"].get("text") for event in messages
                     if event.get("type") == "item.completed" and event["item"].get("type") == "agent_message"]
        if displayed.count("VISIBLE-ONCE") != 1 or displayed.count("RELAY-E2E-OK") != 1:
            raise SystemExit(f"FAIL duplicated/lost visible output: {displayed}\n{result.stderr[-800:]}")
        if not marker.exists() or marker.read_text() != "TOOL-ONCE\n":
            raise SystemExit(f"FAIL completed tool execution: {result.stdout}\n{result.stderr[-800:]}")
        history = requests[-1].get("input", [])
        if not any(item.get("type") == "function_call_output" and item.get("call_id") == "call-once" for item in history):
            raise SystemExit(f"FAIL recovered request omitted completed tool result: {history}")
    outcomes = [json.loads(line)["outcome"] for line in log.splitlines() if line.startswith("{")]
    proof = " visible=1 tool=1 completed-tool-in-history=true" if visible else ""
    print(f"PASS {','.join(script)}: ok={ok} calls={len(calls)} relay={outcomes}{proof}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codexctl", type=pathlib.Path, required=True)
    parser.add_argument("--codex", default="codex")
    # Codex 0.160.1 treats every HTTP 429 as final; pass `stop` to expect that
    # and still check the capacity cases.
    parser.add_argument("--rate-429", choices=["retry", "stop"], default="retry")
    parser.add_argument("--advice-proof-only", action="store_true",
                        help="Prove advised after-output recovery directly against fake upstream before changing the relay")
    args = parser.parse_args()
    print(subprocess.run([args.codex, "--version"], capture_output=True, text=True).stdout.strip())
    with tempfile.TemporaryDirectory(prefix="codexctl-relay-e2e-") as home:
        home = pathlib.Path(home)
        if args.advice_proof_only:
            scenario(args, home, ["advised_visible_output", "ok"], 600, True,
                     ["advised_visible_output", "ok"], direct=True, visible=True)
            return
        if args.rate_429 == "retry":
            scenario(args, home, ["rate", "ok"], 600, True, ["rate", "ok"])
        else:
            scenario(args, home, ["rate", "ok"], 600, False, ["rate"])
        scenario(args, home, ["overloaded", "overloaded", "ok"], 600, True, ["overloaded", "overloaded", "ok"])
        scenario(args, home, ["after_output", "ok"], 600, True, ["after_output", "ok"])
        scenario(args, home, ["after_output", "after_output", "ok"], 600, True,
                 ["after_output", "after_output", "ok"])
        scenario(args, home, ["visible_output", "ok"], 600, True,
                 ["visible_output", "ok"], visible=True)
        # A 2 s budget fits the first advice (1 to 2 s) but never the second
        # (2 s or more after at least 1 s), so Codex stops after two calls.
        scenario(args, home, ["overloaded"] * 4, 2, False, ["overloaded"] * 2)
        scenario(args, home, ["after_output"] * 4, 2, False, ["after_output"] * 2)


if __name__ == "__main__":
    main()
