#!/usr/bin/env python3
"""Real launch adapters with synthetic archives and a native-selection stand-in."""

import fcntl
import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def child():
    home = Path(os.environ["HOME"])
    receipt = home / "child.json"
    receipt.write_text(json.dumps({"args": sys.argv[1:]}))
    expected = json.loads(os.environ["RESUME_EXPECTED_ARGS"])
    assert sys.argv[1:] == expected, (sys.argv[1:], expected)
    sessions = Path(os.environ.get("CODEX_HOME", str(home / ".codex"))) / "sessions"
    rows = [json.loads(path.read_text()) for path in sessions.glob("*.jsonl")]
    assert len(rows) == 2, rows
    assert not list(sessions.glob("*.gz"))
    # The child decides which visible session to use; the wrapper must leave
    # --last untouched and make both candidates available before this decision.
    selected = next(row for row in rows if row["payload"]["native_pick"])
    receipt.write_text(json.dumps({"args": sys.argv[1:], "selected": selected["payload"]["session_id"]}))


def check(binary, command, case):
    with tempfile.TemporaryDirectory(prefix="codexctl-state-cap-resume-") as temporary:
        home = Path(temporary).resolve()
        root = home / ".codex"
        sessions = root / "sessions"
        sessions.mkdir(parents=True)
        bindir = home / "bin"
        bindir.mkdir()
        (bindir / "codex").symlink_to(Path(__file__).resolve())
        ids = ["11111111-1111-4111-8111-111111111111", "22222222-2222-4222-8222-222222222222"]
        entries = []
        for index, session_id in enumerate(ids):
            original = sessions / f"rollout-{session_id}.jsonl"
            archive = original.with_suffix(".jsonl.gz")
            source = json.dumps({"type": "session_meta", "payload": {"session_id": session_id, "native_pick": index == 0, "pad": "x" * 8192}}).encode()
            archive.write_bytes(gzip.compress(source))
            entries.append(f"{session_id}\t{original.relative_to(root)}\t{archive.relative_to(root)}\t{len(source)}\t0\t{hashlib.sha256(source).hexdigest()}\n")
        (root / "state-cap-archive-index.tsv").write_text("".join(entries))
        profile = home / ".codexctl/profiles/a"
        profile.mkdir(parents=True)
        auth = '{"access_token":"synthetic-local-token"}'
        (profile / "auth.json").write_text(auth)
        (profile / "meta.json").write_text(json.dumps({"alias": "a", "saved_at": "2026-01-01T00:00:00Z"}))
        (root / "auth.json").write_text(auth)
        native_args = ["--cd", str(home), "resume", "--last", "--all"]
        env = {key: value for key, value in os.environ.items() if not key.startswith(("CODEX", "CLAUDE", "HERDR"))}
        env.update(HOME=str(home), PATH=str(bindir) + os.pathsep + os.environ["PATH"], RESUME_EXPECTED_ARGS=json.dumps(native_args))
        if case == "inherited":
            pinned = home / ".codexctl/exec-homes/a"
            pinned.mkdir(parents=True)
            (pinned / "sessions").symlink_to(sessions)
            (pinned / "auth.json").write_text(auth)
            env["CODEX_HOME"] = str(pinned)
        lock = None
        if case == "low-space":
            # 1 GiB + 12 KiB fits either 8 KiB source, but not both.
            (bindir / "df").symlink_to(Path(__file__).resolve())
        elif case == "locked":
            (root / "state-cap-archive-index.tsv").unlink()
            lock = (root / ".state-cap.lock").open("w")
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        args = [binary, "codex"] if command == "codex" else [binary, "exec", "--account", "a", "--", "codex"]
        result = subprocess.run(args + native_args, env=env, cwd=home, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=30)
        if lock:
            lock.close()
        if case in {"restores", "inherited"}:
            assert result.returncode == 0, (result.stdout, result.stderr)
            receipt = json.loads((home / "child.json").read_text())
            assert receipt == {"args": native_args, "selected": ids[0]}, receipt
            assert (root / "state-cap-archive-index.tsv").read_text() == ""
        else:
            assert result.returncode != 0, (result.stdout, result.stderr)
            reason = "insufficient free space" if case == "low-space" else "state-cap lock is held"
            assert reason in result.stderr, result.stderr
            assert not (home / "child.json").exists(), "child started after restore refusal"
            assert len(list(sessions.glob("*.gz"))) == 2
            assert not list(sessions.glob("*.jsonl"))


if __name__ == "__main__":
    if Path(sys.argv[0]).name == "codex":
        child()
    elif Path(sys.argv[0]).name == "df":
        print("Filesystem 1024-blocks Used Available Capacity Mounted on\nfixture 2097152 1048564 1048588 50% /")
    else:
        check(*sys.argv[1:])
