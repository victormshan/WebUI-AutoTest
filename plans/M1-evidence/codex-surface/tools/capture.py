#!/usr/bin/env python3
"""Fake Responses API: saves every request body, answers with one assistant message."""
import json, sys, http.server, itertools
out, port = sys.argv[1], int(sys.argv[2])
seq = itertools.count(1)
def sse(ev, data): return f"event: {ev}\ndata: {json.dumps(data)}\n\n".encode()
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        n = next(seq); open(f"{out}/req{n:02d}-GET.txt","w").write(self.path)
        body = b'{"object":"list","data":[]}' if "models" in self.path else b'{}'
        self.send_response(200 if "models" in self.path else 404); self.send_header("content-type","application/json"); self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        n = next(seq); raw = self.rfile.read(int(self.headers.get("content-length",0)))
        open(f"{out}/req{n:02d}-POST{self.path.replace('/','_')}.json","wb").write(raw)
        self.send_response(200); self.send_header("content-type","text/event-stream"); self.end_headers()
        usage={"input_tokens":1,"input_tokens_details":{"cached_tokens":0},"output_tokens":1,"output_tokens_details":{"reasoning_tokens":0},"total_tokens":2}
        self.wfile.write(sse("response.created",{"type":"response.created","response":{"id":"r1"}}))
        self.wfile.write(sse("response.output_item.done",{"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","id":"m1","content":[{"type":"output_text","text":"CAPTURED"}]}}))
        self.wfile.write(sse("response.completed",{"type":"response.completed","response":{"id":"r1","usage":usage}}))
http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
