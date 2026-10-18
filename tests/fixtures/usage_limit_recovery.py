#!/usr/bin/env python3
"""Synthetic account server and Codex child for the public recovery CLI."""
import argparse
import base64
import datetime
import http.server
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import threading
import time

SESSION = "019e8489-aa28-7071-ab90-16b81c7cfd1d"
MESSAGE = "You’ve hit your usage limit. To get more access now, send a request to your admin or try again at Jan 1st, 2030 12:00 AM."


def split_output(case):
    if case == "split_utf8":
        event = json.dumps({"type": "turn.failed", "error": {"message": MESSAGE}},
                           ensure_ascii=False).encode() + b"\n"
        return event, event.index("’".encode()) + 1
    prefix = b'{"type":"item.completed","item":{"type":"command_execution","output":"'
    marker = ("■ " + MESSAGE).encode()
    # The PTY adds CR before the final LF: 17001 - retained 8192 = 8809.
    event = prefix + b"x" * (8809 - len(prefix)) + marker
    event += b"y" * (17000 - len(event) - len(b'"}}\n')) + b'"}}\n'
    assert json.loads(event)["type"] == "item.completed"
    return event, 16384


def child():
    if "--version" in sys.argv:
        print("codex 0.163.0")
        return
    home = Path(os.environ["HOME"])
    alias = os.environ["CODEXCTL_PINNED_ALIAS"]
    with (home / "launches.jsonl").open("a") as output:
        output.write(json.dumps({"alias": alias, "args": sys.argv[1:], "cwd": os.getcwd()}) + "\n")
    if alias == "a":
        case = os.environ["RECOVERY_CASE"]
        sessions = home / ".codex/sessions/2026/10/18"
        sessions.mkdir(parents=True, exist_ok=True)
        (sessions / f"rollout-{SESSION}.jsonl").write_text(json.dumps({
            "type": "session_meta", "payload": {"id": SESSION, "cwd": os.getcwd(),
                "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat()}}) + "\n")
        if case == "marked":
            print("\x1b[31m■ " + MESSAGE + "\x1b[0m", flush=True)
            sys.exit(1)
        if case == "marked_wrapped":
            print("■ You’ve hit your usage ▕\nlimit. To get more access now ▕", flush=True)
            sys.exit(1)
        if case == "quoted_json":
            print(json.dumps({"type": "item.completed", "item": {"type": "command_execution",
                "output": "■ " + MESSAGE + "\n■ exceeded retry limit, last status: 429"}},
                ensure_ascii=False), flush=True)
            print(json.dumps({"type": "turn.completed"}), flush=True)
            return
        if case == "quoted_text":
            print("│ tool output: ■ " + MESSAGE + " │", flush=True)
            print("assistant quoted: ■ " + MESSAGE, flush=True)
            sys.exit(1)
        if case == "nonterminal_error":
            print(json.dumps({"type": "error", "message": MESSAGE}, ensure_ascii=False), flush=True)
            sys.exit(1)
        if case in {"split_utf8", "long_quoted_json"}:
            event, split = split_output(case)
            os.write(sys.stdout.fileno(), event[:split])
            deadline = time.monotonic() + 10
            while not (home / "release-output").exists():
                if time.monotonic() >= deadline:
                    raise TimeoutError("waiting for wrapper to forward first output fragment")
                time.sleep(0.005)
            os.write(sys.stdout.fileno(), event[split:])
            sys.exit(0 if case == "long_quoted_json" else 1)
        # Captured from real Codex 0.163.0 against a usage_limit_reached 429.
        for event in [{"type": "thread.started", "thread_id": SESSION},
                      {"type": "turn.started"}, {"type": "error", "message": MESSAGE},
                      {"type": "turn.failed", "error": {"message": MESSAGE}}]:
            print(json.dumps(event, ensure_ascii=False), flush=True)
        sys.exit(1)
    print("INCLUDED-RECOVERY-OK", flush=True)


def probe(binary, case):
    requests = []
    def account(alias, score, billing="rate_limited", available=True):
        return dict(userId="synthetic-user", alias=alias, label=None, accountId="seat-" + alias,
                    plan="team_usage_based" if billing == "usage_based" else "pro",
                    billingClass=billing, primaryUsed=score, secondaryUsed=0,
                    resetsAt=4102444800, available=available, usageScore=score,
                    usageStale=False, liveSessions=0)
    accounts = [account("a", 1), account("b", 10),
                account("paid", 0, "usage_based"), account("excluded", 0, available=False)]
    if case == "no_alternate":
        accounts[1]["available"] = False

    class Server(http.server.BaseHTTPRequestHandler):
        def reply(self, status, body):
            data = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            if self.path == "/v1/accounts":
                self.reply(200, accounts)
            elif self.path == "/v1/me":
                self.reply(200, {"id": "synthetic-user"})
            else:
                self.reply(404, {})

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/v1/token":
                requests.append(body["alias"])
                row = next(row for row in accounts if row["alias"] == body["alias"])
                claims = {"sub": "login-" + row["alias"], "exp": 4102444800, "iat": 2000000000,
                          "https://api.openai.com/auth": {
                              "chatgpt_account_id": row["accountId"], "chatgpt_plan_type": row["plan"]}}
                token = "header." + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=") + "."
                self.reply(200, dict(userId="synthetic-user", accessToken=token,
                    chatgptAccountId=row["accountId"], chatgptPlanType=row["plan"],
                    revision="synthetic", billingClass=row["billingClass"], nativeRoutingSupported=True))
            elif self.path == "/v1/relay/capacity-events":
                self.reply(204, None)
            else:
                self.reply(404, {})

        def log_message(self, *_):
            pass

    with tempfile.TemporaryDirectory(prefix="codexctl-usage-recovery-") as temporary:
        home = Path(temporary)
        (home / "bin").mkdir()
        (home / "work").mkdir()
        # Execute the checked-in child; never race an executable being written.
        (home / "bin/codex").symlink_to(Path(__file__).resolve())
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("CODEX", "CLAUDE", "HERDR"))}
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server) as server:
            thread = threading.Thread(target=server.serve_forever)
            thread.start()
            try:
                env.update(HOME=str(home), PATH=str(home / "bin") + ":" + os.environ["PATH"],
                           RECOVERY_CASE=case,
                           CODEXCTL_SERVER_URL=f"http://127.0.0.1:{server.server_port}",
                           CODEXCTL_SERVICE_TOKEN="synthetic-machine", CODEXCTL_ALLOW_INSECURE_LOOPBACK="1")
                command = [binary, "codex"]
                if case == "explicit":
                    command.extend(["--account", "a"])
                command.extend(["exec", "--json", "synthetic"])
                if case in {"split_utf8", "long_quoted_json"}:
                    event, split = split_output(case)
                    with subprocess.Popen(command, env=env, cwd=home / "work",
                            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE) as process:
                        try:
                            forwarded = b""
                            deadline = time.monotonic() + 10
                            while len(forwarded) < split:
                                ready, _, _ = select.select([process.stdout], [], [],
                                    max(0, deadline - time.monotonic()))
                                if not ready:
                                    raise TimeoutError("wrapper forwarding first output fragment")
                                part = os.read(process.stdout.fileno(), split - len(forwarded))
                                assert part, "wrapper exited before forwarding first fragment"
                                forwarded += part
                            assert forwarded == event[:split]
                            (home / "release-output").touch()
                            output, error = process.communicate(timeout=20)
                            result = subprocess.CompletedProcess(command, process.returncode,
                                (forwarded + output).decode(), error.decode())
                        finally:
                            if process.poll() is None:
                                process.kill()
                else:
                    result = subprocess.run(command,
                        env=env, cwd=home / "work", stdin=subprocess.DEVNULL,
                        capture_output=True, text=True, timeout=20)
                launches = [json.loads(line) for line in (home / "launches.jsonl").read_text().splitlines()]
                recovers = case not in {"quoted_json", "long_quoted_json", "quoted_text", "nonterminal_error",
                                        "explicit", "no_alternate"}
                expected_exit = 0 if recovers or case in {"quoted_json", "long_quoted_json"} else 1
                assert result.returncode == expected_exit, (result.stdout, result.stderr)
                expected_aliases = ["a", "b"] if recovers else ["a"]
                assert [launch["alias"] for launch in launches] == expected_aliases, launches
                if recovers:
                    assert SESSION in launches[1]["args"], launches
                    assert "INCLUDED-RECOVERY-OK" in result.stdout, result.stdout
                assert all(launch["cwd"] == str(home / "work") for launch in launches)
                assert set(requests) == set(expected_aliases), requests
                print(f"PASS {case}: exit {expected_exit}, aliases {expected_aliases}; paid/excluded untouched")
            finally:
                server.shutdown()
                thread.join()


if Path(sys.argv[0]).name == "codex":
    child()
else:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--case", default="json")
    args = parser.parse_args()
    probe(args.binary, args.case)
