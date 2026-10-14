#!/usr/bin/env python3
"""Test client for audit-runner (M1 verification): sends one job, prints the reply.

    client.py <socket> <job_id> <input text> [timeout_secs]

Connection errors are printed as `connect-error: <errno name> <message>` with exit 2.
"""
import errno
import json
import socket
import sys

path, job_id, text = sys.argv[1], sys.argv[2], sys.argv[3]
timeout = int(sys.argv[4]) if len(sys.argv) > 4 else None
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    s.connect(path)
except OSError as e:
    print(f"connect-error: {errno.errorcode.get(e.errno, e.errno)} {e.strerror}")
    sys.exit(2)
req = {"job_id": job_id, "input": text}
if timeout:
    req["timeout_secs"] = timeout
s.sendall((json.dumps(req, ensure_ascii=False) + "\n").encode())
data = b""
while not data.endswith(b"\n"):
    chunk = s.recv(65536)
    if not chunk:
        break
    data += chunk
resp = json.loads(data)
print(json.dumps(resp, ensure_ascii=False, indent=2))
