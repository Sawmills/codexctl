#!/usr/bin/env python3
"""Synthetic external OIDC issuer with real RSA signatures and PKCE validation."""
import base64
import hashlib
import json
import pathlib
import secrets
import subprocess
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlparse

root = pathlib.Path(sys.argv[1])
key = root / "oidc-private.pem"
subprocess.run(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", str(key)], check=True, capture_output=True)
key.chmod(0o600)
modulus = subprocess.run(["openssl", "rsa", "-in", str(key), "-modulus", "-noout"], check=True, capture_output=True, text=True).stdout.strip().split("=")[1]
codes = {}

def b64(data):
    return base64.urlsafe_b64encode(data).decode().rstrip("=")

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def output(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        url = urlparse(self.path)
        if url.path == "/.well-known/openid-configuration":
            self.output(200, {"issuer": issuer, "authorization_endpoint": issuer + "/authorize", "token_endpoint": issuer + "/token", "jwks_uri": issuer + "/keys", "response_types_supported": ["code"], "subject_types_supported": ["public"], "id_token_signing_alg_values_supported": ["RS256"], "token_endpoint_auth_methods_supported": ["client_secret_basic"], "scopes_supported": ["openid", "email"], "code_challenge_methods_supported": ["S256"]})
        elif url.path == "/keys":
            self.output(200, {"keys": [{"kty": "RSA", "use": "sig", "alg": "RS256", "kid": "test-key", "n": b64(bytes.fromhex(modulus)), "e": b64(bytes.fromhex("010001"))}]})
        elif url.path == "/authorize":
            params = {key: value[0] for key, value in parse_qs(url.query).items()}
            if params.get("code_challenge_method") != "S256":
                self.output(400, {"error": "PKCE required"})
                return
            code = secrets.token_urlsafe(32)
            codes[code] = params
            self.send_response(302)
            self.send_header("Location", params["redirect_uri"] + "?" + urlencode({"code": code, "state": params["state"]}))
            self.end_headers()
        else:
            self.output(404, {})

    def do_POST(self):
        data = parse_qs(self.rfile.read(int(self.headers.get("Content-Length", "0"))).decode())
        params = codes.pop(data.get("code", [""])[0], None)
        verifier = data.get("code_verifier", [""])[0]
        if params is None or b64(hashlib.sha256(verifier.encode()).digest()) != params["code_challenge"]:
            self.output(400, {"error": "invalid_grant"})
            return
        identity = json.loads((root / "identity.json").read_text())
        now = int(time.time())
        claims = {"iss": issuer, "sub": identity["sub"], "aud": identity.get("aud", "codexctl-test"), "iat": now, "exp": now + 300, "nonce": identity.get("nonce", params["nonce"]), "email": identity["email"], "email_verified": identity.get("verified", True)}
        signing_input = (b64(json.dumps({"alg": "RS256", "kid": "test-key"}).encode()) + "." + b64(json.dumps(claims).encode())).encode()
        signature = subprocess.run(["openssl", "dgst", "-sha256", "-sign", str(key)], input=signing_input, check=True, capture_output=True).stdout
        token = signing_input.decode() + "." + b64(signature)
        self.output(200, {"access_token": "synthetic-company-access", "token_type": "Bearer", "expires_in": 300, "id_token": token})

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
issuer = "http://127.0.0.1:" + str(server.server_address[1])
print(json.dumps({"issuer": issuer}), flush=True)
server.serve_forever()
