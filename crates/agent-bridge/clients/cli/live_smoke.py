#!/usr/bin/env python3
"""Opt-in real CLI smoke test on a new private bridge, never the installed service.
Uses the user's existing CLI logins/models. Calls models and consumes their normal quota.
"""
import argparse
import importlib.util
import json
import os
import secrets
import socket
import subprocess
import tempfile
import time
from pathlib import Path

spec = importlib.util.spec_from_file_location('bridge_agents', Path(__file__).with_name('bridge_agents.py'))
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--bridge-bin', required=True)
    p.add_argument('--codex-bin', required=True)
    p.add_argument('--opencode-bin', default='opencode')
    p.add_argument('--evidence', required=True)
    p.add_argument('--only-opencode', action='store_true', help='Use a deterministic Codex protocol fixture; do not invoke Codex CLI')
    p.add_argument('--approve-bridge-tools', action='store_true', help='Explicitly approve local Codex bridge task-routing tools')
    args = p.parse_args()
    out = Path(args.evidence).resolve()
    out.mkdir(parents=True, exist_ok=False, mode=0o700)
    workspace = out / 'workspace'
    workspace.mkdir()
    with tempfile.TemporaryDirectory(prefix='bridge-live-') as temporary:
        tmp = Path(temporary)
        for agent in ('codex', 'opencode'):
            client.register(tmp, agent)
        (tmp/'notify.json').write_text('{}')
        with socket.socket() as s:
            s.bind(('127.0.0.1', 0))
            port = s.getsockname()[1]
        url = f'http://127.0.0.1:{port}'
        service = subprocess.Popen([args.bridge_bin, 'serve', '--listen', f'127.0.0.1:{port}',
            '--state', str(tmp/'state'), '--agents', str(tmp/'agents.json'), '--notify', str(tmp/'notify.json')],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                try:
                    socket.create_connection(('127.0.0.1', port), timeout=.2).close()
                    break
                except OSError:
                    time.sleep(.1)
            else:
                raise RuntimeError('private service did not start')
            clients = {a: client.Bridge(url, tmp/(a+'.token'), a) for a in ('codex', 'opencode')}
            configurations = {a: argparse.Namespace(client=a, agent=a, url=url, token_file=str(tmp/(a+'.token')),
                bridge_bin=args.bridge_bin, client_bin=args.codex_bin if a=='codex' else args.opencode_bin,
                workdir=str(workspace), model=None, timeout=180, wait=0, retry_task=[],
                approve_bridge_tools=args.approve_bridge_tools and a=='codex') for a in clients}
            reports=[]
            for sender, receiver in [('opencode', 'codex'), ('codex', 'opencode')]:
                task=sender+'-to-'+receiver
                nonce=secrets.token_hex(8)
                body=f'Private bridge smoke test. Compute 17 + 25. Reply using bridge_post result with outcome done, results item containing evidence with the sum and this exact nonce: {nonce}. Do not run shell commands, read files, change code, or contact other services. Only use this private agent_bridge MCP connection.'
                prompt=f'Use only agent_bridge MCP tools. Verify bridge_identity says {sender}. Send a new task via bridge_send with id {task}, to {receiver}, title private collaboration check, and body: {body}. Finish after sending; do not execute the recipient task yourself. No user decisions are required.'
                print('Sending '+('Codex fixture' if args.only_opencode and sender=='codex' else 'real native')+' task:', task, flush=True)
                if args.only_opencode and sender=='codex':
                    clients[sender].request('POST','/v1/tasks',{'id':task,'kind':'task','body':body,'meta':{'to':receiver,'title':'private fixture request'}})
                    sent={'ok':True,'mode':'deterministic Codex fixture'}
                else:
                    sent=client.launch(configurations[sender], prompt)
                client.write_private(out/(task+'-send.json'), sent)
                if not sent['ok']:
                    raise RuntimeError(task+' sender CLI failed; see evidence')
                created=clients[sender].request('GET','/v1/tasks/'+task)
                assert created['task']['from']==sender and created['task']['to']==receiver
                state=out/(receiver+'-worker')
                state.mkdir(exist_ok=True)
                print('Handling task with '+('protocol fixture' if args.only_opencode and receiver=='codex' else 'real receiver')+':', receiver, flush=True)
                if args.only_opencode and receiver=='codex':
                    clients[receiver].post(task,{'kind':'ack','body':'deterministic Codex fixture'})
                    clients[receiver].post(task,{'kind':'result','outcome':'done','results':[{'item':'fixture','status':'done','evidence':'42 '+nonce}]})
                elif not client.worker_once(configurations[receiver], clients[receiver], state):
                    raise RuntimeError(receiver+' worker failed; inspect journal')
                result=clients[sender].request('GET','/v1/tasks/'+task)
                replies=[m for m in result['messages'] if m['kind']=='result' and m['from']==receiver]
                assert replies and replies[-1]['outcome']=='done', result
                proof=json.dumps(replies[-1],ensure_ascii=False)
                assert '42' in proof and nonce in proof, proof
                # Existing sender CLI session is not implicitly trusted as a verifier. Explicitly
                # ask it to check the concrete sum and nonce before posting a verdict/close.
                verify=f'Use only agent_bridge MCP tools. You are requester {sender} for {task}. Read bridge_identity and bridge_show. Independently check 17+25=42 and that receiver evidence includes nonce {nonce}. If both match, post verdict pass then close this task. Otherwise post verdict rework. Do not approve anything else.'
                print('Verifying and closing with '+('protocol fixture' if args.only_opencode and sender=='codex' else 'real requester')+':', sender, flush=True)
                if args.only_opencode and sender=='codex':
                    clients[sender].post(task,{'kind':'verdict','judgement':'pass','body':'deterministic fixture verification'})
                    clients[sender].post(task,{'kind':'close'})
                    checked={'ok':True,'mode':'deterministic Codex fixture'}
                else:
                    checked=client.launch(configurations[sender], verify)
                client.write_private(out/(task+'-verify.json'), checked)
                final=clients[sender].request('GET','/v1/tasks/'+task)
                client.write_private(out/(task+'-messages.json'), final)
                assert checked['ok'] and final['task']['state']=='closed', final
                reports.append({'from':sender,'to':receiver,'task':task,'state':'closed','sum':42,'nonce':nonce})
            client.write_private(out/'result.json',{'passed':True,'private_service':url,'tasks':reports,'production_service_changed':False,'codex_native':not args.only_opencode,'opencode_native':True})
            print(json.dumps({'passed':True,'codex_native':not args.only_opencode,'opencode_native':True,'tasks':reports,'evidence':str(out)},ensure_ascii=False,indent=2),flush=True)
        finally:
            service.terminate()
            service.wait(timeout=10)


if __name__=='__main__':main()
