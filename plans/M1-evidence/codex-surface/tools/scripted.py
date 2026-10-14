#!/usr/bin/env python3
"""Fake Responses API that plays a scripted model: the first request is answered with the tool
call given as JSON in argv[3]; later requests get a plain final message. Every body is saved."""
import json, sys, http.server, itertools, threading
out, port, call = sys.argv[1], int(sys.argv[2]), json.loads(sys.argv[3])
seq = itertools.count(1); lock = threading.Lock(); done = [False]
def sse(ev, data): return f"event: {ev}\ndata: {json.dumps(data)}\n\n".encode()
usage={"input_tokens":1,"input_tokens_details":{"cached_tokens":0},"output_tokens":1,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":2}
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        self.send_response(404); self.end_headers()
    def do_POST(self):
        n = next(seq); raw = self.rfile.read(int(self.headers.get("content-length",0)))
        open(f"{out}/req{n:02d}.json","wb").write(raw)
        with lock:
            first = not done[0]; done[0] = True
        self.send_response(200); self.send_header("content-type","text/event-stream"); self.end_headers()
        self.wfile.write(sse("response.created",{"type":"response.created","response":{"id":f"r{n}"}}))
        item = call if first else {"type":"message","role":"assistant","id":f"m{n}","content":[{"type":"output_text","text":f"FINAL-{n}"}]}
        self.wfile.write(sse("response.output_item.done",{"type":"response.output_item.done","output_index":0,"item":item}))
        self.wfile.write(sse("response.completed",{"type":"response.completed","response":{"id":f"r{n}","usage":usage}}))
http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
