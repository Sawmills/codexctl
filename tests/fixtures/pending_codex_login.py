#!/usr/bin/env python3
"""A pending device-login process that finishes without receiving credentials."""
import os
import pathlib
import time

gate = pathlib.Path(os.environ["CENTRAL_TEST_LOGIN_GATE"])
gate.with_suffix(".ready").touch()
deadline = time.monotonic() + 30
while not gate.exists() and time.monotonic() < deadline:
    time.sleep(0.02)
raise SystemExit(1)
