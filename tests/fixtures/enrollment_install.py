#!/usr/bin/env python3
"""Synthetic enrollment protocol with a deterministic local installation failure."""
import json
import pathlib
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

destination = pathlib.Path(sys.argv[1])


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def reply(self, value):
        payload = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", "0")))
        if self.path == "/v1/enrollment/start":
            self.reply({"deviceCode": "synthetic-install-code", "userCode": "SYNTHETIC", "verificationUrl": "http://127.0.0.1:" + str(self.server.server_port) + "/enroll", "expiresIn": 300, "interval": 1})
        else:
            self.reply({"deviceToken": "synthetic-install-credential"})

    def do_GET(self):
        # install has checked its destination before requesting the user identity.
        # A directory appearing here makes only its final atomic rename fail.
        destination.mkdir()
        self.reply({"id": "synthetic-user"})


server = HTTPServer(("127.0.0.1", 0), Handler)
print(json.dumps({"url": "http://127.0.0.1:" + str(server.server_port)}), flush=True)
server.serve_forever()
