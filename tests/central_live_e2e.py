#!/usr/bin/env python3
"""Two real local Codex clients, one broker, no live refresh or profile writes."""
import argparse
import concurrent.futures
import hashlib
import json
import pathlib
import signal
import subprocess
import tempfile


def checked(binary, args, cwd):
    result = subprocess.run([str(binary), *map(str, args)], cwd=cwd, capture_output=True, text=True, timeout=200)
    if result.returncode:
        raise RuntimeError("prototype command failed: " + result.stderr.strip())
    return result.stdout.strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, required=True)
    parser.add_argument("--auth", type=pathlib.Path, required=True)
    parser.add_argument("--codex-bin", default="codex")
    parser.add_argument("--model", default="gpt-6.1-sol")
    args = parser.parse_args()
    args.binary = args.binary.resolve()
    original = hashlib.sha256(args.auth.read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory(prefix="codexctl-central-live-") as directory:
        root = pathlib.Path(directory)
        state, key = root / "server", root / "vault-key"
        checked(args.binary, ["init", "--state", state, "--key-file", key, "--auth", args.auth, "--alias", "live", "--tenant", "prototype", "--user", "owner"], root)
        tokens = [root / "machine-a.token", root / "machine-b.token"]
        for index, token in enumerate(tokens):
            checked(args.binary, ["register", "--state", state, "--device", f"machine-{index}", "--tenant", "prototype", "--user", "owner", "--token-file", token], root)
        server = subprocess.Popen([str(args.binary), "serve", "--state", str(state), "--key-file", str(key), "--listen", "127.0.0.1:0", "--codex-bin", args.codex_bin, "--read-only"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            ready = json.loads(server.stdout.readline())
            url = "http://" + ready["listening"]
            def run(token):
                return checked(args.binary, ["run", "--server", url, "--token-file", token, "--codex-bin", args.codex_bin, "--model", args.model, "Reply exactly CENTRAL_LIVE_OK. Do not use tools."], root)
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
                outputs = list(pool.map(run, tokens))
            if outputs != ["CENTRAL_LIVE_OK", "CENTRAL_LIVE_OK"]:
                raise RuntimeError("unexpected model response")
            if hashlib.sha256(args.auth.read_bytes()).hexdigest() != original:
                raise RuntimeError("source auth changed during compatibility run")
            print(json.dumps({"live_clients": 2, "responses": outputs, "live_refresh": "disabled", "source_auth": "unchanged", "physical_machines": 1, "codex_version": subprocess.check_output([args.codex_bin, "--version"], text=True).strip()}))
        finally:
            server.send_signal(signal.SIGINT)
            try:
                server.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.communicate()


if __name__ == "__main__":
    main()
