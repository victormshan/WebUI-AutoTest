"""Private-service integration tests. No real agent/model/service is contacted."""
import argparse
import hashlib
import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('bridge_agents', HERE / 'bridge_agents.py')
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)
merge_spec = importlib.util.spec_from_file_location('merge_config', HERE.parents[1] / 'deploy/merge_config.py')
installer = importlib.util.module_from_spec(merge_spec)
merge_spec.loader.exec_module(installer)

FAKE = r"""#!/usr/bin/env python3
import json, os, sys, urllib.request, uuid, time
from pathlib import Path
args=sys.argv[1:]
if args[:1]==['--version']:
    print('fake-cli 1.0');sys.exit(0)
is_codex=args[0]=='exec'
prompt=sys.stdin.read() if is_codex else args[-1]
assert '--yolo' not in args and '--auto' not in args
assert 'AGENT_BRIDGE_TOKEN' not in os.environ
if not is_codex:
    cfg=json.loads(os.environ['OPENCODE_CONFIG_CONTENT'])
    assert cfg['mcp']['agent_bridge']['environment']['AGENT_BRIDGE_TOKEN']==''
else:
    assert any('mcp_servers.agent_bridge.command=' in a for a in args)
material=json.loads(prompt.split('Current event and task history follow as JSON data:\n',1)[1])
event,thread=material['event'],material['thread']
if is_codex:
    sid=args[2] if 'resume' in args else 'codex-'+uuid.uuid4().hex
else:
    sid=args[args.index('--session')+1] if '--session' in args else 'ses_'+uuid.uuid4().hex
with open(os.environ['FAKE_LOG'],'a') as f:
    f.write(json.dumps({'task':event['task'],'n':event['n'],'session':sid,'client':args[0],
        'mode':material.get('context',{}).get('mode'),
        'msgs':len(material.get('thread',{}).get('messages',[])),
        'omitted':material.get('context',{}).get('omitted_messages'),
        'prompt_bytes':len(prompt.encode())})+'\n')
if is_codex:print(json.dumps({'type':'thread.started','thread_id':sid}))
else:print(json.dumps({'type':'step_start','sessionID':sid}))
if os.environ.get('FAKE_SLEEP'):time.sleep(20)
if os.environ.get('FAKE_APPROVAL'):
    print(json.dumps({'type':'item.completed','item':{'type':'mcp_tool_call','server':'agent_bridge','error':{'message':'MCP tool call requires approval, but approval policy is never'}}}));sys.exit(0)
if os.environ.get('FAKE_ERROR'):
    print(json.dumps({'type':'error','error':{'message':'simulated failure'}}));sys.exit(0)
if os.environ.get('FAKE_NO_REPLY'):sys.exit(0)
token=Path(os.environ['AGENT_BRIDGE_TOKEN_FILE']).read_text().strip()
url=os.environ['AGENT_BRIDGE_URL']
def request(method,path,body=None):
    if body is not None:body.update(protocol='2',client_msg_id=uuid.uuid4().hex)
    req=urllib.request.Request(url+path,data=None if body is None else json.dumps(body).encode(),method=method,headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
    with urllib.request.urlopen(req) as response:return json.load(response)
me=request('GET','/v1/identity')['agent']
def post(body):return request('POST','/v1/tasks/'+event['task']+'/messages',body)
if event['kind']=='question':post({'kind':'answer','body':'verified fixture answer'})
elif event['kind']=='result':
    post({'kind':'verdict','judgement':'pass','body':'fixture verifier'})
    post({'kind':'close'})
elif os.environ.get('FAKE_QUESTION'):
    post({'kind':'question','body':'clarify fixture','questions':[{'id':'q','text':'fixture?','blocking':True}]})
elif os.environ.get('FAKE_DELEGATE'):
    request('POST','/v1/tasks',{'id':event['task']+'-child','kind':'task','body':'derived fixture','meta':{'to':'opencode' if me=='codex' else 'codex','title':'child','parent':event['task']}})
    post({'kind':'progress','body':'awaiting derived task'})
else:post({'kind':'result','outcome':'done','results':[{'item':'fixture','status':'done','evidence':'fake CLI protocol exercised'}]})
"""


class Harness(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.env = patch.dict(os.environ, {'FAKE_LOG': str(self.root/'calls.jsonl'), 'AGENT_BRIDGE_TOKEN':'wrong-parent-token', 'XDG_STATE_HOME': str(self.root/'xdg')})
        self.env.start()
        for k in ('FAKE_ERROR', 'FAKE_NO_REPLY', 'FAKE_QUESTION', 'FAKE_DELEGATE', 'FAKE_SLEEP', 'FAKE_APPROVAL'):
            os.environ.pop(k,None)
        cfg = self.root/'config'
        for agent in ('claude','dsh','codex','opencode'):
            client.register(cfg,agent)
        (self.root/'notify.json').write_text('{}')
        with socket.socket() as sock:
            sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
        self.url=f'http://127.0.0.1:{port}'
        self.binary=os.environ.get('TEST_BRIDGE_BIN','agent-bridge')
        self.server=subprocess.Popen([self.binary,'serve','--state',str(self.root/'service'),
            '--listen',f'127.0.0.1:{port}','--agents',str(cfg/'agents.json'),'--notify',str(self.root/'notify.json')],
            stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        self.addCleanup(self.cleanup)
        for _ in range(100):
            try:
                socket.create_connection(('127.0.0.1',port),timeout=.2).close();break
            except OSError:time.sleep(.02)
        else:raise RuntimeError('private bridge did not start')
        self.agents={a:client.Bridge(self.url,cfg/(a+'.token'),a) for a in ('claude','dsh','codex','opencode')}
        self.fake=self.root/'fake-cli';self.fake.write_text(FAKE);self.fake.chmod(0o755)

    def cleanup(self):
        self.server.terminate();self.server.wait(timeout=10)
        self.env.stop();self.tmp.cleanup()

    def args(self,agent):
        return argparse.Namespace(client=agent,agent=agent,url=self.url,
            token_file=str(self.root/'config'/(agent+'.token')),bridge_bin=self.binary,
            client_bin=str(self.fake),workdir=str(self.root),model=None,timeout=10,wait=0,retry_task=[])

    def send(self,who,to,task):
        return self.agents[who].request('POST','/v1/tasks',{'id':task,'kind':'task','body':'fixture','meta':{'to':to,'title':task}})

    def poll(self,agent):
        state=self.root/('worker-'+agent);state.mkdir(exist_ok=True)
        return client.worker_once(self.args(agent),self.agents[agent],state)

    def calls(self):
        p=self.root/'calls.jsonl'
        return [json.loads(line) for line in p.read_text().splitlines()] if p.exists() else []

    def _msg(self,n,frm,kind,body='x',**kw):
        m={'task':'t','n':n,'from':frm,'kind':kind,'body':body}
        m.update(kw);return m

    def doctor_args(self,**kw):
        base=dict(client='codex',agent='codex',url=self.url,
                  token_file=str(self.root/'config/codex.token'),bridge_bin=self.binary,
                  client_bin=str(self.fake),workdir=str(self.root))
        base.update(kw)
        return argparse.Namespace(**base)


class ConnectorTest(Harness):
    def test_native_cli_workers_close_tasks_in_both_directions(self):
        for sender,receiver in [('codex','opencode'),('opencode','codex')]:
            task=sender+'-to-'+receiver
            self.send(sender,receiver,task)
            self.assertTrue(self.poll(receiver))
            self.assertTrue(self.poll(sender))
            self.assertEqual(self.agents[sender].request('GET','/v1/tasks/'+task)['task']['state'],'closed')
        self.assertEqual(len(self.calls()),4)
        self.assertEqual(len({c['session'] for c in self.calls()}),4)

    def test_question_answer_and_per_task_session_resume(self):
        self.send('codex','opencode','question')
        os.environ['FAKE_QUESTION']='1';self.assertTrue(self.poll('opencode'))
        os.environ.pop('FAKE_QUESTION');self.assertTrue(self.poll('codex'))
        self.assertTrue(self.poll('opencode'))
        calls=[c for c in self.calls() if c['client']=='run']
        self.assertEqual(calls[0]['session'],calls[1]['session'])
        self.assertTrue(self.poll('codex'))
        self.assertEqual(self.agents['codex'].request('GET','/v1/tasks/question')['task']['state'],'closed')

    def test_derived_task_routes_without_impersonating_requester(self):
        self.send('claude','codex','parent')
        os.environ['FAKE_DELEGATE']='1';self.assertTrue(self.poll('codex'));os.environ.pop('FAKE_DELEGATE')
        child=self.agents['opencode'].request('GET','/v1/tasks/parent-child')['task']
        self.assertEqual((child['from'],child['to'],child['parent']),('codex','opencode','parent'))
        self.assertTrue(self.poll('opencode'));self.assertTrue(self.poll('codex'))
        self.assertEqual(self.agents['codex'].request('GET','/v1/tasks/parent-child')['task']['state'],'closed')
        self.assertEqual(self.agents['claude'].request('GET','/v1/tasks/parent')['task']['state'],'working')

    def test_interruption_after_ack_is_not_automatically_rerun(self):
        self.send('claude','codex','interrupt')
        os.environ['FAKE_ERROR']='1';self.assertFalse(self.poll('codex'))
        self.assertFalse(self.poll('codex'));self.assertEqual(len(self.calls()),1)
        os.environ.pop('FAKE_ERROR')
        args=self.args('codex');args.retry_task=['interrupt']
        self.assertTrue(client.worker_once(args,self.agents['codex'],self.root/'worker-codex'))
        self.assertEqual(len(self.calls()),2)
        ms=self.agents['claude'].request('GET','/v1/tasks/interrupt')['messages']
        self.assertEqual([m['kind'] for m in ms],['task','ack','result'])

    def test_cli_success_without_protocol_reply_is_not_task_success(self):
        self.send('dsh','opencode','silent')
        os.environ['FAKE_NO_REPLY']='1';self.assertFalse(self.poll('opencode'))
        self.assertEqual(self.agents['dsh'].request('GET','/v1/tasks/silent')['task']['state'],'acked')
        self.assertFalse(self.poll('opencode'));self.assertEqual(len(self.calls()),1)

    def test_done_jobs_do_not_run_twice_and_state_files_are_private(self):
        self.send('claude','codex','once');self.assertTrue(self.poll('codex'));self.assertTrue(self.poll('codex'))
        self.assertEqual(len(self.calls()),1)
        for path in (self.root/'worker-codex').rglob('*.json'):
            self.assertEqual(path.stat().st_mode & 0o777,0o600)

    def test_timeout_is_uncertain_and_does_not_replay_automatically(self):
        self.send('claude','codex','timeout')
        # 3 s: long enough for the fake client to start and record the call on a loaded machine
        # (0.15 s was outrun on 2026-10-27), still far below its 20 s sleep.
        args=self.args('codex');args.timeout=3
        os.environ['FAKE_SLEEP']='1'
        state=self.root/'worker-codex';state.mkdir()
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)
        self.assertIn('timeout',json.loads((state/'timeout-1-output.json').read_text())['error'])

    def test_native_mcp_approval_failure_is_reported_without_overrides(self):
        self.send('claude','codex','approval')
        os.environ['FAKE_APPROVAL']='1'
        self.assertFalse(self.poll('codex'))
        output=json.loads((self.root/'worker-codex/approval-1-output.json').read_text())
        self.assertFalse(output['ok'])
        self.assertIn('approval required',output['error'])
        self.assertEqual(self.agents['claude'].request('GET','/v1/tasks/approval')['task']['state'],'acked')

    def test_peer_input_arriving_during_cli_turn_is_not_lost_or_replayed(self):
        self.send('claude','codex','race')
        real_launch=client.launch
        def with_late_note(*args,**kwargs):
            self.agents['claude'].post('race',{'kind':'note','reply_to':1,'body':'late input','wake':'normal'})
            return real_launch(*args,**kwargs)
        with patch.object(client,'launch',side_effect=with_late_note):
            self.assertFalse(self.poll('codex'))
        self.assertFalse(self.poll('codex'))
        self.assertEqual(len(self.calls()),1)
        jobs=list((self.root/'worker-codex/jobs').glob('*.json'))
        pending=[json.loads(p.read_text()) for p in jobs if json.loads(p.read_text())['status']=='uncertain']
        self.assertEqual(len(pending),1)
        self.assertEqual(pending[0]['event']['body'],'late input')

    def test_old_reply_does_not_count_as_a_new_protocol_response(self):
        self.send('claude','codex','stale')
        os.environ['FAKE_DELEGATE']='1'
        self.assertTrue(self.poll('codex'))
        os.environ.pop('FAKE_DELEGATE')
        self.agents['claude'].post('stale',{'kind':'note','reply_to':1,'body':'new input','wake':'normal'})
        os.environ['FAKE_NO_REPLY']='1'
        self.assertFalse(self.poll('codex'))
        self.assertEqual(self.agents['claude'].request('GET','/v1/tasks/stale')['task']['state'],'working')

    def test_cancelled_uncertain_turn_is_reconciled_without_rerun(self):
        self.send('claude','codex','cancelled')
        os.environ['FAKE_ERROR']='1'
        self.assertFalse(self.poll('codex'))
        self.agents['claude'].post('cancelled',{'kind':'cancel'})
        self.assertTrue(self.poll('codex'))
        self.assertEqual(len(self.calls()),1)
        journal=json.loads((self.root/'worker-codex/jobs/cancelled-1.json').read_text())
        self.assertEqual(journal['status'],'done')

    def test_collaboration_alias_does_not_replace_existing_codex_reviewer(self):
        cfg=self.root/'config'
        old=(cfg/'codex.token').read_text()
        alias=client.register(cfg,'codex-collab')
        self.assertNotEqual(alias.read_text(),old)
        self.assertEqual((cfg/'codex.token').read_text(),old)
        self.assertIn('codex-collab',json.loads((cfg/'agents.json').read_text()))

    def test_registration_is_idempotent_preserves_others_and_refuses_rotation(self):
        cfg=self.root/'config';before=json.loads((cfg/'agents.json').read_text())
        old=(cfg/'codex.token').read_text();client.register(cfg,'codex')
        self.assertEqual((cfg/'codex.token').read_text(),old)
        self.assertEqual(json.loads((cfg/'agents.json').read_text()),before)
        (cfg/'codex.token').write_text('different-token-0123456789abcdef0123456789')
        with self.assertRaises(ValueError):client.register(cfg,'codex')
        self.assertEqual(json.loads((cfg/'agents.json').read_text()),before)

    def test_wrong_identity_and_duplicate_worker_are_rejected(self):
        wrong=client.Bridge(self.url,self.root/'config/claude.token','codex')
        with self.assertRaises(RuntimeError):wrong.identity()
        with client.lock(self.root/'lock'):
            with self.assertRaises(RuntimeError):
                with client.lock(self.root/'lock'):pass

    def test_client_configs_preserve_user_settings_and_do_not_leak_tokens(self):
        args=self.args('opencode')
        os.environ['OPENCODE_CONFIG_CONTENT']=json.dumps({'model':'custom/example','mcp':{'other':{'enabled':True}}})
        cmd,env,stdin=client.build_launch(args,'hello')
        cfg=json.loads(env['OPENCODE_CONFIG_CONTENT'])
        self.assertEqual(cfg['model'],'custom/example');self.assertIn('other',cfg['mcp'])
        self.assertIsNone(stdin);self.assertNotIn('AGENT_BRIDGE_TOKEN',env)
        token=Path(args.token_file).read_text().strip()
        self.assertNotIn(token,json.dumps(cfg));self.assertNotIn(token,' '.join(cmd))
        cmd,env,stdin=client.build_launch(self.args('codex'),'hello','thread-existing')
        self.assertEqual(cmd[1:4],['exec','resume','thread-existing']);self.assertEqual(stdin,'hello')
        self.assertNotIn('--yolo',cmd)

    def test_legacy_installer_keeps_new_agents_and_notify_targets(self):
        cfg=self.root/'config'
        # Legacy installer calls Claude's file 'token'.
        (cfg/'token').write_text((cfg/'claude.token').read_text())
        (cfg/'notify.json').write_text(json.dumps({'opencode':{'url':'http://fixture/notify'},'dsh':{'url':'http://custom/notify'}}))
        before=json.loads((cfg/'agents.json').read_text())
        installer.merge(cfg)
        self.assertEqual(json.loads((cfg/'agents.json').read_text()),before)
        self.assertEqual(json.loads((cfg/'notify.json').read_text())['dsh']['url'],'http://custom/notify')
        self.assertIn('opencode',json.loads((cfg/'notify.json').read_text()))


    def test_task_followed_by_quiet_note_still_executes_exactly_once(self):
        self.send('claude','codex','mixed')
        self.agents['claude'].post('mixed',{'kind':'note','reply_to':1,'body':'quiet tail','wake':'quiet'})
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        args=self.args('codex')
        self.assertTrue(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)
        # The task must really have been worked, not merely marked read behind the quiet note.
        self.assertNotEqual(self.agents['codex'].request('GET','/v1/tasks/mixed')['task']['state'],'new')
        self.assertTrue(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)

    def test_result_followed_by_quiet_input_still_triggers_requester(self):
        self.agents['codex'].request('POST','/v1/tasks',{'id':'rq','kind':'task','body':'fixture','meta':{'to':'opencode','title':'rq'}})
        self.agents['opencode'].post('rq',{'kind':'ack','body':'ack fixture'})
        self.agents['opencode'].post('rq',{'kind':'result','outcome':'done','results':[{'item':'i','status':'done','evidence':'e'}]})
        self.agents['opencode'].post('rq',{'kind':'note','reply_to':3,'body':'quiet after result','wake':'quiet'})
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        self.assertTrue(client.worker_once(self.args('codex'),self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)
        self.assertEqual(self.agents['codex'].request('GET','/v1/tasks/rq')['task']['state'],'closed')

    def test_quiet_only_input_never_launches(self):
        self.send('claude','codex','qonly')
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        self.assertTrue(client.worker_once(self.args('codex'),self.agents['codex'],state))
        before=len(self.calls())
        self.agents['claude'].post('qonly',{'kind':'note','reply_to':1,'body':'quiet status','wake':'quiet'})
        self.assertTrue(client.worker_once(self.args('codex'),self.agents['codex'],state))
        self.assertEqual(len(self.calls()),before)

    def test_multiple_actionable_events_coalesce_into_one_turn(self):
        self.send('claude','codex','multi')
        self.agents['codex'].post('multi',{'kind':'ack','body':'ack fixture'})
        self.agents['claude'].post('multi',{'kind':'note','reply_to':1,'body':'first steer','wake':'normal'})
        self.agents['claude'].post('multi',{'kind':'note','reply_to':1,'body':'second steer','wake':'normal'})
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        self.assertTrue(client.worker_once(self.args('codex'),self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)

    def test_uncertain_job_does_not_rerun_without_authorization(self):
        self.send('claude','codex','uncertain')
        os.environ['FAKE_ERROR']='1'
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        args=self.args('codex')
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)
        self.assertEqual(json.loads((state/'jobs/uncertain-1.json').read_text())['status'],'uncertain')

    def test_one_use_retry_authorization_across_polls(self):
        self.send('claude','codex','retry')
        os.environ['FAKE_ERROR']='1'
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        args=self.args('codex')
        # First failure is uncertain and is not retried on its own.
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)
        # One explicit authorization runs the retry exactly once, even though it fails again.
        args.retry_task=['retry']
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),2)
        # Later polls with the same args must not relaunch the still-uncertain job.
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),2)
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),2)
        # A genuinely new explicit authorization (manual recovery) is still honoured once.
        fresh=self.args('codex');fresh.retry_task=['retry']
        self.assertFalse(client.worker_once(fresh,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),3)


    def test_note_on_fresh_task_posts_initial_ack_and_reaches_a_real_state(self):
        # No fixture ack: a normal note before any ack must still make the worker post the
        # receiver's initial ack, so the model's result is accepted and the task really moves.
        self.send('claude','codex','fresh')
        self.agents['claude'].post('fresh',{'kind':'note','reply_to':1,'body':'start here','wake':'normal'})
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        self.assertTrue(client.worker_once(self.args('codex'),self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)
        thread=self.agents['claude'].request('GET','/v1/tasks/fresh')
        self.assertEqual([m['kind'] for m in thread['messages']],['task','note','ack','result'])
        self.assertEqual(thread['task']['state'],'awaiting_verdict')

    def test_retry_flag_before_first_failure_does_not_authorize_it(self):
        self.send('claude','codex','preauth')
        os.environ['FAKE_ERROR']='1'
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        args=self.args('codex');args.retry_task=['preauth']
        # The flag names a task that is not uncertain yet: it must expire without authorizing the
        # failure this poll itself produces, so no later poll reruns it.
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)
        self.assertEqual(args.retry_task,[])
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)


    # ---- item 1: cross-state-dir identity exclusion --------------------------------

    def test_identity_lock_is_cross_process_and_per_identity_or_server(self):
        ready=self.root/'lock-ready'
        script=("import importlib.util,sys,time\n"
                "from pathlib import Path\n"
                "spec=importlib.util.spec_from_file_location('b',sys.argv[1])\n"
                "m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)\n"
                "with m.identity_lock(sys.argv[2],sys.argv[3]):\n"
                "    Path(sys.argv[4]).write_text('ready')\n"
                "    time.sleep(30)\n")
        child=subprocess.Popen([sys.executable,'-c',script,str(HERE/'bridge_agents.py'),
                                self.url,'codex',str(ready)],env=os.environ.copy())
        self.addCleanup(lambda:(child.terminate(),child.wait(timeout=10)))
        for _ in range(300):
            if ready.exists():break
            time.sleep(.02)
        else:self.fail('child process did not acquire the identity lock')
        # A true separate process holds the same identity: this one must be refused even
        # though its state-dir/client are irrelevant to the lock.
        with self.assertRaises(RuntimeError):
            with client.identity_lock(self.url,'codex'):pass
        # Different identity and different server get different locks and coexist.
        with client.identity_lock(self.url,'opencode'):pass
        with client.identity_lock('http://127.0.0.1:1','codex'):pass
        # Releasing on exit frees the identity.
        child.terminate();child.wait(timeout=10)
        with client.identity_lock(self.url,'codex'):pass

    def test_worker_entry_point_honours_the_identity_lock(self):
        # The worker command itself must refuse to start while another instance holds the
        # identity, even with a brand-new state-dir.
        state=self.root/'other-state'
        with client.identity_lock(self.url,'codex'):
            r=subprocess.run([sys.executable,str(HERE/'bridge_agents.py'),'worker','--client','codex',
                '--agent','codex','--url',self.url,'--token-file',str(self.root/'config/codex.token'),
                '--bridge-bin',self.binary,'--client-bin',str(self.fake),'--workdir',str(self.root),
                '--state-dir',str(state),'--once','--wait','0'],
                capture_output=True,text=True,env=os.environ.copy(),timeout=30)
        self.assertNotEqual(r.returncode,0)
        self.assertIn('already consumes identity',r.stderr)
        self.assertEqual(len(self.calls()),0)


    # ---- item 2: cancellation while the native CLI runs ----------------------------

    def test_cancel_during_cli_stops_owned_group_and_reconciles_without_result(self):
        self.send('claude','codex','cancelrun')
        os.environ['FAKE_SLEEP']='1'
        args=self.args('codex');args.cancel_poll=.2;args.timeout=15
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        calls_file=self.root/'calls.jsonl'
        def canceller():
            for _ in range(500):
                if calls_file.exists() and calls_file.read_text().strip():break
                time.sleep(.02)
            self.agents['claude'].post('cancelrun',{'kind':'cancel'})
        thread=threading.Thread(target=canceller);thread.start()
        start=time.monotonic()
        ok=client.worker_once(args,self.agents['codex'],state)
        elapsed=time.monotonic()-start
        thread.join(timeout=5)
        self.assertTrue(ok)
        self.assertLess(elapsed,10)  # stopped promptly, not after the fixture's 20s sleep
        self.assertEqual(len(self.calls()),1)
        journal=json.loads((state/'jobs/cancelrun-1.json').read_text())
        self.assertEqual(journal['status'],'done')
        self.assertIn('reconcil',json.dumps(journal))
        thread_view=self.agents['claude'].request('GET','/v1/tasks/cancelrun')
        self.assertNotIn('result',[m['kind'] for m in thread_view['messages']])
        self.assertEqual(thread_view['task']['state'],'cancelled')


    # ---- item 3: read-only doctor ---------------------------------------------------

    def test_doctor_healthy_is_structured_and_secret_free(self):
        report=client.doctor(self.doctor_args())
        self.assertTrue(report['ok'])
        names={c['name'] for c in report['checks']}
        for expected in ('bridge_binary','client_binary','token_file','server','identity','mcp_config','approval_gating'):
            self.assertIn(expected,names)
        token=(self.root/'config/codex.token').read_text().strip()
        self.assertNotIn(token,json.dumps(report))
        approval=[c for c in report['checks'] if c['name']=='approval_gating'][0]
        self.assertIn('approval',approval['detail'].lower())
        # Doctor is read-only: the task inbox/cursor is untouched (it never consumes it).
        self.assertTrue(report['checks'][0]['ok'])

    def test_doctor_missing_executable_fails(self):
        report=client.doctor(self.doctor_args(bridge_bin=str(self.root/'does-not-exist')))
        self.assertFalse(report['ok'])
        self.assertFalse([c for c in report['checks'] if c['name']=='bridge_binary'][0]['ok'])

    def test_doctor_identity_mismatch_fails(self):
        report=client.doctor(self.doctor_args(agent='opencode',
                  token_file=str(self.root/'config/codex.token')))
        self.assertFalse(report['ok'])
        self.assertFalse([c for c in report['checks'] if c['name']=='identity'][0]['ok'])
        token=(self.root/'config/codex.token').read_text().strip()
        self.assertNotIn(token,json.dumps(report))

    def test_doctor_unavailable_server_fails_without_crashing(self):
        with socket.socket() as s:
            s.bind(('127.0.0.1',0));port=s.getsockname()[1]
        report=client.doctor(self.doctor_args(url=f'http://127.0.0.1:{port}'))
        self.assertFalse(report['ok'])
        self.assertFalse([c for c in report['checks'] if c['name']=='server'][0]['ok'])


    # ---- item 4: bounded/incremental task context ----------------------------------

    def test_context_new_session_bounds_and_keeps_original_task(self):
        thread={'task':{'id':'t','state':'open','last_n':50},
                'messages':[self._msg(1,'claude','task','ORIGINAL')]+
                           [self._msg(i,'claude','note','y'*5000) for i in range(2,51)]}
        event=self._msg(50,'claude','note','trigger')
        material=client.build_context(thread,event,None,None,budget=8000)
        self.assertEqual(material['context']['mode'],'new')
        self.assertEqual(material['original_task']['body'],'ORIGINAL')
        self.assertLessEqual(client._size(material),8000)
        self.assertTrue(material['context']['truncated'])
        self.assertEqual(material['event']['body'],'trigger')

    def test_context_resume_carries_only_peer_delta(self):
        thread={'task':{'id':'t','state':'open','last_n':5},
                'messages':[self._msg(1,'claude','task','t1'),self._msg(2,'codex','ack','a'),
                            self._msg(3,'claude','note','new peer'),self._msg(4,'codex','progress','mine'),
                            self._msg(5,'claude','note','trigger')]}
        event=self._msg(5,'claude','note','trigger')
        material=client.build_context(thread,event,'ses_1',{'session':'ses_1','delivered_n':2},
                                      budget=8000,agent='codex')
        self.assertEqual(material['context']['mode'],'resume')
        self.assertEqual([m['n'] for m in material['thread']['messages']],[3])
        self.assertEqual(material['context']['baseline_n'],2)
        self.assertEqual(material['event']['body'],'trigger')

    def test_context_session_mismatch_recovers_original_task(self):
        thread={'task':{'id':'t','state':'open','last_n':3},
                'messages':[self._msg(1,'claude','task','ORIG'),self._msg(2,'claude','note','x'),
                            self._msg(3,'claude','note','trigger')]}
        material=client.build_context(thread,self._msg(3,'claude','note','trigger'),'ses_new',
                                      {'session':'ses_old','delivered_n':2},budget=8000)
        self.assertEqual(material['context']['mode'],'new')
        self.assertEqual(material['original_task']['body'],'ORIG')

    def test_context_unicode_budget_preserves_event_and_points_to_bridge_show(self):
        big='漢'*8000
        thread={'task':{'id':'t'},'messages':[self._msg(1,'claude','task',big)]}
        event=self._msg(1,'claude','task',big)
        material=client.build_context(thread,event,'s',{'session':'s','delivered_n':0},
                                      budget=2000,agent='codex')
        self.assertLessEqual(client._size(material),2000)
        self.assertTrue(material['event']['body'])
        self.assertTrue(material['event']['body_truncated'])
        self.assertIn('bridge_show',material['context']['full_thread'])
        json.dumps(material,ensure_ascii=False)  # never splits a code point

    def test_worker_resume_sends_bounded_delta_not_full_history(self):
        self.send('claude','codex','incr')
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        args=self.args('codex')
        os.environ['FAKE_QUESTION']='1'
        self.assertTrue(client.worker_once(args,self.agents['codex'],state))
        os.environ.pop('FAKE_QUESTION')
        first=[c for c in self.calls() if c['task']=='incr'][-1]
        self.assertEqual(first['mode'],'new')
        # The receiver asked a question; the requester answers and the same session resumes.
        self.agents['claude'].post('incr',{'kind':'answer','body':'answer steer'})
        self.assertTrue(client.worker_once(args,self.agents['codex'],state))
        calls=[c for c in self.calls() if c['task']=='incr']
        self.assertEqual(len(calls),2)
        self.assertEqual(calls[1]['mode'],'resume')
        self.assertEqual(calls[1]['session'],calls[0]['session'])
        self.assertLess(calls[1]['msgs'],calls[0]['msgs'])
        # The full thread stays available through the bridge while the prompt was bounded.
        full=self.agents['codex'].request('GET','/v1/tasks/incr')['messages']
        self.assertGreater(len(full),calls[1]['msgs'])


    def test_launch_reads_large_stdout_and_stderr_without_deadlock(self):
        script=self.root/'big-cli'
        script.write_text('#!/usr/bin/env python3\n'
            'import sys,json\n'
            'sys.stdin.read()\n'
            'sys.stdout.write(json.dumps({"type":"thread.started","thread_id":"big"})+"\\n")\n'
            'sys.stdout.write("x"*(2*1024*1024)+"\\n")\n'
            'sys.stderr.write("e"*(2*1024*1024)+"\\n")\n')
        script.chmod(0o755)
        args=self.args('codex');args.client_bin=str(script);args.timeout=30
        result=client.launch(args,'prompt')
        self.assertTrue(result['ok'])
        self.assertEqual(result['session'],'big')
        self.assertGreater(len(result['stdout']),2*1024*1024)
        self.assertGreater(len(result['stderr']),2*1024*1024)


    def test_doctor_does_not_consume_inbox(self):
        self.send('claude','codex','docread')
        before=[m['n'] for m in self.agents['codex'].request('GET','/v1/inbox?wait=0')['messages']]
        client.doctor(self.doctor_args())
        after=[m['n'] for m in self.agents['codex'].request('GET','/v1/inbox?wait=0')['messages']]
        self.assertEqual(before,after)
        self.assertIn(self.agents['claude'].request('GET','/v1/tasks/docread')['task']['state'],('new','open'))


    # ---- item 5: extracted native client adapters ----------------------------------

    def test_adapters_parse_streamed_sessions_errors_and_approval(self):
        import bridge_adapters
        codex=bridge_adapters.CodexAdapter(argparse.Namespace(client='codex'))
        r=codex.parse_stream('not json\n'+json.dumps({'type':'thread.started','thread_id':'t1'}))
        self.assertEqual(r.session,'t1');self.assertFalse(r.failed)
        r=codex.parse_stream(json.dumps({'type':'turn.failed','error':{'message':'x'}}))
        self.assertTrue(r.failed)
        r=codex.parse_stream(json.dumps({'type':'item.completed','item':{'type':'mcp_tool_call',
            'server':'agent_bridge','error':{'message':'MCP tool call requires approval, but approval policy is never'}}}))
        self.assertTrue(r.approval_denied);self.assertTrue(r.failed)
        oc=bridge_adapters.OpenCodeAdapter(argparse.Namespace(client='opencode'))
        r=oc.parse_stream(json.dumps({'type':'step_start','sessionID':'ses_a'}))
        self.assertEqual(r.session,'ses_a')
        r=oc.parse_stream(json.dumps({'type':'error','error':{'message':'boom'}}))
        self.assertTrue(r.failed);self.assertIsNone(r.session)

    def test_adapters_build_launch_and_resume_commands(self):
        cmd,env,stdin=client.build_launch(self.args('codex'),'hi','thread-1')
        self.assertEqual(cmd[1:4],['exec','resume','thread-1']);self.assertEqual(stdin,'hi')
        cmd,env,stdin=client.build_launch(self.args('opencode'),'hi')
        self.assertEqual(cmd[1:3],['run','--format']);self.assertIsNone(stdin)
        cmd,env,stdin=client.build_launch(self.args('opencode'),'hi','ses_z')
        self.assertEqual(cmd[cmd.index('--session')+1],'ses_z')

    def test_worker_monitor_never_assumes_cancelled_when_service_errors(self):
        # A monitor that raises (service unavailable) must not be read as a cancellation.
        # 3 s: see test_timeout_is_uncertain_and_does_not_replay_automatically.
        args=self.args('codex');args.timeout=3;args.cancel_poll=.1
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        self.send('claude','codex','monitor-err')
        os.environ['FAKE_SLEEP']='1'
        original=client.task_monitor
        def broken(bridge,task):
            def check():raise RuntimeError('service unavailable')
            return check
        with patch.object(client,'task_monitor',broken):
            self.assertFalse(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(len(self.calls()),1)


def _http_response(code, body):
    payload=body.encode()
    reason={200:'OK',401:'Unauthorized',404:'Not Found'}.get(code,'ERR')
    return (f'HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\n'
            f'Content-Length: {len(payload)}\r\nConnection: close\r\n\r\n').encode()+payload


class FakeHTTP:
    """A bounded throwaway HTTP server for adversarial doctor/monitor probes."""
    def __init__(self,handler):
        self._sock=socket.socket()
        self._sock.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
        self._sock.bind(('127.0.0.1',0));self._sock.listen(8)
        self.port=self._sock.getsockname()[1];self._handler=handler
        self._thread=threading.Thread(target=self._serve,daemon=True);self._thread.start()
    def _serve(self):
        while True:
            try:conn,_=self._sock.accept()
            except OSError:return
            threading.Thread(target=self._handle,args=(conn,),daemon=True).start()
    def _handle(self,conn):
        try:
            data=b''
            while b'\r\n\r\n' not in data:
                chunk=conn.recv(4096)
                if not chunk:break
                data+=chunk
            self._handler(data.decode('latin1'),conn)
        except OSError:pass
        finally:
            try:conn.close()
            except OSError:pass
    def url(self):return f'http://127.0.0.1:{self.port}'
    def close(self):self._sock.close()


def _pid_dead(pid):
    try:
        state=Path(f'/proc/{pid}/stat').read_text().split(') ',1)[1].split(' ',1)[0]
    except (FileNotFoundError,IndexError,OSError):return True
    return state=='Z'


class ReworkTest(Harness):
    """Regression coverage for the review rework (items 1-6)."""

    # ---- item 1: canonical server URL ------------------------------------------------

    def test_canonical_url_normalizes_and_rejects_unsafe_spellings(self):
        self.assertEqual(client.canonical_url('HTTP://LocalHost:80/'),'http://localhost')
        self.assertEqual(client.canonical_url('https://EXAMPLE.com:443/x/'),'https://example.com/x')
        self.assertEqual(client.canonical_url('http://127.0.0.1:7879'),'http://127.0.0.1:7879')
        self.assertEqual(client.canonical_url('http://[::1]:80/'),'http://[::1]')
        for bad in ('http://user:pw@host/','http://host/?q=1','http://host/#f','ftp://host',
                    'http://','http://host:99999','','   '):
            with self.assertRaises(ValueError):
                client.canonical_url(bad)
        # Equivalent spellings must map to one lock identity.
        import hashlib
        a=client.canonical_url('HTTP://LOCALHOST:80/')
        b=client.canonical_url('http://localhost')
        self.assertEqual(a,b)

    # ---- item 2: the whole serialized prompt is bounded ------------------------------

    def test_full_prompt_is_bounded_including_title_and_instructions(self):
        huge={'task':{'id':'t','state':'open','last_n':1,'title':'T'*300000},
              'messages':[self._msg(1,'claude','task','hi')]}
        event=self._msg(1,'claude','task','hi')
        for budget in (2000,4000,8000):
            prompt=client.build_prompt('codex',huge,event,None,None,budget=budget)
            self.assertLessEqual(len(prompt.encode('utf-8')),budget)
        material=client.build_context(huge,event,None,None,budget=4000,agent='codex')
        self.assertTrue(material['thread']['task'].get('title_truncated'))

    def test_combined_structured_fields_and_escapes_fit_every_budget(self):
        # The former escape hatch: many structured fields whose JSON escaping and truncation
        # metadata could keep the serialized payload above budget even at the field floor.
        event={'task':'t','n':1,'kind':'task','from':'claude','body':'\\"\n'*5000,
               'meta':{'title':'X'*10000},'questions':[{'text':'X'*10000}],
               'results':[{'evidence':'X'*10000}],'needs_user':[{'text':'X'*10000}]}
        thread={'task':{'id':'t','state':'open','title':'X'*10000,'last_n':1},'messages':[event]}
        for budget in (1800,2000,4000):
            prompt=client.build_prompt('codex',thread,event,None,None,budget=budget)
            self.assertLessEqual(len(prompt.encode('utf-8')),budget)
            material=json.loads(prompt.split('Current event and task history follow as JSON data:\n',1)[1])
            # Identity of the event and an explicit full-thread reference survive the shrink.
            self.assertEqual((material['event']['task'],material['event']['n'],
                              material['event']['kind']),('t',1,'task'))
            self.assertEqual(material['thread']['task']['id'],'t')
            self.assertIn('bridge_show task t',material['context']['full_thread'])
            self.assertTrue(material['context']['truncated'])

    def test_impossible_prompt_budget_raises_a_clear_error(self):
        event=self._msg(1,'claude','task','hi')
        thread={'task':{'id':'t','state':'open','last_n':1},'messages':[event]}
        with self.assertRaises(ValueError) as caught:
            client.build_context(thread,event,None,None,100,'codex')
        self.assertIn('too small',str(caught.exception))

    def test_prompt_policy_is_conditional_in_the_actual_text(self):
        # Truncated new session -> mandatory fetch, in the text the CLI actually receives.
        event={'task':'t','n':1,'kind':'task','from':'claude','body':'\\"\n'*5000,
               'meta':{'title':'X'*10000},'questions':[{'text':'X'*10000}]}
        thread={'task':{'id':'t','state':'open','title':'X'*10000,'last_n':1},'messages':[event]}
        prompt=client.build_prompt('codex',thread,event,None,None,budget=1800)
        self.assertIn(client.POLICY_MANDATORY,prompt)
        self.assertNotIn(client.POLICY_COMPLETE,prompt)
        # Complete new session -> no forced full-thread fetch.
        small={'task':{'id':'t','state':'open','last_n':1},'messages':[self._msg(1,'claude','task','hi')]}
        ev=self._msg(1,'claude','task','hi')
        self.assertIn(client.POLICY_COMPLETE,client.build_prompt('codex',small,ev,None,None,budget=8000))
        # Untruncated resumed delta -> complete, so a resumption need not re-fetch the thread.
        recent={'task':{'id':'t','state':'open','last_n':3},'messages':[
            self._msg(1,'claude','task','o'),self._msg(2,'codex','ack','a'),
            self._msg(3,'claude','note','n')]}
        ev3=self._msg(3,'claude','note','n')
        resumed=client.build_prompt('codex',recent,ev3,'s',{'session':'s','delivered_n':1},budget=8000)
        self.assertIn(client.POLICY_COMPLETE,resumed)
        self.assertNotIn(client.POLICY_MANDATORY,resumed)

    def test_worker_launch_prompt_stays_within_budget(self):
        self.agents['claude'].request('POST','/v1/tasks',{'id':'bigbudget','kind':'task',
            'body':'A'*200000,'meta':{'to':'codex','title':'bigbudget'}})
        args=self.args('codex');args.prompt_budget=1800
        captured={}
        real=client.launch
        def spy(a,prompt,session=None,monitor=None,interval=2.0):
            captured['bytes']=len(prompt.encode());captured['prompt']=prompt
            return real(a,prompt,session,monitor,interval)
        with patch.object(client,'launch',side_effect=spy):
            self.assertTrue(client.worker_once(args,self.agents['codex'],self.root/'worker-codex'))
        self.assertLessEqual(captured['bytes'],1800)
        self.assertIn(client.POLICY_MANDATORY,captured['prompt'])
        self.assertEqual([c for c in self.calls() if c['task']=='bigbudget'][0]['prompt_bytes'],captured['bytes'])

    def test_opencode_prompt_budget_is_clamped_below_argv_limit(self):
        args=argparse.Namespace(client='opencode',prompt_budget=10_000_000)
        budget=client.effective_budget(args)
        self.assertLessEqual(budget,client.ARGV_SAFE_LIMIT)
        huge={'task':{'id':'t','state':'open','last_n':2,'title':'T'*400000},
              'messages':[self._msg(1,'claude','task','x'*200000),self._msg(2,'claude','note','y'*200000)]}
        event=self._msg(2,'claude','note','z'*200000)
        prompt=client.build_prompt('opencode',huge,event,None,None,budget=budget)
        self.assertLessEqual(len(prompt.encode('utf-8')),client.ARGV_SAFE_LIMIT)

    # ---- item 3: honest omission counts and checkpoints ------------------------------

    def test_new_session_counts_every_omission_and_requires_bridge_show(self):
        msgs=[self._msg(1,'claude','task','ORIGINAL')]+[
            self._msg(i,'claude','note','y'*4000) for i in range(2,22)]
        thread={'task':{'id':'t','state':'open','last_n':21},'messages':msgs}
        event=self._msg(21,'claude','note','trigger')
        m=client.build_context(thread,event,None,None,budget=6000)
        self.assertGreater(m['context']['omitted_messages'],0)
        self.assertTrue(m['context']['truncated'])
        self.assertTrue(m['context']['requires_bridge_show'])
        self.assertEqual(m['context']['delivered_through'],m['context']['checkpoint_n'])
        self.assertIn('bridge_show',m['context']['note'])

    def test_untruncated_turn_does_not_demand_bridge_show(self):
        thread={'task':{'id':'t','state':'open','last_n':2},
                'messages':[self._msg(1,'claude','task','t'),self._msg(2,'codex','ack','a')]}
        event=self._msg(1,'claude','task','t')
        m=client.build_context(thread,event,None,None,budget=8000)
        self.assertEqual(m['context']['omitted_messages'],0)
        self.assertFalse(m['context']['truncated'])
        self.assertFalse(m['context']['requires_bridge_show'])

    def test_a_clipped_title_alone_marks_the_turn_truncated(self):
        # m2c/A214-3: only the title is cut; the payload must not claim to be complete.
        thread={'task':{'id':'t','state':'open','last_n':2,'title':'T'*9000},
                'messages':[self._msg(1,'claude','task','t'),self._msg(2,'codex','ack','a')]}
        event=self._msg(1,'claude','task','t')
        m=client.build_context(thread,event,None,None,budget=20000)
        self.assertTrue(m['thread']['task']['title_truncated'])
        self.assertEqual(m['context']['omitted_messages'],0)
        self.assertTrue(m['context']['truncated'])
        self.assertTrue(m['context']['requires_bridge_show'])
        self.assertIn('bridge_show',m['context']['note'])

    def test_resume_oversized_delta_keeps_checkpoint_below_dropped_peer_input(self):
        msgs=[self._msg(i,'claude','note','z'*4000) for i in range(1,21)]
        thread={'task':{'id':'t','state':'open','last_n':20},'messages':msgs}
        event=self._msg(25,'claude','note','trigger')
        m=client.build_context(thread,event,'s',{'session':'s','delivered_n':0},
                               budget=2000,agent='codex')
        self.assertEqual(m['context']['mode'],'resume')
        self.assertGreater(m['context']['omitted_messages'],0)
        self.assertLess(m['context']['checkpoint_n'],20)

    def test_worker_records_the_checkpoint_build_context_reported(self):
        self.send('claude','codex','ckpt')
        os.environ['FAKE_QUESTION']='1'
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        record={}
        real=client.build_prompt
        def spy(agent,thread,event,session,prior,budget=client.DEFAULT_PROMPT_BUDGET):
            record['ck']=client.build_context(thread,event,session,prior,budget,agent)['context']['checkpoint_n']
            return real(agent,thread,event,session,prior,budget)
        with patch.object(client,'build_prompt',side_effect=spy):
            self.assertTrue(client.worker_once(self.args('codex'),self.agents['codex'],state))
        os.environ.pop('FAKE_QUESTION')
        ctx=json.loads((state/'context.json').read_text())
        self.assertIn('ck',record)
        self.assertEqual(ctx['ckpt']['delivered_n'],record['ck'])

    # ---- item 4: process-group cleanup when the parent exits first -------------------

    def test_parent_exit_leaves_no_resistant_descendant(self):
        pid_file=self.root/'child.pid';survived=self.root/'child.survived'
        script=self.root/'fork-cli'
        script.write_text(
            '#!/usr/bin/env python3\n'
            'import os,signal,json,sys,time\n'
            'signal.signal(signal.SIGTERM,signal.SIG_IGN)\n'
            'pid=os.fork()\n'
            'if pid==0:\n'
            '    open(os.environ["CHILD_PID"],"w").write(str(os.getpid()))\n'
            '    time.sleep(30)\n'
            '    open(os.environ["CHILD_SURVIVED"],"w").write("x")\n'
            '    os._exit(0)\n'
            'print(json.dumps({"type":"thread.started","thread_id":"pf"}));sys.stdout.flush()\n')
        script.chmod(0o755)
        os.environ['CHILD_PID']=str(pid_file);os.environ['CHILD_SURVIVED']=str(survived)
        os.environ.pop('PARENT_SLEEP',None)
        self.addCleanup(lambda:(os.environ.pop('CHILD_PID',None),os.environ.pop('CHILD_SURVIVED',None)))
        args=self.args('codex');args.client_bin=str(script);args.timeout=30
        result=client.launch(args,'prompt')
        self.assertTrue(result['ok'])
        self.assertEqual(result['session'],'pf')
        for _ in range(150):
            if pid_file.exists():break
            time.sleep(.02)
        self.assertTrue(pid_file.exists(),'child never started')
        pid=int(pid_file.read_text())
        for _ in range(200):
            if _pid_dead(pid):break
            time.sleep(.02)
        self.assertTrue(_pid_dead(pid),'resistant descendant was left alive')
        self.assertFalse(survived.exists(),'descendant outlived its process group')

    def test_interrupt_during_launch_cleans_up_owned_group(self):
        pid_file=self.root/'child2.pid';survived=self.root/'child2.survived'
        script=self.root/'fork-cli2'
        # The CLI stand-in starts slowly (before it ignores SIGTERM and forks) and the child
        # publishes its pid atomically (rename).  Without the readiness handshake below, the
        # interrupt always lands during that start: the stand-in dies before it forks and no pid
        # is ever written (the intermittent failure this test used to have).
        script.write_text(
            '#!/usr/bin/env python3\n'
            'import os,signal,json,sys,time\n'
            'time.sleep(0.3)\n'
            'signal.signal(signal.SIGTERM,signal.SIG_IGN)\n'
            'pid=os.fork()\n'
            'if pid==0:\n'
            '    tmp=os.environ["CHILD_PID"]+".tmp"\n'
            '    open(tmp,"w").write(str(os.getpid()))\n'
            '    os.replace(tmp,os.environ["CHILD_PID"])\n'
            '    time.sleep(30)\n'
            '    open(os.environ["CHILD_SURVIVED"],"w").write("x")\n'
            '    os._exit(0)\n'
            'print(json.dumps({"type":"thread.started","thread_id":"in"}));sys.stdout.flush()\n'
            'time.sleep(30)\n')
        script.chmod(0o755)
        os.environ['CHILD_PID']=str(pid_file);os.environ['CHILD_SURVIVED']=str(survived)
        self.addCleanup(lambda:(os.environ.pop('CHILD_PID',None),os.environ.pop('CHILD_SURVIVED',None)))
        args=self.args('codex');args.client_bin=str(script);args.timeout=30

        class NotReady(BaseException):
            """Not an Exception: launch() swallows those from a monitor probe."""

        ready_by=time.monotonic()+10
        def boom():
            # Readiness handshake: interrupt only once the forked child has published its pid,
            # so the group stop is known to cover a live descendant.  Bounded: never ready in
            # time is a clear failure, not a skipped check.
            try:
                text=pid_file.read_text()
            except FileNotFoundError:
                text=''
            if text.strip().isdigit():
                raise KeyboardInterrupt
            if time.monotonic()>ready_by:
                raise NotReady('the forked child never wrote its pid')
            return False
        with self.assertRaises(KeyboardInterrupt):
            client.launch(args,'prompt',None,monitor=boom,interval=.05)
        pid=int(pid_file.read_text())
        for _ in range(200):
            if _pid_dead(pid):break
            time.sleep(.02)
        self.assertTrue(_pid_dead(pid),'interrupt left the owned group alive')

    # ---- item 5: bounded monitoring probes ------------------------------------------

    def test_monitor_probe_is_bounded_when_server_stalls(self):
        def stall(request,conn):
            time.sleep(10)
        srv=FakeHTTP(stall);self.addCleanup(srv.close)
        bridge=client.Bridge(srv.url(),self.root/'config/codex.token','codex')
        monitor=client.task_monitor(bridge,'t',timeout=.3)
        start=time.monotonic();result=monitor();elapsed=time.monotonic()-start
        self.assertFalse(result)  # a stalled probe is never read as "cancelled"
        self.assertLess(elapsed,3)

    def test_monitor_never_pushes_launch_past_a_short_deadline(self):
        # A stalled monitor request must not extend the CLI's own deadline.
        def stall(request,conn):
            time.sleep(2)
        srv=FakeHTTP(stall);self.addCleanup(srv.close)
        bridge=client.Bridge(srv.url(),self.root/'config/codex.token','codex')
        os.environ['FAKE_SLEEP']='1'
        args=self.args('codex');args.timeout=.3
        ev=self._msg(1,'claude','task','hi')
        thread={'task':{'id':'t','state':'open','last_n':1},'messages':[ev]}
        prompt=client.build_prompt('codex',thread,ev,None,None,budget=8000)
        start=time.monotonic()
        result=client.launch(args,prompt,monitor=client.task_monitor(bridge,'t'),interval=.05)
        elapsed=time.monotonic()-start
        self.assertLess(elapsed,1.5)  # not ~2s+ as when the probe is awaited inline
        self.assertFalse(result['ok'])
        self.assertFalse(result.get('cancelled'))  # a stalled probe is never read as "cancelled"
        self.assertIn('timeout',result['error'])

    def test_doctor_probe_is_bounded_when_server_stalls(self):
        def stall(request,conn):
            time.sleep(10)
        srv=FakeHTTP(stall);self.addCleanup(srv.close)
        start=time.monotonic()
        report=client.doctor(self.doctor_args(url=srv.url()),http_timeout=.3)
        elapsed=time.monotonic()-start
        self.assertFalse(report['ok'])
        self.assertFalse([c for c in report['checks'] if c['name']=='server'][0]['ok'])
        self.assertLess(elapsed,4)

    # ---- item 6: hardened, secret-safe doctor ---------------------------------------

    def _served(self,protocol='2',identity_error=None,identity_protocol='2',agents=None):
        token=(self.root/'config/codex.token').read_text().strip()
        if agents is None:
            agents=['codex']
        def handler(request,conn):
            path=request.split('\r\n',1)[0].split(' ')[1]
            if path.startswith('/v1/health'):
                conn.sendall(_http_response(200,json.dumps({'service':'agent-bridge','protocol':protocol})))
            elif path.startswith('/v1/identity'):
                if identity_error is not None:
                    conn.sendall(_http_response(401,identity_error))
                else:
                    conn.sendall(_http_response(200,json.dumps({'agent':'codex','protocol':identity_protocol,'agents':agents})))
            else:
                conn.sendall(_http_response(404,'{}'))
        return FakeHTTP(handler),token

    def test_doctor_rejects_incompatible_api_protocol(self):
        srv,token=self._served(protocol='1');self.addCleanup(srv.close)
        report=client.doctor(self.doctor_args(url=srv.url()),http_timeout=2)
        self.assertFalse(report['ok'])
        self.assertFalse([c for c in report['checks'] if c['name']=='server'][0]['ok'])
        self.assertNotIn(token,json.dumps(report))

    def test_doctor_redacts_a_token_reflected_in_an_error_body(self):
        token=(self.root/'config/codex.token').read_text().strip()
        srv,_=self._served(identity_error='{"error":"bad token '+token+' rejected"}')
        self.addCleanup(srv.close)
        report=client.doctor(self.doctor_args(url=srv.url()),http_timeout=2)
        self.assertFalse(report['ok'])
        self.assertNotIn(token,json.dumps(report))
        self.assertNotIn(hashlib.sha256(token.encode()).hexdigest(),json.dumps(report))

    def test_doctor_flags_malformed_and_windows_client_configs(self):
        os.environ['OPENCODE_CONFIG_CONTENT']='{ not valid json'
        self.addCleanup(lambda:os.environ.pop('OPENCODE_CONFIG_CONTENT',None))
        report=client.doctor(self.doctor_args(client='opencode',agent='opencode',
            token_file=str(self.root/'config/opencode.token')),http_timeout=2)
        self.assertFalse([c for c in report['checks'] if c['name']=='mcp_config'][0]['ok'])
        report=client.doctor(self.doctor_args(client='opencode',agent='opencode',
            token_file=str(self.root/'config/opencode.token'),client_bin='opencode.exe'),http_timeout=2)
        self.assertFalse([c for c in report['checks'] if c['name']=='mcp_config'][0]['ok'])

    def test_doctor_flags_missing_workdir_and_nonzero_cli(self):
        report=client.doctor(self.doctor_args(workdir=str(self.root/'nope')),http_timeout=2)
        self.assertFalse([c for c in report['checks'] if c['name']=='workdir'][0]['ok'])
        self.assertFalse(report['ok'])
        bad=self.root/'bad-cli'
        bad.write_text('#!'+sys.executable+'\nimport json,sys\nprint(json.dumps({"v":1}));sys.exit(1)\n')
        bad.chmod(0o755)
        report=client.doctor(self.doctor_args(client_bin=str(bad)),http_timeout=2)
        self.assertFalse([c for c in report['checks'] if c['name']=='client_version'][0]['ok'])
        self.assertTrue([c for c in report['checks'] if c['name']=='client_binary'][0]['ok'])
        # An unusable/declared version must fail the whole report, not just the one check.
        self.assertFalse(report['ok'])

    def test_doctor_nonzero_client_cli_exits_nonzero_json(self):
        bad=self.root/'bad-cli'
        bad.write_text('#!'+sys.executable+'\nimport json,sys\nprint(json.dumps({"v":1}));sys.exit(1)\n')
        bad.chmod(0o755)
        proc=subprocess.run([sys.executable,str(HERE/'bridge_agents.py'),'doctor','--client','codex',
            '--agent','codex','--url',self.url,'--token-file',str(self.root/'config/codex.token'),
            '--bridge-bin',self.binary,'--client-bin',str(bad)],capture_output=True,text=True,timeout=30)
        self.assertEqual(proc.returncode,1)
        report=json.loads(proc.stdout)
        self.assertFalse(report['ok'])
        self.assertFalse([c for c in report['checks'] if c['name']=='client_version'][0]['ok'])

    def test_doctor_rejects_invalid_or_missing_identity_membership(self):
        # Schema violation: agents is not a list of non-empty strings.
        srv,_=self._served(agents=['codex',7]);self.addCleanup(srv.close)
        report=client.doctor(self.doctor_args(url=srv.url()),http_timeout=2)
        self.assertFalse([c for c in report['checks'] if c['name']=='identity'][0]['ok'])
        self.assertFalse(report['ok'])
        # Membership violation: the authenticated agent omitted from the server's agent list.
        srv,_=self._served(agents=['claude','dsh']);self.addCleanup(srv.close)
        report=client.doctor(self.doctor_args(url=srv.url()),http_timeout=2)
        self.assertFalse([c for c in report['checks'] if c['name']=='identity'][0]['ok'])
        self.assertFalse(report['ok'])


    def test_changed_native_session_keeps_sent_checkpoint_and_recovers_original(self):
        self.send('claude','codex','sessionchange')
        args=self.args('codex');state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        os.environ['FAKE_QUESTION']='1'
        self.assertTrue(client.worker_once(args,self.agents['codex'],state))
        os.environ.pop('FAKE_QUESTION')
        self.agents['claude'].post('sessionchange',{'kind':'answer','body':'answer'})
        for i in range(25):
            self.agents['claude'].post('sessionchange',{'kind':'note','reply_to':1,
                'body':'extra '+str(i),'wake':'normal'})
        args.prompt_budget=1800
        sent={};real=client.launch
        def switched(args,prompt,session=None,**kw):
            material=json.loads(prompt.split('Current event and task history follow as JSON data:\n',1)[1])
            sent.update(checkpoint=material['context']['checkpoint_n'],session=session)
            result=real(args,prompt,session,**kw)
            result['session']='new-native-session'
            return result
        with patch.object(client,'launch',side_effect=switched):
            self.assertTrue(client.worker_once(args,self.agents['codex'],state))
        ctx=json.loads((state/'context.json').read_text())['sessionchange']
        self.assertEqual(ctx['delivered_n'],sent['checkpoint'])
        self.assertIsNone(ctx['session'])  # returned session did not receive the old session's context
        self.assertEqual(json.loads((state/'sessions.json').read_text())['sessionchange'],'new-native-session')
        self.agents['claude'].post('sessionchange',{'kind':'verdict','judgement':'rework','body':'continue'})
        args.prompt_budget=8000
        recovered={}
        def spy(args,prompt,session=None,**kw):
            material=json.loads(prompt.split('Current event and task history follow as JSON data:\n',1)[1])
            recovered.update(mode=material['context']['mode'],origin=material['original_task'])
            return real(args,prompt,session,**kw)
        with patch.object(client,'launch',side_effect=spy):
            self.assertTrue(client.worker_once(args,self.agents['codex'],state))
        self.assertEqual(recovered['mode'],'new')
        self.assertEqual(recovered['origin']['kind'],'task')
        self.assertEqual(recovered['origin']['n'],1)


class ScopedApprovalTest(Harness):
    def test_default_codex_does_not_preapprove_tools(self):
        cmd,_,_=client.build_launch(self.args('codex'),'test')
        self.assertFalse(any('approval_mode' in part for part in cmd))

    def test_explicit_approvals_are_scoped_and_apply_to_resume(self):
        import bridge_adapters
        args=self.args('codex');args.approve_bridge_tools=True
        for session in (None,'existing-thread'):
            cmd,_,_=client.build_launch(args,'test',session)
            actual={part.split('.tools.',1)[1].split('.',1)[0] for part in cmd if '.tools.' in part}
            self.assertEqual(actual,set(bridge_adapters.APPROVED_BRIDGE_TOOLS))
            self.assertNotIn('bridge_decide',actual);self.assertNotIn('bridge_asked',actual)
            self.assertFalse(any('approval_policy' in part or 'sandbox_mode' in part for part in cmd))
            self.assertNotIn('--yolo',cmd);self.assertNotIn('--auto',cmd)
            self.assertTrue(all('approval_mode="approve"' in part for part in cmd if '.tools.' in part))

    def test_preapproval_rejects_nonlocal_or_ambiguous_endpoints_and_other_clients(self):
        args=self.args('codex');args.approve_bridge_tools=True
        for url in ('https://example.com','http://127.0.0.1.evil','http://u:p@localhost',
                    'http://localhost?q=1','http://localhost#f','file:///tmp/socket'):
            args.url=url
            with self.assertRaises(ValueError):client.build_launch(args,'test')
        args=self.args('opencode');args.approve_bridge_tools=True
        with self.assertRaises(ValueError):client.build_launch(args,'test')

    def test_config_outputs_valid_toml_with_only_selected_tool_approvals(self):
        import tomllib,bridge_adapters
        args=self.args('codex')
        cmd=[sys.executable,str(HERE/'bridge_agents.py'),'config','--client','codex',
             '--url',args.url,'--token-file',args.token_file,'--bridge-bin',self.binary,
             '--approve-bridge-tools']
        result=subprocess.run(cmd,capture_output=True,text=True,timeout=15)
        self.assertEqual(result.returncode,0,result.stderr)
        cfg=tomllib.loads(result.stdout)['mcp_servers']['agent_bridge']
        self.assertEqual(set(cfg['tools']),set(bridge_adapters.APPROVED_BRIDGE_TOOLS))
        self.assertTrue(all(v['approval_mode']=='approve' for v in cfg['tools'].values()))
        self.assertNotIn((self.root/'config/codex.token').read_text().strip(),result.stdout)

    def test_explicit_approval_worker_preserves_task_protocol(self):
        self.send('claude','codex','approved-flow')
        args=self.args('codex');args.approve_bridge_tools=True
        state=self.root/'worker-codex';state.mkdir(exist_ok=True)
        self.assertTrue(client.worker_once(args,self.agents['codex'],state))
        thread=self.agents['claude'].request('GET','/v1/tasks/approved-flow')
        self.assertEqual([m['kind'] for m in thread['messages']],['task','ack','result'])
        self.assertEqual(thread['task']['state'],'awaiting_verdict')


if __name__=='__main__':unittest.main(verbosity=2)
