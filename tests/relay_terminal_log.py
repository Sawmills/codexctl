#!/usr/bin/env python3
"""Isolated public CLI/PTY regression; synthetic loopback service, no provider."""
import argparse
import base64
import datetime
import errno
import http.server
import json
import os
from pathlib import Path
import pty
import select
import socket
import stat
import subprocess
import tempfile
import threading
import time

EXACT = b"terminal_capacity_report_failed reason=native_completion_unavailable"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("--log-state", choices=("absent", "permissive", "blocked"), default="absent")
    args = parser.parse_args()
    calls = []
    encoded = base64.urlsafe_b64encode(json.dumps({
        "sub": "fixture-login", "exp": 2000000000,
        "https://api.openai.com/auth": {"chatgpt_account_id": "fixture-seat"},
    }).encode()).decode().rstrip("=")
    synthetic_auth = "eyJhbGciOiJub25lIn0." + encoded + ".synthetic"
    account = dict(userId="fixture", alias="lane", accountId="fixture-seat",
                   billingClass="rate_limited", available=True)

    class API(http.server.BaseHTTPRequestHandler):
        def handle_request(self):
            calls.append(self.command + " " + self.path)
            length = int(self.headers.get("Content-Length", "0"))
            if length:
                payload = json.loads(self.rfile.read(length))
                assert not payload.get("previousRevision"), "fixture must never refresh"
            if self.path == "/v1/me":
                value = {"id": "fixture"}
            elif self.path == "/v1/accounts":
                value = [account]
            elif self.path == "/v1/accounts/renamed":
                value = []
            elif self.path == "/v1/token":
                value = dict(userId="fixture", accessToken=synthetic_auth,
                             chatgptAccountId="fixture-seat", revision="fixture-v1",
                             billingClass="rate_limited", nativeRoutingSupported=True)
            else:
                self.send_error(404)
                return
            body = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        do_GET = handle_request
        do_POST = handle_request

        def log_message(self, *_):
            pass

    with tempfile.TemporaryDirectory(prefix="fixture-", dir=Path(__file__).parent) as directory:
        home = Path(directory)
        (home / "bin").mkdir()
        fake = home / "bin/codex"
        fake.write_text('''#!/usr/bin/python3
import os, pathlib, sys, time
if "--version" in sys.argv:
    print("codex-cli 0.163.0")
    raise SystemExit(0)
assert os.isatty(1), "child must own a PTY"
identity = pathlib.Path.home() / ".codexctl/relay/identity"
identity.rename(identity.with_name("identity-unavailable"))
print("SYNTHETIC-CHILD-PTY", flush=True)
time.sleep(1)
raise SystemExit(17)
''')
        fake.chmod(0o700)
        env = dict(HOME=str(home), PATH=str(home / "bin") + ":/usr/bin:/bin",
                   TERM="xterm-256color", CODEXCTL_ALLOW_INSECURE_LOOPBACK="1")
        with socket.socket() as port:
            port.bind(("127.0.0.1", 0))
            address = "127.0.0.1:" + str(port.getsockname()[1])
        relay = subprocess.Popen([args.binary, "relay", "serve", "--listen", address,
                                  "--upstream", "http://127.0.0.1:9"],
                                 env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        try:
            deadline = time.monotonic() + 5
            while True:
                ready = subprocess.run([args.binary, "relay", "status", "--listen", address],
                                       env=env, capture_output=True, timeout=2)
                if ready.returncode == 0:
                    break
                assert relay.poll() is None and time.monotonic() < deadline, "relay startup failed"
                time.sleep(0.02)
            enabled = subprocess.run([args.binary, "relay", "enable", "--listen", address],
                                     env=env, capture_output=True, timeout=5)
            assert enabled.returncode == 0, "relay enable failed"
            log = home / ".codexctl/terminal-capacity.jsonl"
            if args.log_state == "permissive":
                log.touch()
                log.chmod(0o644)
            elif args.log_state == "blocked":
                log.mkdir()
            with http.server.ThreadingHTTPServer(("127.0.0.1", 0), API) as api:
                worker = threading.Thread(target=api.serve_forever, daemon=True)
                worker.start()
                env.update(CODEXCTL_SERVER_URL=f"http://127.0.0.1:{api.server_port}",
                           CODEXCTL_SERVICE_TOKEN="synthetic-no-authority")
                master, slave = pty.openpty()
                child = subprocess.Popen([args.binary, "codex", "--account", "lane",
                                          "exec", "--json", "synthetic prompt"],
                                         env=env, stdin=slave, stdout=slave, stderr=slave)
                os.close(slave)
                output = bytearray()
                deadline = time.monotonic() + 12
                try:
                    while time.monotonic() < deadline:
                        if select.select([master], [], [], 0.1)[0]:
                            try:
                                chunk = os.read(master, 65536)
                            except OSError as error:
                                if error.errno == errno.EIO:
                                    break
                                raise
                            if not chunk:
                                break
                            output.extend(chunk)
                    if child.poll() is None:
                        child.kill()
                    code = child.wait(timeout=3)
                finally:
                    os.close(master)
                    api.shutdown()
                reached = b"SYNTHETIC-CHILD-PTY" in output
                rows = [json.loads(line) for line in log.read_text().splitlines()] if log.is_file() else []
                logged = any(row.get("reason") == "native_completion_unavailable" for row in rows)
                print(json.dumps(dict(exit=code, child_pty_reached=reached,
                                      exact_diagnostic_on_terminal=EXACT in output,
                                      diagnostic_in_private_log=logged, calls=calls)))
                if not reached:
                    print("SETUP FAILURE:", bytes(output).decode(errors="replace")[-1600:])
                assert reached and code == 17, "did not reach the real failed-report path"
                assert b"terminal_capacity_report_failed" not in output, "capacity diagnostic reached child's terminal"
                if args.log_state == "blocked":
                    assert not logged, "blocked log unexpectedly writable"
                else:
                    assert logged, "capacity-report reason missing from private log"
                    assert stat.S_IMODE(log.stat().st_mode) == 0o600, "log must be private"
                    for row in rows:
                        assert set(row) == {"at", "reason"}, "unexpected diagnostic fields"
                        datetime.datetime.fromisoformat(row["at"])
        finally:
            relay.terminate()
            relay.wait(timeout=3)


if __name__ == "__main__":
    main()
