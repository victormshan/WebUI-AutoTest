// Runs the reference client against a real agent-bridge binary.
//   AGENT_BRIDGE_BIN=target/release/agent-bridge node --test crates/agent-bridge/clients/node/bridge-client.test.mjs
import test from 'node:test'
import assert from 'node:assert/strict'
import { spawn, execFileSync } from 'node:child_process'
import { mkdtempSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { randomBytes } from 'node:crypto'
import { BridgeClient, BridgeError } from './bridge-client.mjs'

const BIN = process.env.AGENT_BRIDGE_BIN || 'target/release/agent-bridge'
const hash = (t) => execFileSync(BIN, ['hash-token'], { input: t }).toString().trim()

async function startService() {
  const dir = mkdtempSync(path.join(tmpdir(), 'agent-bridge-node-'))
  const claude = randomBytes(32).toString('hex')
  const dsh = randomBytes(32).toString('hex')
  writeFileSync(path.join(dir, 'agents.json'), JSON.stringify({ claude: hash(claude), dsh: hash(dsh) }))
  // An explicit empty notify file: without it the service would use ~/.config/agent-bridge/notify.json
  // and push test tasks to the real DSH.
  writeFileSync(path.join(dir, 'notify.json'), '{}')
  const port = 20000 + Math.floor(Math.random() * 20000)
  const proc = spawn(BIN, ['serve', '--state', path.join(dir, 'state'), '--listen', `127.0.0.1:${port}`, '--agents', path.join(dir, 'agents.json'), '--notify', path.join(dir, 'notify.json')], { stdio: 'ignore' })
  const url = `http://127.0.0.1:${port}`
  for (let i = 0; i < 50; i++) {
    try {
      if ((await fetch(url + '/v1/health')).ok) break
    } catch {}
    await new Promise((r) => setTimeout(r, 100))
  }
  return { url, claude, dsh, stop: () => proc.kill() }
}

test('reference client: send, long poll, reply, ack, verdict, close', async () => {
  const s = await startService()
  try {
    const claude = new BridgeClient({ url: s.url, token: s.claude })
    const dsh = new BridgeClient({ url: s.url, token: s.dsh })
    assert.equal((await dsh.health()).service, 'agent-bridge')

    const waiting = dsh.inbox({ wait: 20 })
    await new Promise((r) => setTimeout(r, 200))
    const t0 = Date.now()
    await claude.send({ id: 'n1', to: 'dsh', title: '核对', body: '请核对（中文）' })
    const { messages } = await waiting
    assert.ok(Date.now() - t0 < 5000, 'long poll returns on arrival')
    assert.equal(messages[0].body, '请核对（中文）')

    // Retrying with the same clientMsgId does not create a second message.
    const a1 = await dsh.post('n1', { kind: 'ack', clientMsgId: 'same-key' })
    const a2 = await dsh.post('n1', { kind: 'ack', clientMsgId: 'same-key' })
    assert.equal(a1.n, a2.n)
    await dsh.post('n1', { kind: 'result', outcome: 'done', results: [{ item: '核对', status: 'done', evidence: 'node --test' }] })
    await dsh.ack('n1', 1)
    assert.equal((await dsh.inbox()).messages.length, 0)
    assert.equal((await claude.inbox()).messages.length, 2)
    await claude.post('n1', { kind: 'verdict', judgement: 'pass' })
    await claude.post('n1', { kind: 'close' })
    assert.equal((await claude.task('n1')).task.state, 'closed')
  } finally {
    s.stop()
  }
})

test('reference client: protocol 2 — note, restart phase, user decisions, presence', async () => {
  const s = await startService()
  try {
    const claude = new BridgeClient({ url: s.url, token: s.claude })
    const dsh = new BridgeClient({ url: s.url, token: s.dsh })
    const sent = await claude.send({ id: 'p1', to: 'dsh', title: '交付', body: 'v4.15' })
    assert.equal(sent.delivery.to, 'dsh')
    assert.equal(sent.delivery.push, 'no-push-target')
    const ack = await dsh.post('p1', { kind: 'ack' })
    assert.deepEqual(ack.implicit_read, { task: 'p1', from: 1, to: 2 })
    await claude.note('p1', 1, '补充：先跑 verify')
    assert.equal((await claude.task('p1')).task.state, 'acked', 'a note never moves the task')
    await dsh.post('p1', { kind: 'progress', phase: 'pending-restart', needs_user: [{ text: '可以重启宿主吗？', relay: 'claude' }] })
    assert.equal((await claude.task('p1')).task.state, 'awaiting_restart')
    const [item] = (await claude.pending()).items
    assert.equal(item.relay, 'claude')
    assert.equal((await claude.asked(item.item)).advanced, true)
    assert.equal((await claude.decide({ item: item.item, verbatim: '可以' })).status, 'decided')
    const direct = await dsh.decide({ task: 'p1', verbatim: '用户说今晚再合并', form: 'paraphrase' })
    assert.equal(direct.item, 'p1#direct#1')
    await dsh.post('p1', { kind: 'progress', phase: 'restarted', body: 'bootId 2' })
    await dsh.post('p1', { kind: 'result', outcome: 'partial', results: [
      { item: '交付', status: 'done', evidence: 'verify ok' },
      { item: '手机链接', status: 'failed', evidence: '404', follow_up: 'p2' },
    ] })
    await dsh.presence({ host: '4.15.0', up: true })
    const h = await claude.health()
    assert.equal(h.protocol, '2')
    assert.equal(h.presence.dsh.platform.host, '4.15.0')
  } finally {
    s.stop()
  }
})

test('reference client: 4xx is final, unreachable is retried then reported', async () => {
  const s = await startService()
  try {
    let calls = 0
    const counting = (...a) => { calls++; return fetch(...a) }
    const dsh = new BridgeClient({ url: s.url, token: s.dsh, fetch: counting })
    await assert.rejects(dsh.post('missing', { kind: 'ack' }), (e) => e instanceof BridgeError && e.status === 404 && !e.retryable)
    assert.equal(calls, 1, '4xx not retried')
    const wrong = new BridgeClient({ url: s.url, token: 'x'.repeat(40) })
    await assert.rejects(wrong.tasks(), (e) => e.status === 401)
  } finally {
    s.stop()
  }
  const gone = new BridgeClient({ url: 'http://127.0.0.1:9', token: 'y'.repeat(40), retries: 2 })
  await assert.rejects(gone.health(), (e) => e instanceof BridgeError && e.status === null && /unreachable/.test(e.message))
  assert.throws(() => new BridgeClient({ token: 'short' }), BridgeError)
})
