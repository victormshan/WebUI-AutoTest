#!/usr/bin/env python3
"""Tests for codex-worker.py against a private agent-bridge service and a fake Codex.

    python3 crates/agent-bridge/clients/codex/test_codex_worker.py

The service gets its own state directory, agents file and an explicit empty --notify file,
so nothing reaches the real bridge or the real DSH. Needs `agent-bridge` on PATH.
"""

import importlib.util
import json
import os
import socket
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("codex_worker", HERE / "codex-worker.py")
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)

FAKE_CODEX = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    # Fake `codex exec [resume <id>]`: reports a thread like --json does, echoes what it was
    # given into the -o file, logs each call, and fails on request.
    import json, os, sys, uuid
    args = sys.argv[1:]
    material = sys.stdin.read()
    resume = len(args) > 1 and args[1] == "resume"
    thread = args[2] if resume else str(uuid.uuid4())
    # Like the real CLI: `exec` appends stdin to the prompt argument, `exec resume` does not;
    # there stdin is read only when the prompt argument is "-".
    if resume and args[-1] != "-":
        material = ""
    with open(os.environ["FAKE_CODEX_LOG"], "a") as log:
        log.write(json.dumps({"resume": resume, "thread": thread, "material": material}) + "\\n")
    if os.environ.get("FAKE_CODEX_FAIL") or (resume and os.environ.get("FAKE_CODEX_RESUME_FAIL")):
        print("fake failure", file=sys.stderr)
        sys.exit(2)
    if resume:
        assert 'sandbox_mode="read-only"' in args, args
    else:
        assert "-s" in args and args[args.index("-s") + 1] == "read-only", args
    assert "--json" in args, args
    out = args[args.index("-o") + 1]
    with open(out, "w") as f:
        f.write("VERDICT: APPROVE\\n\\nreviewed:\\n" + material)
    print(json.dumps({"type": "thread.started", "thread_id": thread}))
    print(json.dumps({"type": "turn.completed"}))
    """
)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class WorkerTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name)
        tokens = {a: f"{a}-test-token-0123456789abcdef0123456789" for a in ("claude", "codex")}
        hashes = {}
        for agent, token in tokens.items():
            (root / f"{agent}.token").write_text(token)
            h = subprocess.run(["agent-bridge", "hash-token"], input=token, capture_output=True, text=True, check=True)
            hashes[agent] = h.stdout.strip()
        (root / "agents.json").write_text(json.dumps(hashes))
        (root / "notify.json").write_text("{}")
        port = free_port()
        cls.url = f"http://127.0.0.1:{port}"
        cls.server = subprocess.Popen(
            ["agent-bridge", "serve", "--state", str(root / "state"), "--listen", f"127.0.0.1:{port}",
             "--agents", str(root / "agents.json"), "--notify", str(root / "notify.json")],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=0.2).close()
                break
            except OSError:
                time.sleep(0.1)
        fake = root / "fake-codex"
        fake.write_text(FAKE_CODEX)
        fake.chmod(0o755)
        cls.root = root
        cls.base_env = {
            "AGENT_BRIDGE_URL": cls.url,
            "CODEX_EXE": str(fake),
            "CODEX_WORKDIR": str(root / "work"),
            "XDG_STATE_HOME": str(root / "xdg"),
            "FAKE_CODEX_LOG": str(root / "codex-calls.jsonl"),
        }

    @classmethod
    def tearDownClass(cls):
        cls.server.terminate()
        cls.server.wait(timeout=10)
        cls.tmp.cleanup()

    def setUp(self):
        self.saved = dict(os.environ)
        os.environ.update(self.base_env)
        os.environ.pop("FAKE_CODEX_FAIL", None)
        os.environ.pop("FAKE_CODEX_RESUME_FAIL", None)
        # Each test starts without a review thread and with an empty call log.
        (self.root / "xdg" / "codex-worker" / "threads.json").unlink(missing_ok=True)
        (self.root / "codex-calls.jsonl").unlink(missing_ok=True)

    def tearDown(self):
        os.environ.clear()
        os.environ.update(self.saved)

    def as_agent(self, agent):
        os.environ["AGENT_BRIDGE_TOKEN_FILE"] = str(self.root / f"{agent}.token")

    def send(self, task, body):
        self.as_agent("claude")
        worker.bridge("send", "--id", task, "--to", "codex", "--title", f"title of {task}", "--body", body)

    def run_worker_once(self):
        self.as_agent("codex")
        worker.poll(self.root / "xdg" / "codex-worker", 0)

    def calls(self):
        f = self.root / "codex-calls.jsonl"
        return [json.loads(l) for l in f.read_text().splitlines()] if f.exists() else []

    def messages(self, task):
        self.as_agent("claude")
        return worker.bridge("show", task)["messages"]

    def test_a_review_is_acked_and_answered(self):
        self.send("t-ok", "MATERIAL-OK")
        self.run_worker_once()
        ms = self.messages("t-ok")
        self.assertEqual([m["kind"] for m in ms], ["task", "ack", "result"])
        result = ms[-1]
        self.assertEqual(result["from"], "codex")
        self.assertEqual(result["outcome"], "done")
        self.assertTrue(result["body"].startswith("VERDICT: APPROVE"))
        self.assertIn("# title of t-ok", result["body"])
        self.assertIn("MATERIAL-OK", result["body"])
        # Read cursor moved: nothing left in codex's inbox.
        self.as_agent("codex")
        self.assertEqual((worker.bridge("inbox") or {}).get("messages", []), [])

    def test_a_failed_review_is_reported_as_blocked(self):
        os.environ["FAKE_CODEX_FAIL"] = "1"
        self.send("t-fail", "MATERIAL")
        self.run_worker_once()
        result = self.messages("t-fail")[-1]
        self.assertEqual((result["kind"], result["outcome"]), ("result", "blocked"))
        self.assertIn("没有完成", result["body"])
        self.assertIn("fake failure", result["body"])

    def test_rework_in_the_same_thread_sends_only_the_new_message(self):
        self.send("t-rework", "FIRST-ROUND")
        self.run_worker_once()
        self.as_agent("claude")
        worker.bridge("post", "--task", "t-rework", "--kind", "verdict", "--judgement", "rework",
                      "--body", "FIXED-IN-ROUND-TWO")
        self.run_worker_once()
        ms = self.messages("t-rework")
        self.assertEqual([m["kind"] for m in ms], ["task", "ack", "result", "verdict", "progress", "result"])
        first, second = self.calls()
        self.assertFalse(first["resume"])
        self.assertTrue(second["resume"])
        self.assertEqual(second["thread"], first["thread"])
        # The thread already has round one: only the new message is sent.
        self.assertIn("FIXED-IN-ROUND-TWO", second["material"])
        self.assertNotIn("FIRST-ROUND", second["material"])
        self.assertIn(first["thread"], ms[-1]["body"])

    def test_later_reviews_continue_the_same_thread(self):
        self.send("t-one", "PHASE-ONE")
        self.run_worker_once()
        self.send("t-two", "PHASE-TWO")
        self.run_worker_once()
        one, two = self.calls()
        self.assertEqual((one["resume"], two["resume"]), (False, True))
        self.assertEqual(two["thread"], one["thread"])
        self.assertIn("PHASE-TWO", two["material"])
        result = self.messages("t-two")[-1]["body"]
        self.assertTrue(result.startswith("VERDICT: APPROVE"))
        self.assertIn(f"codex resume {one['thread']}", result)

    def test_a_thread_that_cannot_be_resumed_is_replaced_with_full_material(self):
        self.send("t-a", "ROUND-ONE")
        self.run_worker_once()
        os.environ["FAKE_CODEX_RESUME_FAIL"] = "1"
        self.as_agent("claude")
        worker.bridge("post", "--task", "t-a", "--kind", "verdict", "--judgement", "rework",
                      "--body", "ROUND-TWO")
        self.run_worker_once()
        first, failed, fresh = self.calls()
        self.assertTrue(failed["resume"])
        self.assertFalse(fresh["resume"])
        self.assertNotEqual(fresh["thread"], first["thread"])
        # A new thread knows nothing: it gets the whole task thread.
        for text in ("ROUND-ONE", "ROUND-TWO", "rework"):
            self.assertIn(text, fresh["material"])
        result = self.messages("t-a")[-1]
        self.assertEqual(result["outcome"], "done")
        self.assertIn(fresh["thread"], result["body"])
        threads = json.loads((self.root / "xdg" / "codex-worker" / "threads.json").read_text())
        self.assertEqual(threads["thread"], fresh["thread"])

    def test_a_partial_threads_file_does_not_stop_reviews(self):
        state = self.root / "xdg" / "codex-worker"
        state.mkdir(parents=True, exist_ok=True)
        for content in ('{"thread": "abc"}', '[]', '{"tasks": null}'):
            (state / "threads.json").write_text(content)
            self.assertIn("tasks", worker.load_threads(state))
        # A thread id without a task map: the review resumes it and sends everything.
        (state / "threads.json").write_text('{"thread": "kept-thread"}')
        self.send("t-partial", "MATERIAL-PARTIAL")
        self.run_worker_once()
        call = self.calls()[-1]
        self.assertTrue(call["resume"])
        self.assertEqual(call["thread"], "kept-thread")
        self.assertIn("MATERIAL-PARTIAL", call["material"])
        self.assertEqual(self.messages("t-partial")[-1]["outcome"], "done")

    def test_a_restarted_worker_does_not_answer_twice(self):
        self.send("t-retry", "MATERIAL")
        self.as_agent("codex")
        task_msg = worker.bridge("inbox")["messages"][0]
        state = self.root / "xdg" / "codex-worker"
        # First run posts ack and result, then "crashes" before moving the read cursor.
        worker.review("t-retry", task_msg["n"], "t", "MATERIAL", state, True)
        # The restarted worker runs again: no second ack, no second result.
        self.run_worker_once()
        kinds = [m["kind"] for m in self.messages("t-retry")]
        self.assertEqual(kinds, ["task", "ack", "result"])

    def test_a_review_interrupted_after_the_ack_is_finished_after_a_restart(self):
        self.send("t-crash", "MATERIAL-CRASH")
        self.as_agent("codex")
        task_msg = worker.bridge("inbox")["messages"][0]
        # The worker acked, then died before Codex answered. The ack marked the task read.
        worker.post("t-crash", {"kind": "ack", "body": "x", "client_msg_id": f"codex-start-t-crash-{task_msg['n']}"},
                    self.root / "xdg" / "codex-worker")
        self.assertEqual((worker.bridge("inbox") or {}).get("messages", []), [])
        self.run_worker_once()
        ms = self.messages("t-crash")
        self.assertEqual([m["kind"] for m in ms], ["task", "ack", "result"])
        self.assertIn("MATERIAL-CRASH", ms[-1]["body"])

if __name__ == "__main__":
    unittest.main(verbosity=2)
