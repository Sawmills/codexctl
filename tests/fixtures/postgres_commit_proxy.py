"""PostgreSQL wire faults: delay startup or lose an audit COMMIT acknowledgement.

For isolated trust/NoTLS test databases only. Forward bytes without logging them.
"""
import json
import pathlib
import socket
import socketserver
import struct
import sys
import threading
import time

host, port, directory = sys.argv[1], int(sys.argv[2]), pathlib.Path(sys.argv[3])


def read_exact(stream, length):
    data = bytearray()
    while len(data) < length:
        chunk = stream.recv(length - len(data))
        if not chunk:
            raise EOFError
        data.extend(chunk)
    return bytes(data)


def packet(stream):
    kind = read_exact(stream, 1)
    size = read_exact(stream, 4)
    length = struct.unpack("!I", size)[0]
    if length < 4 or length > 16 * 1024 * 1024:
        raise ValueError("invalid PostgreSQL frame")
    payload = read_exact(stream, length - 4)
    return kind, payload, kind + size + payload


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        backend = socket.create_connection((host, port), timeout=10)
        backend.settimeout(None)
        state = {"audit": False, "commit": False}
        try:
            size = read_exact(self.request, 4)
            length = struct.unpack("!I", size)[0]
            startup = size + read_exact(self.request, length - 4)
            if (directory / "delay-startup").exists():
                delay = float((directory / "delay-startup").read_text())
                (directory / "startup-delayed").write_text("PostgreSQL startup delayed\n")
                time.sleep(delay)
            backend.sendall(startup)

            def client_to_database():
                try:
                    while True:
                        kind, payload, raw = packet(self.request)
                        sql = b""
                        if kind == b"P":
                            sql = payload.split(b"\0", 2)[1]
                        elif kind == b"Q":
                            sql = payload.split(b"\0", 1)[0]
                        if b"INSERT INTO dashboard_audit" in sql:
                            state["audit"] = True
                        if sql.strip().upper() == b"COMMIT":
                            state["commit"] = True
                        backend.sendall(raw)
                except (EOFError, OSError, ValueError):
                    try:
                        backend.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass

            forward = threading.Thread(target=client_to_database, daemon=True)
            forward.start()
            while True:
                kind, payload, raw = packet(backend)
                if (kind == b"C" and payload in (b"BEGIN\0", b"START TRANSACTION\0")
                        and (directory / "delay-begin").exists()):
                    delay = float((directory / "delay-begin").read_text())
                    (directory / "begin-delayed").write_text("PostgreSQL BEGIN response delayed\n")
                    time.sleep(delay)
                if (kind == b"C" and payload == b"COMMIT\0" and state["audit"]
                        and state["commit"] and (directory / "lose-commit-ack").exists()):
                    try:
                        with (directory / "ack-dropped").open("x") as receipt:
                            receipt.write("PostgreSQL committed; acknowledgement dropped\n")
                        break
                    except FileExistsError:
                        pass
                self.request.sendall(raw)
        except (EOFError, OSError, ValueError):
            pass
        finally:
            for stream in (self.request, backend):
                try:
                    stream.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                stream.close()


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True


server = Server(("127.0.0.1", 0), Handler)
print(json.dumps({"port": server.server_address[1]}), flush=True)
server.serve_forever()
