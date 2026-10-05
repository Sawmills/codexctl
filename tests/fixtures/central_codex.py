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
definitive_rejection = False
billing_rotated = False

if "login" in sys.argv:
    # Login writes a grant, but never reads accounts or refreshes credentials.
    mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
    if mode == "login-invalid-prompt":
        print("\033]52;unsupported\n", flush=True)
        while True:
            time.sleep(1)
    print("1. Open this link in your browser and sign in to your account", flush=True)
    print("   https://auth.openai.com/codex/device", flush=True)
    print("\n2. Enter this one-time code (expires in 15 minutes)", flush=True)
    print("   TEST-LOGIN", flush=True)
    gate = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).with_name("login-release")
    while not gate.exists():
        time.sleep(0.02)
    auth_path.write_text(gate.read_text())
    auth_path.chmod(0o600)
    if mode == "login-hold-after-save":
        gate.with_name("login-saved").write_text("saved")
        while not gate.with_name("login-exit").exists():
            time.sleep(0.02)
    sys.exit(0)


def send(value):
    print(json.dumps(value), flush=True)


def complete():
    send({"method": "item/completed", "params": {"threadId": "central-thread", "item": {"type": "agentMessage", "text": "CENTRAL_OK"}}})
    send({"method": "turn/completed", "params": {"threadId": "central-thread", "turn": {"id": "central-turn", "status": "completed"}}})


def rotate(plan_change=False):
    auth = json.loads(auth_path.read_text())
    old = auth["tokens"]["access_token"]
    payload = json.loads(base64.urlsafe_b64decode(old.split(".")[1] + "=="))
    mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
    if mode == "disconnect" or (mode == "hold" and payload.get("sub") == "alex-login"):
        pathlib.Path(os.environ["CENTRAL_TEST_REFRESH_COUNTER"]).with_name("refresh-started").write_text("started")
    if mode == "hold" and payload.get("sub") == "alex-login":
        release = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).with_name("release")
        deadline = time.monotonic() + 10
        while not release.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
    if mode in ["slow", "disconnect", "late-error"]:
        time.sleep(0.2)
    if mode == "gain-uid":
        payload["https://api.openai.com/auth"]["chatgpt_user_id"] = "learned-uid"
    if mode == "identity" or (mode == "identity-nonzero" and payload.get("sub") == "same-login"):
        payload["sub"] = "different-login"
    if plan_change:
        payload["https://api.openai.com/auth"]["chatgpt_plan_type"] = "business"
    payload["generation"] = payload.get("generation", 0) + 1
    payload["iat"] = 2000000000 + payload["generation"]
    body = base64.urlsafe_b64encode(json.dumps(payload).encode()).decode().rstrip("=")
    if mode != "refresh-only":
        auth["tokens"]["access_token"] = "eyJhbGciOiJub25lIn0." + body + "."
    auth["tokens"]["refresh_token"] = "synthetic-rotated-refresh"
    if mode == "refresh-only":
        counter = pathlib.Path(os.environ["CENTRAL_TEST_REFRESH_COUNTER"])
        auth["tokens"]["refresh_token"] = "synthetic-only-refresh-" + str(int(counter.read_text()) + 1)
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
        launch_counter = os.environ.get("CENTRAL_TEST_LAUNCH_COUNTER")
        if launch_counter:
            counter = pathlib.Path(launch_counter)
            counter.write_text(str(int(counter.read_text()) + 1))
        mode_path = os.environ.get("CENTRAL_TEST_MODE_FILE")
        mode = pathlib.Path(mode_path).read_text() if mode_path else ""
        if mode == "respawn-launch-fail-once":
            marker = pathlib.Path(mode_path).with_name("launch-failed")
            if not marker.exists():
                marker.write_text("failed")
                sys.exit(1)
        if os.environ.get("CENTRAL_TEST_OWNER_CWD_FILE"):
            pathlib.Path(os.environ["CENTRAL_TEST_OWNER_CWD_FILE"]).write_text(os.getcwd())
        if mode in ["startup-hold", "startup-hold-error"] and json.loads(auth_path.read_text())["tokens"]["account_id"] == "synthetic-seat":
            pathlib.Path(mode_path).with_name("initialize-started").write_text("started")
            deadline = time.monotonic() + 75
            while not pathlib.Path(mode_path).with_name("release-initialize").exists():
                if time.monotonic() >= deadline:
                    sys.exit(1)
                time.sleep(0.01)
        if auth_path.exists() and mode in ["startup", "startup-hold", "startup-hold-error", "startup-error", "startup-exit"]:
            rotate()
        if mode == "startup-exit":
            sys.exit(1)
        if mode in ["startup-error", "startup-hold-error"]:
            send({"id": message["id"], "error": {"code": -32000, "message": "synthetic initialize failure"}})
            continue
        result = {"userAgent": "synthetic-codex"}
    elif method == "account/read":
        if not params.get("refreshToken") and pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text() == "routing-billing-change":
            rotate(plan_change=True)
        if pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text() == "notifications":
            for _ in range(1500):
                send({"method": "account/rateLimits/updated", "params": {}})
        if params.get("refreshToken"):
            mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
            partial_failure = mode == "partial-migration" and json.loads(auth_path.read_text())["tokens"].get("account_id") == "bad-seat"
            invalid_grant = json.loads(auth_path.read_text())["tokens"].get("refresh_token") == "synthetic-rejected-refresh"
            if mode == "rejected-success" and invalid_grant:
                definitive_rejection = True
                send({"id":message["id"],"result":{"account":None,"requiresOpenaiAuth":True}})
                continue
            if mode in ["error", "routing-error", "non-exportable"] or partial_failure or invalid_grant:
                definitive_rejection = mode != "routing-error"
                send({"id": message["id"], "error": {"code": -32000, "message": "synthetic upstream rejection"}})
                continue
            if pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text() == "late-error":
                print("{invalid", flush=True)
            rotate()
        mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
        if mode in ["routing-policy-missing", "routing-policy-override", "routing-policy-wrong-code"]:
            text = "workspace routing discovery has invalid account routing override" if mode == "routing-policy-override" else "workspace routing discovery missing backend origin"
            code = -32000 if mode == "routing-policy-wrong-code" else -32603
            send({"id":message["id"], "error":{"code":code,"message":text}})
            continue
        saved = json.loads(auth_path.read_text())
        claims = json.loads(base64.urlsafe_b64decode(saved["tokens"]["access_token"].split(".")[1] + "=="))
        account_id = saved["tokens"].get("account_id") or saved.get("chatgpt_account_id") or claims.get("https://api.openai.com/auth", {}).get("chatgpt_account_id")
        result = {"account": {"type": "chatgpt"}, "workspaceRouting":{"chatgptAccountId":account_id, "backendOrigin":"https://chatgpt.com", "accountRoutingOverride":"NO_CONSTRAINT"}}
        if mode in ["routing-us", "routing-us_cr"]:
            result["workspaceRouting"]["accountRoutingOverride"] = mode.removeprefix("routing-")
        if mode == "routing-regional":
            result["workspaceRouting"]["backendOrigin"] = "https://regional.chatgpt.com"
        if mode == "routing-missing":
            result["workspaceRouting"] = None
    elif method == "getAuthStatus":
        mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
        if mode == "cached-rotation":
            rotate()
        if mode == "cached-rejection" and json.loads(auth_path.read_text())["tokens"].get("refresh_token") == "synthetic-rejected-refresh":
            definitive_rejection = True
        current = json.loads(auth_path.read_text())["tokens"]["access_token"]
        result = {"authMethod":"chatgpt", "authToken":None if definitive_rejection else current, "requiresOpenaiAuth":True}
        if mode in ["non-exportable", "baseline-null"]:
            result = {"authMethod":None, "authToken":None, "requiresOpenaiAuth":True}
    elif method == "account/rateLimits/read":
        mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
        if mode == "billing-policy-wrong-method":
            send({"id":message["id"], "error":{"code":-32603,"message":"workspace routing discovery missing backend origin"}})
            continue
        if mode == "billing-error":
            send({"id":message["id"], "error":{"code":-32000,"message":"synthetic billing read failure"}})
            continue
        if mode == "billing-error-marked":
            send({"id":message["id"], "error":{"code":-32000,"message":"synthetic billing read failure","data":{"retryable":True}}})
            continue
        if mode == "rpc-unhealthy":
            print("{invalid", flush=True)
            sys.exit(1)
        if mode == "billing-permanent-error":
            send({"id":message["id"], "error":{"code":-32000,"message":"synthetic permanent billing rejection"}})
            continue
        if mode in ["billing-error-once", "rpc-unhealthy-once"]:
            marker = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).with_name("billing-failed")
            if not marker.exists():
                marker.write_text("failed")
                if mode == "billing-error-once":
                    send({"id":message["id"], "error":{"code":-32000,"message":"synthetic billing read failure"}})
                    continue
                print("{invalid", flush=True)
                sys.exit(1)
        if mode == "billing-slow":
            pathlib.Path(os.environ["CENTRAL_TEST_REFRESH_COUNTER"]).with_name("billing-started").write_text("started")
            time.sleep(6)
        if mode == "billing-rotation" and not billing_rotated:
            rotate()
            billing_rotated = True
        auth = json.loads(auth_path.read_text())
        payload = json.loads(base64.urlsafe_b64decode(auth["tokens"]["access_token"].split(".")[1] + "=="))
        mode = pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text()
        result = {"rateLimits": {"planType":payload["https://api.openai.com/auth"].get("chatgpt_plan_type"), "primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":4102444800}, "credits":{"hasCredits": mode == "credits", "unlimited":False, "balance":"10" if mode == "credits" else "0"}}}
        if mode in ["closed-spend-cap", "open-spend-cap"]:
            result["rateLimits"]["spendControlReached"] = mode == "closed-spend-cap"
            result["rateLimits"]["credits"] = {"hasCredits":True,"unlimited":False,"balance":"10"}
        if mode in ["included-weekly", "exhausted-weekly", "credits"]:
            result["rateLimits"]["primary"] = {"usedPercent": 100 if mode == "exhausted-weekly" else 15, "windowDurationMins":10080,"resetsAt":4102444800}
            result["rateLimits"]["credits"] = {"hasCredits":True,"unlimited":False,"balance":"10"}
            result["rateLimits"]["spendControlReached"] = False
        if mode == "dashboard-unknown":
            result["rateLimits"]["primary"] = {"usedPercent":15}
        if mode in ["status-reset", "rate-distinct-reset"]:
            result["rateLimits"]["secondary"] = {"usedPercent":37,"windowDurationMins":10080,"resetsAt":4102444800}
        if mode == "rate-distinct-reset":
            result["rateLimits"]["primary"]["resetsAt"] = 4102440000
        if mode == "billing-late-change":
            rotate(plan_change=True)
        if mode.startswith("billing-routing-policy-"):
            rotate()
            pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).write_text(mode.removeprefix("billing-"))
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

if auth_path.exists() and pathlib.Path(os.environ["CENTRAL_TEST_MODE_FILE"]).read_text() == "identity-nonzero":
    saved = json.loads(auth_path.read_text())
    claims = json.loads(base64.urlsafe_b64decode(saved["tokens"]["access_token"].split(".")[1] + "=="))
    if claims.get("sub") == "different-login":
        sys.exit(1)

if os.environ.get("CENTRAL_TEST_EXIT_FILE"):
    pathlib.Path(os.environ["CENTRAL_TEST_EXIT_FILE"]).write_text("exited")
