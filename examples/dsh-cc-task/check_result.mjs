#!/usr/bin/env node
// Reads a settled webtest cc-task the way dsh-web-relay does, using the relay's
// own modules: v2 result validation + cc-channel review verdict parsing.
//
//   node check_result.mjs <taskId>        (env: CC_TASKS, DSH_WEB_RELAY)
//
// Prints one JSON object; exit 0 when the task settled with a consistent verdict.
import { readFileSync, existsSync, promises as fsp } from 'node:fs'
import { execFileSync } from 'node:child_process'
import path from 'node:path'
import { pathToFileURL } from 'node:url'

const taskId = process.argv[2]
if (!taskId) {
  console.error('usage: check_result.mjs <taskId>')
  process.exit(2)
}
const ccTasks = process.env.CC_TASKS || '/mnt/d/cc-tasks'
const relay = process.env.DSH_WEB_RELAY || '/mnt/d/dsh-web-relay'
const taskDir = path.join(ccTasks, 'tasks', taskId)
const readJson = (p) => (existsSync(p) ? JSON.parse(readFileSync(p, 'utf8').replace(/^﻿/, '')) : null)

const { readReviewOut, parseCcVerdict } = await import(pathToFileURL(path.join(relay, 'lib/cc-channel.js')).href)

// 1. runner settlement + v2 result validation (same CLI runner.sh uses)
const runnerResult = readJson(path.join(taskDir, 'result.json'))
let v2
try {
  v2 = JSON.parse(execFileSync('node', [path.join(relay, 'scripts/task-schema-cli.mjs'), 'validate-result', taskDir], { encoding: 'utf8' }))
} catch (e) {
  v2 = JSON.parse(e.stdout || '{"ok":false}')
}

// 2. verdict exactly as cc-channel reads a review task
const review = await readReviewOut({ fsImpl: fsp, root: ccTasks, taskId })
const verdict = review.ok ? parseCcVerdict(review.text) : null // { verdict: 'approved'|'rejected', reason }

// 3. webtest's own machine-readable result
const webtest = readJson(path.join(taskDir, 'out', 'webtest-result.json'))
const expected = webtest ? (webtest.status === 'passed' ? 'approved' : 'rejected') : null

const consistent =
  runnerResult?.status === 'done' && v2?.ok === true && Boolean(webtest) && verdict?.verdict === expected

console.log(JSON.stringify({ taskId, runner: runnerResult, v2, verdict, expectedVerdict: expected, webtest, consistent }, null, 2))
process.exit(consistent ? 0 : 1)
