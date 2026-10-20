"""Exercise real state-cap CLI accounting, SQLite, and publication failures."""

import gzip
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def run(binary, home, apply=False, extra_env=None, expect_report=True):
    env = {key: value for key, value in os.environ.items() if not key.startswith(("CODEX", "CLAUDE", "HERDR"))}
    env["HOME"] = str(home)
    env.update(extra_env or {})
    args = [binary, "state", "cap", "--max-bytes", "0", "--min-free-bytes", "0", "--older-than-days", "0", "--reserve-bytes", "0"]
    if apply:
        args.append("--apply")
    result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=30)
    if expect_report:
        assert "Codex state cap:" in result.stdout, (result.stdout, result.stderr)
    fields = dict(word.split("=", 1) for word in result.stdout.split() if "=" in word)
    return fields, result


def leave_committed_wal(path):
    # A crashed writer leaves committed WAL frames with no holder.
    subprocess.run([sys.executable, "-c", "import os,sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('PRAGMA journal_mode=WAL'); c.execute('PRAGMA wal_autocheckpoint=0'); c.execute('CREATE TABLE fixture(value)'); c.execute('INSERT INTO fixture VALUES(42)'); c.commit(); os._exit(0)", str(path)], check=True, timeout=10)


def check(binary, case):
    with tempfile.TemporaryDirectory(prefix="codexctl-state-cap-safety-") as temporary:
        home = Path(temporary).resolve()
        root = home / ".codex"
        root.mkdir()
        if case == "symlink":
            external = home / "outside.sqlite"
            leave_committed_wal(external)
            files = [external, home / "outside.sqlite-wal", home / "outside.sqlite-shm"]
            before = [path.read_bytes() for path in files]
            (root / "logs_27.sqlite").symlink_to(external)
            for apply in [False, True]:
                _, result = run(binary, home, apply=apply, expect_report=False)
                assert result.returncode != 0, (result.stdout, result.stderr)
                assert "symlinked state database entry" in result.stderr, result.stderr
                assert [path.read_bytes() for path in files] == before, "external database was modified"
            return
        if case == "accounting":
            files = {"logs_27.sqlite": 100, "logs_27.sqlite-wal": 25, "logs_27.sqlite-shm": 13,
                     "thread_history_8.sqlite": 200, "thread_history_8.sqlite-wal": 31,
                     "thread_history_8.sqlite-shm": 7, "logs_99.sqlite-wal": 19}
            for name, size in files.items():
                (root / name).write_bytes(b"x" * size)
            for name in ["logs_old.sqlite", "state_5.sqlite", "thread_history_.sqlite", "logs_27.sqlite-journal"]:
                (root / name).write_bytes(b"x" * 4096)
            fields, _ = run(binary, home)
            assert fields["total_bytes"] == "395", fields
            assert fields["final_bytes"] == "395", fields
            assert fields["cap_met"] == "false", fields
            return
        if case == "checkpoint":
            for name in ["logs_27.sqlite", "thread_history_8.sqlite"]:
                leave_committed_wal(root / name)
                assert (root / (name + "-wal")).stat().st_size > 0
            fields, result = run(binary, home, apply=True)
            assert fields["errors"] == "0", (fields, result.stderr)
            for name in ["logs_27.sqlite", "thread_history_8.sqlite"]:
                wal = root / (name + "-wal")
                assert not wal.exists() or wal.stat().st_size == 0, name
            return
        sessions = root / "sessions"
        sessions.mkdir()
        original = sessions / "rollout-durable.jsonl"
        source = json.dumps({"type": "session_meta", "payload": {"session_id": "durable", "pad": "x" * 8192}}).encode()
        original.write_bytes(source)
        archive = original.with_suffix(".jsonl.gz")
        trace = home / "trace"
        library = home / "sync-fault.so"
        subprocess.run(["cc", "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", str(Path(__file__).with_name("state_cap_sync_fault.c")), "-ldl", "-o", str(library)], check=True, capture_output=True, text=True, timeout=30)
        fields, result = run(binary, home, apply=True, extra_env={
            "LD_PRELOAD": str(library), "STATE_CAP_TEST_ROOT": str(root),
            "STATE_CAP_TEST_SOURCE": str(original), "STATE_CAP_TEST_TRACE": str(trace),
            "STATE_CAP_TEST_FAIL": case})
        events = trace.read_text().splitlines() if trace.exists() else []
        if case == "success":
            assert events == ["sync-archive", "sync-index", "unlink-source", "sync-source"], events
            assert fields["errors"] == "0", (fields, result.stderr)
            assert not original.exists()
        else:
            assert fields["errors"] == "1", (fields, result.stderr, events)
            assert f"fail-{case}" in events, events
            assert original.exists() == (case != "source"), events
            if case != "source":
                assert "unlink-source" not in events, events
                assert original.read_bytes() == source
        if not original.exists():
            assert gzip.decompress(archive.read_bytes()) == source
            assert (root / "state-cap-archive-index.tsv").read_text().startswith("durable\t")
        if case in {"archive", "index"}:
            # A failed publication sync must not leave an unindexed archive
            # that prevents maintenance from retrying once the disk recovers.
            assert not archive.exists(), "unindexed archive left after failed sync"
            index = root / "state-cap-archive-index.tsv"
            assert not index.exists() or index.read_text() == "", "stale index left after failed sync"
            if case == "index":
                # A subsequent native write must not encounter a stale hash
                # left behind by the failed index-directory publication.
                source += b"\n{}\n"
                original.write_bytes(source)
            trace.unlink()
            fields, result = run(binary, home, apply=True, extra_env={
                "LD_PRELOAD": str(library), "STATE_CAP_TEST_ROOT": str(root),
                "STATE_CAP_TEST_SOURCE": str(original), "STATE_CAP_TEST_TRACE": str(trace),
                "STATE_CAP_TEST_FAIL": "success"})
            assert fields["errors"] == "0", (fields, result.stderr)
            assert trace.read_text().splitlines() == ["sync-archive", "sync-index", "unlink-source", "sync-source"]
            assert not original.exists()
            assert gzip.decompress(archive.read_bytes()) == source


if __name__ == "__main__":
    check(*sys.argv[1:])
