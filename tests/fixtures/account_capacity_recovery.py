#!/usr/bin/env python3
"""Public CLI recovery through a real relay and synthetic account/upstream HTTP."""
import argparse
import base64
import datetime
import http.server
import json
import os
import re
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

SESSION = "019e8489-aa28-7071-ab90-16b81c7cfd1d"
MODEL = "gpt-6.1-sol"
MESSAGE = "Selected model is at capacity. Please try a different model."


def child():
    if "--version" in sys.argv:
        print("codex 0.163.0")
        return
    home = Path.home()
    alias = os.environ["CODEXCTL_PINNED_ALIAS"]
    with (home / "launches.jsonl").open("a") as out:
        out.write(json.dumps(dict(alias=alias, args=sys.argv[1:], cwd=os.getcwd())) + "\n")
    if alias != "a":
        print("INCLUDED-CAPACITY-RECOVERY-OK", flush=True)
        return
    sessions = home / ".codex/sessions/2026/10/20"
    sessions.mkdir(parents=True, exist_ok=True)
    (sessions / f"rollout-{SESSION}.jsonl").write_text(json.dumps({
        "type": "session_meta", "payload": {"id": SESSION, "cwd": os.getcwd(),
        "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat()}}) + "\n")
    print(json.dumps({"type": "thread.started", "thread_id": SESSION}), flush=True)
    print('{"type":"turn.started"}', flush=True)
    case = os.environ["CAPACITY_CASE"]
    if case == "quoted":
        print(json.dumps({"type":"item.completed","item":{"type":"command_execution","output":"■ " + MESSAGE}}), flush=True)
        sys.exit(1)
    launch = next((match.group(1) for arg in sys.argv if (match := re.search(r'X-Codexctl-Launch-Id"?\s*=\s*"([a-f0-9]+)"', arg))), None)
    for _ in range(2 if case == "below_threshold" else 0 if case == "no_evidence" else 3):
        req = urllib.request.Request(os.environ["CAPACITY_URL"] + "/backend-api/codex/responses",
            data=json.dumps({"model": MODEL}).encode(), headers={
                "thread-id": SESSION, "x-codexctl-account-alias": alias,
                "x-codexctl-account-class": "included", **({"x-codexctl-launch-id": launch} if launch else {})})
        try:
            urllib.request.urlopen(req, timeout=5).read()
        except urllib.error.HTTPError as error:
            error.read()
    if case in {"marked", "http_marked"}:
        print("■ " + MESSAGE, flush=True)
    else:
        print(json.dumps({"type": "turn.failed", "error": {"message": MESSAGE}}), flush=True)
    if case == "no_model":
        time.sleep(.2)
        (home / "capacity-session-survived").write_text("yes")
    sys.exit(1)


def probe(binary, case):
    token_requests, reports, upstream, approved_plans = [], [], [], []
    def account(alias, score, billing="rate_limited", available=True):
        return dict(userId="synthetic-user", alias=alias, label=None, accountId="seat-" + alias,
            plan="pro" if billing == "rate_limited" else "team_usage_based", billingClass=billing,
            primaryUsed=score, secondaryUsed=0, resetsAt=4102444800, available=available,
            usageScore=score, usageStale=False, liveSessions=0)
    accounts = [account("a", 1), account("b", 10), account("paid", 0, "usage_based"),
                account("excluded", 0, available=False)]
    class Server(http.server.BaseHTTPRequestHandler):
        def reply(self, status, value):
            body = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def do_GET(self):
            self.reply(200, accounts if self.path == "/v1/accounts" else {"id": "synthetic-user"})
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))))
            if self.path == "/v1/token":
                token_requests.append(body["alias"])
                row = next(a for a in accounts if a["alias"] == body["alias"])
                claims = {"sub": "login-" + row["alias"], "exp": 4102444800, "iat": 2000000000,
                    "https://api.openai.com/auth": {"chatgpt_account_id": row["accountId"], "chatgpt_plan_type": row["plan"]}}
                token = "header." + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip("=") + "."
                self.reply(200, dict(userId="synthetic-user", accessToken=token,
                    chatgptAccountId=row["accountId"], chatgptPlanType=row["plan"], revision="synthetic",
                    billingClass=row["billingClass"], nativeRoutingSupported=True))
            elif self.path == "/backend-api/codex/responses":
                upstream.append(body)
                data = ("event: response.failed\ndata: " + json.dumps({"type": "response.failed",
                    "response": {"error": {"code": "server_is_overloaded", "message": MESSAGE}}}) + "\n\n").encode()
                if case == "http_marked":
                    data=json.dumps({"error":{"code":"server_is_overloaded","message":MESSAGE}}).encode()
                self.send_response(503 if case == "http_marked" else 200)
                self.send_header("content-type", "application/json" if case == "http_marked" else "text/event-stream")
                self.send_header("x-request-id", "capacity-" + str(len(upstream)))
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)
            elif self.path == "/v1/relay/capacity-events":
                reports.append(body)
                self.reply(204, None)
            elif self.path == "/v1/capacity/plan":
                # Selection is covered separately against the real account server.
                if not body.get("session_id"):
                    self.reply(200, {"alias": "a", "reason": None})
                    return
                # The wrapper must keep an approved alias, even if shared
                # evidence changes before the child stops. A re-plan is refused.
                if approved_plans:
                    self.reply(200, {"alias": None, "reason": "evidence_pending"})
                    return
                evidence = [r["account_capacity"] for r in reports if "account_capacity" in r]
                overloaded = {e["event_id"] for e in evidence if e["alias"] == "a" and e["model"] == MODEL and e["outcome"] == "overloaded"}
                terminal = any(e["alias"] == "a" and e["model"] == MODEL and e["outcome"] == "terminal" and e["session_id"] == SESSION for e in evidence)
                if len(overloaded) == 3 and terminal:
                    approved_plans.append("b")
                self.reply(200, {"alias": "b" if len(overloaded) == 3 and terminal else None,
                    "reason": None if len(overloaded) == 3 and terminal else "evidence_pending"})
            else:
                self.reply(404, {})
        def log_message(self, *_):
            pass
    with tempfile.TemporaryDirectory(prefix="codexctl-account-capacity-") as tmp:
        home = Path(tmp).resolve()
        (home / "bin").mkdir()
        (home / "work").mkdir()
        (home / "bin/codex").symlink_to(Path(__file__).resolve())
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CODEX", "CLAUDE", "HERDR"))}
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Server) as server:
            thread = threading.Thread(target=server.serve_forever)
            thread.start()
            address = f"http://127.0.0.1:{server.server_port}"
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                relay_address = f"127.0.0.1:{sock.getsockname()[1]}"
            env.update(HOME=str(home), PATH=str(home / "bin") + ":" + os.environ["PATH"],
                CODEXCTL_SERVER_URL=address, CODEXCTL_SERVICE_TOKEN="synthetic-machine",
                CODEXCTL_ALLOW_INSECURE_LOOPBACK="1", CAPACITY_URL="http://" + relay_address, CAPACITY_CASE=case)
            relay = subprocess.Popen([binary, "relay", "serve", "--listen", relay_address,
                "--upstream", address, "--overloaded-budget-secs", "0"], env=env,
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            try:
                for _ in range(100):
                    assert relay.poll() is None
                    try:
                        urllib.request.urlopen(env["CAPACITY_URL"] + "/health", timeout=1).read()
                        break
                    except OSError:
                        time.sleep(.02)
                (home / ".codexctl/relay.json").write_text(json.dumps({"listen": relay_address}))
                command = [binary, "codex"]
                if case == "explicit": command += ["--account", "a"]
                model_args = [] if case == "no_model" else ['--config=model="' + MODEL + '"'] if case == "compact_config" else ["--model", MODEL]
                command += ["exec", "--json", *model_args, "synthetic"]
                result = subprocess.run(command,
                    env=env, cwd=home / "work", stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=25)
                launches = [json.loads(line) for line in (home / "launches.jsonl").read_text().splitlines()]
                recovers = case not in {"quoted", "no_evidence", "below_threshold", "explicit", "no_model"}
                expected = ["a", "b"] if recovers else ["a"]
                assert [x["alias"] for x in launches] == expected, (launches, result.stdout, result.stderr)
                assert result.returncode == (0 if recovers else 1), result.stderr
                if recovers:
                    assert approved_plans == ["b"], approved_plans
                    assert SESSION in launches[1]["args"], launches
                    assert any(MODEL in a for a in launches[1]["args"]), launches
                assert all(x["cwd"] == str(home / "work") for x in launches), launches
                assert set(token_requests) == set(expected), token_requests
                assert len(upstream) == (2 if case == "below_threshold" else 0 if case in {"quoted", "no_evidence"} else 3), upstream
                assert "capacity policy stopped" not in result.stderr, result.stderr
                assert not (home / ".codexctl/active").exists()
                if case == "no_model": assert (home / "capacity-session-survived").exists()
                print("PASS: same session/model/cwd, included b; no paid/excluded token or host active change")
            finally:
                relay.terminate()
                relay.communicate(timeout=10)
                server.shutdown()
                thread.join()


if Path(sys.argv[0]).name == "codex":
    child()
else:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--case", default="json", choices=["json", "marked", "http_marked", "compact_config", "explicit", "quoted", "no_evidence", "below_threshold", "no_model"])
    args = parser.parse_args()
    probe(str(Path(args.binary).resolve()), args.case)
