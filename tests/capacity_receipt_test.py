"""Public wrapper -> relay regression with synthetic HTTP and Codex events."""
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest
import urllib.request

THREAD = "11111111-2222-4333-8444-555555555555"
MESSAGE = "Selected model is at capacity. Please try a different model."
CODEX = r'''#!/usr/bin/python3
import json, os, urllib.error, urllib.request, sys
from pathlib import Path
settings = json.loads((Path.home() / "fixture.json").read_text())
print(json.dumps({"type":"thread.started", "thread_id":settings["thread"]}), flush=True)
print('{"type":"turn.started"}', flush=True)
for _ in range(settings["attempts"]):
    request = urllib.request.Request(settings["url"], data=b'{"model":"gpt-6.1-sol"}', headers={"thread-id":settings["thread"]})
    try:
        urllib.request.urlopen(request).read()
    except urllib.error.HTTPError as error:
        error.read()
if settings.get("unrelated_success"):
    urllib.request.urlopen(urllib.request.Request(settings["url"], data=b'{}', headers={"thread-id":settings["thread"]})).read()
if settings["recover"]:
    print(json.dumps({"type":"item.completed", "item":{"type":"agent_message","text":settings["message"]}}), flush=True)
    print('{"type":"turn.completed"}', flush=True)
elif settings.get("quoted"):
    print(json.dumps({"type":"item.completed", "item":{"type":"agent_message","text":"■ " + settings["message"]}}, ensure_ascii=False), flush=True)
else:
    print(json.dumps({"type":"error", "message":settings["message"]}), flush=True)
    event = json.dumps({"type":"turn.failed", "error":{"message":settings["message"]}}) + "\n"
    # Real PTY reads can split a JSON record or contain duplicate final events.
    for part in [event[:20], event[20:], event]:
        sys.stdout.write(part); sys.stdout.flush()
sys.exit(0 if settings["recover"] else 1)
'''


class CapacityReceiptTest(unittest.TestCase):
    def scenario(self, recover=False, streamed=False, quoted=False, code="server_is_overloaded", unrelated_success=False, stream_after_http=False):
        calls = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers.get("content-length", 0)))
                calls.append(self.path)
                success = (recover and len(calls) > 1) or (unrelated_success and len(calls) > 3)
                body = {"error": {"code": code, "message": MESSAGE}}
                if (streamed and (not stream_after_http or len(calls) > 1)) or success:
                    body = ("event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\"}}\n\n" if success else
                            "event: response.failed\ndata: " + json.dumps({"type": "response.failed", "response": body}) + "\n\n").encode()
                    self.send_response(200)
                    self.send_header("content-type", "text/event-stream")
                else:
                    body = json.dumps(body).encode()
                    self.send_response(503)
                    self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_):
                pass

        upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary).resolve()
            scripts = home / "bin"
            scripts.mkdir()
            (scripts / "codex").write_text(CODEX)
            (scripts / "codex").chmod(0o755)
            environment = {"HOME": str(home), "CODEX_HOME": str(home), "PATH": str(scripts) + ":/usr/bin:/bin", "TERM": "dumb"}
            binary = os.environ["CODEXCTL_TEST_BINARY"]
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            address = f"127.0.0.1:{port}"
            relay = subprocess.Popen([binary, "relay", "serve", "--listen", address, "--upstream", f"http://127.0.0.1:{upstream.server_port}", "--overloaded-budget-secs", "0"], env=environment, cwd=home, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            try:
                for _ in range(100):
                    self.assertIsNone(relay.poll())
                    try:
                        urllib.request.urlopen(f"http://{address}/metrics", timeout=1).read()
                        break
                    except OSError:
                        time.sleep(.02)
                (home / ".codexctl" / "relay.json").write_text(json.dumps({"listen": address}))
                attempts = 2 if recover or stream_after_http else 1 if streamed else 3
                (home / "fixture.json").write_text(json.dumps({"url": f"http://{address}/backend-api/codex/responses", "thread": THREAD, "message": MESSAGE, "recover": recover, "attempts": attempts, "quoted": quoted, "unrelated_success": unrelated_success}))
                result = subprocess.run([binary, "codex", "exec", "--json", "say hi"], env=environment, cwd=home, stdin=subprocess.DEVNULL, capture_output=True, timeout=10)
                self.assertEqual(result.returncode, 0 if recover else 1, (result.stdout, result.stderr))
                metrics = urllib.request.urlopen(f"http://{address}/metrics", timeout=2).read().decode()
                expected = 0 if recover or quoted or code != "server_is_overloaded" else 1
                self.assertIn(f'codexctl_relay_lane_stops_total{{reason="capacity"}} {expected}\n', metrics)
                self.assertEqual(len(calls), attempts + int(unrelated_success))
                for body, headers, expected_status in [
                    (b'{"thread_id":"' + THREAD.encode() + b'"}', {}, 401),
                    (b'{}', {"x-codexctl-receipt": "invalid"}, 401),
                    (b'x' * 513, {}, 413),
                ]:
                    with self.assertRaises(urllib.error.HTTPError) as error:
                        urllib.request.urlopen(urllib.request.Request(f"http://{address}/v1/terminal-capacity", data=body, headers=headers))
                    self.assertEqual(error.exception.code, expected_status)
            finally:
                relay.terminate()
                relay.communicate(timeout=10)
                upstream.shutdown()
                upstream.server_close()

    def test_exhausted_http_capacity_is_counted_once_after_client_confirmation(self):
        self.scenario()

    def test_unrelated_same_thread_success_without_turn_metadata_cannot_erase_a_stop(self):
        self.scenario(unrelated_success=True)

    def test_recovered_http_capacity_is_not_a_stop_even_if_assistant_quotes_it(self):
        self.scenario(recover=True)

    def test_streamed_capacity_already_counted_by_relay_is_not_counted_twice(self):
        self.scenario(streamed=True)

    def test_http_retry_followed_by_counted_stream_capacity_is_not_counted_twice(self):
        self.scenario(streamed=True, stream_after_http=True)

    def test_marked_capacity_in_assistant_json_is_not_confirmation(self):
        self.scenario(quoted=True)

    def test_capacity_message_without_a_classified_upstream_failure_is_not_counted(self):
        self.scenario(code="internal_error")


if __name__ == "__main__":
    unittest.main()
