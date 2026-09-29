#!/usr/bin/env python3
"""Synthetic external Codex protocol fixture; never used for live acceptance."""
import base64
import json
import os
import pathlib
import sys
import time

home = pathlib.Path(os.environ["CODEX_HOME"])
auth_path = home / "auth.json"
access = None
waiting = False
pending_turn = None


def send(value):
    print(json.dumps(value), flush=True)


def complete():
    send({"method": "item/completed", "params": {"threadId": "central-thread", "item": {"type": "agentMessage", "text": "CENTRAL_OK"}}})
    send({"method": "turn/completed", "params": {"threadId": "central-thread", "turn": {"id": "central-turn", "status": "completed"}}})


def rotate():
    auth = json.loads(auth_path.read_text())
    old = auth["tokens"]["access_token"]
    payload = json.loads(base64.urlsafe_b64decode(old.split(".")[1] + "=="))
    mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
    if mode == "disconnect":
        pathlib.Path(os.environ["CENTRAL_TEST_REFRESH_COUNTER"]).with_name("refresh-started").write_text("started")
    if mode in ["slow", "disconnect"]:
        time.sleep(0.2)
    if mode == "identity":
        payload["sub"] = "different-login"
    payload["generation"] = payload.get("generation", 0) + 1
    body = base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip("=")
    auth["tokens"]["access_token"] = "eyJhbGciOiJub25lIn0." + body + "."
    auth["tokens"]["refresh_token"] = "synthetic-rotated-refresh"
    auth_path.write_text(json.dumps(auth))
    auth_path.chmod(0o600)
    counter = pathlib.Path(os.environ["CENTRAL_TEST_REFRESH_COUNTER"])
    counter.write_text(str(int(counter.read_text()) + 1))
    if mode == "break-key":
        pathlib.Path(os.environ["CENTRAL_TEST_KEY_FILE"]).unlink()


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    params = message.get("params", {})
    result = {}
    if method == "initialize":
        if auth_path.exists() and pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text() == "startup":
            rotate()
        result = {"userAgent": "synthetic-codex"}
    elif method == "account/read":
        if pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text() == "notifications":
            for _ in range(1500):
                send({"method": "account/rateLimits/updated", "params": {}})
        if params.get("refreshToken"):
            if pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text() == "error":
                send({"id": message["id"], "error": {"code": -32000, "message": "synthetic upstream rejection"}})
                continue
            rotate()
        result = {"account": {"type": "chatgpt"}}
    elif method == "account/rateLimits/read":
        mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
        if mode == "billing-error":
            send({"id":message["id"], "error":{"code":-32000,"message":"synthetic billing read failure"}})
            continue
        if mode == "billing-rotation":
            rotate()
        auth = json.loads(auth_path.read_text())
        payload = json.loads(base64.urlsafe_b64decode(auth["tokens"]["access_token"].split(".")[1] + "=="))
        mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
        result = {"rateLimits": {"planType":payload["https://api.openai.com/auth"].get("chatgpt_plan_type"), "primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":4102444800}, "credits":{"hasCredits": mode == "credits", "unlimited":False, "balance":"10" if mode == "credits" else "0"}}}
    elif method == "account/login/start":
        access = params["accessToken"]
        result = {"type": "chatgptAuthTokens"}
    elif method == "thread/start":
        result = {"thread": {"id": "central-thread"}}
    elif method == "turn/start":
        if os.environ.get("CENTRAL_TEST_EARLY_CALLBACK"):
            pending_turn = message["id"]
        else:
            send({"id": message["id"], "result": {"turn": {"id": "central-turn"}}})
        send({"method": "account/chatgptAuthTokens/refresh", "id": 9001, "params": {"previousAccountId": "acct-central", "reason": "unauthorized"}})
        waiting = True
        continue
    elif method is None and waiting and message.get("id") == 9001:
        if "error" not in message:
            if pending_turn is not None:
                send({"id": pending_turn, "result": {"turn": {"id": "central-turn"}}})
            complete()
        waiting = False
        continue
    if "id" in message:
        send({"id": message["id"], "result": result})

if auth_path.exists() and pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text() == "exit-rotation":
    rotate()
