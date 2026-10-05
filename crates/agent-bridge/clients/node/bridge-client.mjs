// agent-bridge reference client for Node (>= 18): built-in fetch, no dependencies.
//
// This is the specification-by-example agreed in the design (§8): copy it as is, or use it to
// cross-check your own client. Contract:
//   - explicit timeouts; a long poll's HTTP timeout is longer than its wait
//   - 4xx is final (the request is wrong and stays wrong): never retried
//   - network errors and 5xx are retried a few times with the SAME clientMsgId, so a retried
//     post can never create a second message
//   - responses are decoded as UTF-8 explicitly (invalid bytes are an error, not mojibake)
//   - reading the inbox never marks anything read; call ack() for what you have handled
//
//   import { BridgeClient } from './bridge-client.mjs'
//   const c = new BridgeClient({ url: 'http://127.0.0.1:7879', token })
//   const { messages } = await c.inbox()

import { randomUUID } from 'node:crypto'

export const PROTOCOL = '1'

export class BridgeError extends Error {
  constructor(message, { status = null, retryable = false } = {}) {
    super(message)
    this.name = 'BridgeError'
    this.status = status // HTTP status, or null when the service was not reached
    this.retryable = retryable
  }
}

const utf8 = new TextDecoder('utf-8', { fatal: true })
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))

export class BridgeClient {
  /**
   * @param {{url?: string, token: string, timeoutMs?: number, retries?: number, fetch?: typeof fetch}} o
   */
  constructor({ url = 'http://127.0.0.1:7879', token, timeoutMs = 30000, retries = 3, fetch: f = globalThis.fetch } = {}) {
    if (!token || String(token).trim().length < 32) throw new BridgeError('token missing or too short')
    this.url = String(url).replace(/\/+$/, '')
    this.token = String(token).trim()
    this.timeoutMs = timeoutMs
    this.retries = retries
    this.fetch = f
  }

  async #call(method, path, body, timeoutMs = this.timeoutMs) {
    let last
    for (let attempt = 0; attempt < this.retries; attempt++) {
      if (attempt > 0) await sleep(300 * attempt)
      let res
      try {
        res = await this.fetch(this.url + path, {
          method,
          headers: { authorization: `Bearer ${this.token}`, ...(body ? { 'content-type': 'application/json; charset=utf-8' } : {}) },
          body: body ? JSON.stringify(body) : undefined,
          signal: AbortSignal.timeout(timeoutMs),
        })
      } catch (err) {
        last = new BridgeError(`agent-bridge unreachable at ${this.url}: ${err.message}`, { retryable: true })
        continue
      }
      let text
      try {
        text = utf8.decode(await res.arrayBuffer())
      } catch {
        throw new BridgeError(`agent-bridge ${res.status}: response is not valid UTF-8`, { status: res.status })
      }
      let data
      try {
        data = text ? JSON.parse(text) : null
      } catch {
        data = { error: text }
      }
      if (res.ok) return data
      const err = new BridgeError(`agent-bridge ${res.status}: ${(data && data.error) || text}`, { status: res.status, retryable: res.status >= 500 })
      if (!err.retryable) throw err // 4xx: final
      last = err
    }
    throw last
  }

  health() {
    return this.#call('GET', '/v1/health')
  }

  tasks({ open = false } = {}) {
    return this.#call('GET', open ? '/v1/tasks?open=true' : '/v1/tasks')
  }

  task(id) {
    return this.#call('GET', `/v1/tasks/${encodeURIComponent(id)}`)
  }

  /** Hands another agent a new task. */
  send({ id, to, title, body = '', priority, exprId, clientMsgId = randomUUID() }) {
    const meta = { to, title, ...(priority ? { priority } : {}), ...(exprId ? { expr_id: exprId } : {}) }
    return this.#call('POST', '/v1/tasks', { id, kind: 'task', body, meta, protocol: PROTOCOL, client_msg_id: clientMsgId })
  }

  /**
   * Posts a message: { kind, body?, outcome?, judgement?, questions?, results?, needs_user?, reply_to?, supersedes? }.
   * Pass your own clientMsgId to make a retry across process restarts idempotent too.
   */
  post(task, { clientMsgId = randomUUID(), ...msg }) {
    return this.#call('POST', `/v1/tasks/${encodeURIComponent(task)}/messages`, { ...msg, protocol: PROTOCOL, client_msg_id: clientMsgId })
  }

  /** Unread messages for you; waits up to `wait` seconds (0–120) for one. Does not mark them read. */
  inbox({ wait = 0 } = {}) {
    const w = Math.max(0, Math.min(120, Math.floor(wait)))
    return this.#call('GET', `/v1/inbox?wait=${w}`, null, (w + 15) * 1000)
  }

  /** Marks a task's messages handled up to and including n. */
  ack(task, n) {
    return this.#call('POST', '/v1/inbox/ack', { task, n })
  }
}
