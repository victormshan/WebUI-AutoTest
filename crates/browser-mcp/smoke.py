"""Real stdio MCP -> Rust Runtime -> Chrome integration test (standard library only)."""
import http.server, json, os, pathlib, re, selectors, subprocess, threading, time
ROOT = pathlib.Path(__file__).resolve().parents[2]
class Fixture(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'''<!doctype html><title>MCP fixture</title><input id="name"><input id="hidden" hidden><button id="go" onclick="document.querySelector('#result').textContent='Hello '+document.querySelector('#name').value">Apply</button><p id="result">Ready</p>'''
        self.send_response(200); self.send_header('Content-Type','text/html; charset=utf-8'); self.end_headers(); self.wfile.write(body)
    def log_message(self, *args): pass
server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
threading.Thread(target=server.serve_forever, daemon=True).start()
p = subprocess.Popen([str(ROOT/'target/debug/browser-mcp'), '--chrome', os.environ.get('WEBTEST_CHROME', '/home/administrator/.local/bin/chrome-wsl')], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
sel = selectors.DefaultSelector(); sel.register(p.stdout, selectors.EVENT_READ)
sequence = 0
checks = 0
def send(v):
    p.stdin.write(json.dumps(v).encode()+b'\n'); p.stdin.flush()
def request(method, params):
    global sequence, checks
    sequence += 1
    send({'jsonrpc':'2.0', 'id':sequence, 'method':method, 'params':params})
    deadline=time.monotonic()+40
    while time.monotonic()<deadline:
        if not sel.select(max(0,deadline-time.monotonic())): break
        line=p.stdout.readline()
        if not line: raise AssertionError('MCP exited: '+p.stderr.read().decode())
        reply=json.loads(line)
        if reply.get('id')==sequence:
            checks+=1
            return reply
    raise AssertionError('MCP response timeout: '+method)
def call(name, args=None, error=False):
    reply=request('tools/call', {'name':name,'arguments':args or {}})
    if error:
        assert 'error' in reply or reply['result'].get('isError'), reply
        return reply
    assert 'error' not in reply and not reply['result'].get('isError'), reply
    return json.loads(reply['result']['content'][0]['text'])
try:
    init=request('initialize',{'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'browser-smoke','version':'1'}})
    assert init['result']['serverInfo']['name']=='rust-browser-runtime', init
    send({'jsonrpc':'2.0','method':'notifications/initialized'})
    tools=request('tools/list',{})['result']['tools']
    assert {t['name'] for t in tools}=={'browser_navigate','browser_snapshot','browser_click','browser_fill'}, tools
    call('missing_tool', error=True)
    call('browser_navigate', {}, error=True)
    call('browser_navigate', {'url':'file:///etc/passwd'}, error=True)
    call('browser_navigate', {'url':f'http://127.0.0.1:{server.server_port}/'})
    deadline=time.monotonic()+10
    while True:
        snapshot=call('browser_snapshot')
        if snapshot['title']=='MCP fixture' and snapshot['ready_state']=='complete': break
        assert time.monotonic()<deadline, snapshot
        time.sleep(.05)
    call('browser_click', {'selector':'#missing'}, error=True)
    call('browser_click', {'selector':'input'}, error=True)
    call('browser_fill', {'selector':'#hidden','value':'x'}, error=True)
    call('browser_fill', {'selector':'#go','value':'x'}, error=True)
    value='Alice " \\ ${x} </script>'
    call('browser_fill', {'selector':'#name','value':value})
    call('browser_click', {'selector':'#go'})
    assert 'Hello '+value in call('browser_snapshot')['text']
    # Identify this server's own Chrome profile, then verify EOF removes it and Chrome.
    children=set()
    for task in pathlib.Path(f'/proc/{p.pid}/task').iterdir():
        children.update((task/'children').read_text().split())
    chrome_pid=None; profile=None; observed=[]
    for child in children:
        args=pathlib.Path(f'/proc/{child}/cmdline').read_bytes().split(bytes([0]))
        observed.append(args)
        # Chrome rewrites argv into a single process-title string on this build.
        match=re.search(rb'--user-data-dir=(/tmp/browser-mcp-[^ /]+/chrome)', b' '.join(args))
        if match:
            chrome_pid=int(child); profile=pathlib.Path(os.fsdecode(match[1])).parent
    assert chrome_pid and profile and profile.exists(), observed
    if os.environ.get('BROWSER_MCP_SMOKE_SHUTDOWN') == 'sigterm':
        p.terminate()
    else:
        p.stdin.close()
    assert p.wait(timeout=20)==0, p.stderr.read().decode()
    assert not profile.exists(), profile
    assert not pathlib.Path(f'/proc/{chrome_pid}').exists(), chrome_pid
    print(f'PASS: {checks} MCP exchanges; initialize, tools, errors, real navigation/fill/click/observation, Chrome/profile cleanup via {os.environ.get("BROWSER_MCP_SMOKE_SHUTDOWN", "eof")}')
finally:
    if p.poll() is None:
        p.stdin.close()
        p.terminate()
        try: p.wait(timeout=20)
        except subprocess.TimeoutExpired: p.kill(); p.wait(timeout=5)
    server.shutdown(); server.server_close()
