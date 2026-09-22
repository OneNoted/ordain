"""Offline native-host contracts. The loopback judge is a labelled fixture, not Jev.

Run with a Hermes Python environment and HERMES_SOURCE pointing at its checkout.
No production Hermes configuration or provider credentials are used.
"""
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from threading import Thread
from types import SimpleNamespace
import unittest

SOURCE = Path(os.environ["HERMES_SOURCE"]).resolve()
PRODUCT = Path(__file__).resolve().parents[2]
BINARY = PRODUCT / "target/debug/ordain"
SANDBOX = tempfile.TemporaryDirectory(prefix="ordain-hermes-")
HOME = Path(SANDBOX.name)
REPO = HOME / "repo"
REPO.mkdir()
for key in list(os.environ):
    if any(s in key for s in ("API_KEY", "AUTH_TOKEN", "SERVICE_ACCOUNT_TOKEN")):
        os.environ.pop(key)
os.environ.update(HOME=str(HOME), HERMES_HOME=str(HOME / "hermes"),
                  XDG_CONFIG_HOME=str(HOME / "config"), XDG_STATE_HOME=str(HOME / "state"),
                  XDG_CACHE_HOME=str(HOME / "cache"), TERMINAL_ENV="local", TERMINAL_CWD=str(REPO),
                  TYPESAFE_AI_API_KEY="fixture-key")
sys.path.insert(0, str(SOURCE))

REQUESTS = []


class Judge(BaseHTTPRequestHandler):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        REQUESTS.append(body)
        added = "\n".join(line for line in body["state"]["diff"].splitlines() if line.startswith("+"))
        probability = .96 if "BAD_FIXTURE" in added else .65 if "NOTICE_FIXTURE" in added else .04
        answers = {} if "ERROR_FIXTURE" in added else {
            key: {"type": "noul", "noul": probability} for key in body["questions"]}
        response = json.dumps({"answers": answers}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(response)

    def log_message(self, format, *args):
        pass


SERVER = ThreadingHTTPServer(("127.0.0.1", 0), Judge)
Thread(target=SERVER.serve_forever, daemon=True).start()
os.environ["ORDAIN_TYPESAFE_BASE_URL"] = f"http://127.0.0.1:{SERVER.server_port}"


def git(*args):
    subprocess.run(["git", "-C", str(REPO), *args], check=True, capture_output=True)


git("init", "-q")
git("config", "core.excludesFile", "/dev/null")
git("config", "user.name", "Fixture")
git("config", "user.email", "fixture@example.test")
(REPO / "sample.rs").write_text('pub fn value() -> u32 { 1 }\n')
(REPO / "AGENTS.md").write_text("Synthetic adapter fixture: do not add BAD_FIXTURE.\n")
def cli(*args):
    result = subprocess.run(args, cwd=REPO, text=True, capture_output=True, timeout=40)
    if result.returncode:
        raise AssertionError(f"{args}: {result.stdout}\n{result.stderr}")
    return result.stdout


prompt = cli(str(BINARY), "compile", "--print")
assert "AGENTS.md" in prompt and str(REPO / ".ordain/rubric.json") in prompt, prompt
assert (REPO / ".ordain/compile-skill.md").is_file()
# Hand-authored compiler output: discovery and validation are real, not model inference.
(REPO / ".ordain/rubric.json").write_text(json.dumps({
    "version": 1, "compiledAt": datetime.now(timezone.utc).isoformat(),
    "sources": [{"path": "AGENTS.md", "scope": "**/*"}],
    "rules": [{"id": "fixture-marker", "text": "Synthetic adapter fixture: do not add BAD_FIXTURE.",
               "source": {"path": "AGENTS.md", "line": 1}, "when": "edit",
               "check": {"type": "model", "question": {
                   "type": "boolean", "instructions": "Does the change add BAD_FIXTURE?",
                   "criteria": {"true": "An added BAD_FIXTURE marker.",
                                "false": "No added marker, including an unrelated edit."}}}}],
}))
cli(str(BINARY), "rubric", "validate")
cli(str(BINARY), "config", "validate")
os.environ.pop("TYPESAFE_AI_API_KEY")
calibration = json.loads(cli(str(BINARY), "calibrate", "--json"))
assert calibration["skipped"] == "no usable history" and not REQUESTS, calibration
os.environ["TYPESAFE_AI_API_KEY"] = "fixture-key"
print("Fresh rubric calibration:", json.dumps(calibration))
assert "up to date" in cli(str(BINARY), "compile", "--print")
git("add", ".")
git("commit", "-qm", "fixture: native integration baseline")

plugin = HOME / "hermes/plugins/ordain"
import yaml
config_path = HOME / "hermes/config.yaml"
config_path.parent.mkdir(parents=True, exist_ok=True)
unrelated = {"settings": {"sentinel": "keep-me"}}
config_path.write_text(yaml.safe_dump({
    "terminal": {"env_type": "local", "cwd": str(REPO)},
    "plugins": {"enabled": [], "entries": {
        "unrelated": unrelated, "ordain": {"settings": {
            "project": str(REPO), "binary": str(BINARY)}}}},
}))
cli(str(BINARY), "integration", "install", "hermes", "--workspace", str(REPO))
status = json.loads(cli(str(BINARY), "integration", "status", "hermes", "--json"))[0]
assert status["state"] == "current" and status["runtime"] == "not_verified", status
assert "ordain" in yaml.safe_load(config_path.read_text())["plugins"]["enabled"]
import model_tools
from hermes_cli.plugins import invoke_hook, has_middleware, has_hook
from agent.turn_stop_gates import _pre_verify_nudge


class NativeContracts(unittest.TestCase):
    def setUp(self):
        git("checkout", "--", "sample.rs")
        self.session = self.id()
        invoke_hook("pre_llm_call", session_id=self.session, user_message="Update the value function.")
        self.dispatch("read_file", {"path": str(REPO / "sample.rs")})

    def tearDown(self):
        invoke_hook("on_session_finalize", session_id=self.session)

    def dispatch(self, name, args):
        return model_tools.handle_function_call(name, args, task_id=self.session,
                                                session_id=self.session, tool_call_id="call-" + str(len(REQUESTS)))

    def gate(self, attempt=0):
        agent = SimpleNamespace(session_id=self.session, _resolved_is_coding=True,
                                _turn_file_mutation_paths={str(REPO / "sample.rs")})
        return _pre_verify_nudge(agent, "Done", attempt)

    def test_real_write_feedback_and_repair_preserve_result(self):
        self.assertTrue(has_middleware("tool_execution"))
        self.assertTrue(has_hook("pre_verify"))
        bad = 'pub fn value() -> u32 { 2 } // BAD_FIXTURE\n'
        result = json.loads(self.dispatch("write_file", {"path": str(REPO / "sample.rs"), "content": bad}))
        self.assertTrue(result.get("verified"), result)
        self.assertIn("fixture-marker", result["ordain"]["feedback"])
        self.assertEqual((REPO / "sample.rs").read_text(), bad)
        good = 'pub fn value() -> u32 { 2 }\n'
        result = json.loads(self.dispatch("patch", {"path": str(REPO / "sample.rs"), "old_string": bad,
                                                    "new_string": good, "mode": "replace"}))
        self.assertTrue(result.get("success"), result)
        self.assertNotIn("ordain", result)
        self.assertIsNone(self.gate())

    def test_operator_notices_do_not_reach_agent_or_continue_turn(self):
        path = REPO / "sample.rs"
        with self.assertLogs(level="WARNING") as logs:
            result = json.loads(self.dispatch("write_file", {
                "path": str(path), "content": 'pub fn value() -> u32 { 2 } // NOTICE_FIXTURE\n'}))
        self.assertTrue(result["verified"])
        self.assertNotIn("ordain", result)
        self.assertTrue(any("uncertain" in line for line in logs.output))
        # An unobserved edit forces an actual Stop evaluation rather than coverage reuse.
        path.write_text('pub fn value() -> u32 { 3 } // NOTICE_FIXTURE\n')
        with self.assertLogs(level="WARNING") as logs:
            self.assertIsNone(self.gate())
        self.assertTrue(any("uncertain" in line for line in logs.output))

    def test_incomplete_review_stays_visible_without_becoming_a_violation(self):
        result = json.loads(self.dispatch("write_file", {
            "path": str(REPO / "sample.rs"), "content": 'pub fn value() -> u32 { 2 } // ERROR_FIXTURE\n'}))
        self.assertTrue(result["verified"])
        self.assertIn("no clean result", result["ordain"]["feedback"])
        self.assertNotIn("Repair", result["ordain"]["feedback"])
        self.assertIn("no clean result", self.gate())
        (REPO / "sample.rs").write_text('pub fn value() -> u32 { 3 }\n')
        before = len(REQUESTS)
        self.assertIsNone(self.gate(attempt=1))
        self.assertGreater(len(REQUESTS), before)

    def test_explicit_steering_remains_agent_context(self):
        config = REPO / ".ordain/config.toml"
        config.write_text('[[rules.fixture-marker.actions]]\nat = 0.5\naction = "steer"\n')
        try:
            result = json.loads(self.dispatch("write_file", {
                "path": str(REPO / "sample.rs"), "content": 'pub fn value() -> u32 { 2 } // BAD_FIXTURE\n'}))
            self.assertTrue(result["verified"])
            self.assertIn("Consider", result["ordain"]["feedback"])
            self.assertNotIn("Repair", result["ordain"]["feedback"])
            path = REPO / "sample.rs"
            path.write_text('pub fn value() -> u32 { 3 } // BAD_FIXTURE\n')
            with self.assertLogs(level="WARNING") as logs:
                self.assertIsNone(self.gate())
            self.assertTrue(any("UNSUPPORTED_DELIVERY" in line for line in logs.output))
        finally:
            config.unlink()

    def test_v4a_create_and_delete(self):
        path = REPO / "added.rs"
        try:
            result = self.dispatch("patch", {"mode": "patch", "patch":
                f"*** Begin Patch\n*** Add File: {path}\n+// BAD_FIXTURE\n*** End Patch"})
            self.assertEqual(path.read_text().strip(), "// BAD_FIXTURE")
            self.assertIn("fixture-marker", json.loads(result)["ordain"]["feedback"])
            self.dispatch("patch", {"mode": "patch", "patch":
                f"*** Begin Patch\n*** Delete File: {path}\n*** End Patch"})
            self.assertFalse(path.exists())
            self.assertIsNone(self.gate())
        finally:
            path.unlink(missing_ok=True)

    def test_turn_gate_catches_unobserved_shell_edit(self):
        # Real filesystem mutation outside explicit-file middleware, like a shell command.
        (REPO / "sample.rs").write_text('pub fn value() -> u32 { 2 } // BAD_FIXTURE\n')
        output = self.gate()
        self.assertIn("fixture-marker", output)
        self.assertIsNone(self.gate(attempt=999))
        (REPO / "sample.rs").write_text('pub fn value() -> u32 { 2 }\n')
        self.assertIsNone(self.gate())

    def test_failed_edit_and_other_project_do_not_call_judge(self):
        before = len(REQUESTS)
        result = self.dispatch("patch", {"path": str(REPO / "sample.rs"), "mode": "replace",
                                          "old_string": "not present", "new_string": "BAD_FIXTURE"})
        self.assertNotIn('"ordain"', result)
        self.dispatch("write_file", {"path": str(HOME / "outside.rs"), "content": "BAD_FIXTURE"})
        self.assertEqual(len(REQUESTS), before)

    def test_remote_backend_does_not_inspect_local_paths(self):
        from tools.terminal_tool import register_task_env_overrides, clear_task_env_overrides
        import runpy
        Adapter = runpy.run_path(str(PRODUCT / "integrations/hermes/__init__.py"))["Adapter"]

        # Do not provision a remote machine: invoke middleware with a stub downstream.
        adapter = Adapter(str(BINARY), str(REPO))
        register_task_env_overrides(self.session, {"env_type": "ssh"})
        calls = []
        task = self.session + "-remote-task"
        register_task_env_overrides(task, {"env_type": "ssh"})
        try:
            # Native pre_llm_call provides a task identity distinct from the session.
            clear_task_env_overrides(self.session)
            notice = adapter.start(self.session, "Remote edit", task_id=task)
            self.assertIn("inactive", notice["context"])
            self.assertNotIn(self.session, adapter.turns)
            register_task_env_overrides(self.session, {"env_type": "ssh"})
            result = adapter.execute("write_file", {"path": str(REPO / "sample.rs")},
                                     lambda args: calls.append(args) or "remote-result",
                                     session_id=self.session, task_id=self.session)
            self.assertEqual(result, "remote-result")
            self.assertEqual(len(calls), 1)
            clear_task_env_overrides(self.session)
            clear_task_env_overrides(task)
            adapter.start(self.session, "Local edit", task_id=task)
            (REPO / "sample.rs").write_text('pub fn value() -> u32 { 3 } // BAD_FIXTURE\n')
            register_task_env_overrides(task, {"env_type": "ssh"})
            before = len(REQUESTS)
            self.assertIsNone(adapter.verify(self.session, [str(REPO / "sample.rs")]))
            self.assertEqual(len(REQUESTS), before)
        finally:
            clear_task_env_overrides(self.session)
            clear_task_env_overrides(task)

    def test_session_rotation_keeps_the_original_turn_baseline(self):
        import runpy
        Adapter = runpy.run_path(str(PRODUCT / "integrations/hermes/__init__.py"))["Adapter"]
        adapter = Adapter(str(BINARY), str(REPO))
        task = self.session + "-task"
        child = self.session + "-compacted"
        adapter.start(self.session, "Edit the function", task_id=task)
        # Hermes compaction rotates session_id, but not the running task identity.
        result = json.loads(adapter.execute(
            "write_file", {"path": str(REPO / "sample.rs"),
                           "content": 'pub fn value() -> u32 { 3 } // BAD_FIXTURE\n'},
            lambda args: model_tools.handle_function_call("write_file", args, task_id=task,
                                                         skip_tool_execution_middleware=True),
            session_id=child, task_id=task,
        ))
        self.assertTrue(result["verified"])
        self.assertIn("Repair", result["ordain"]["feedback"])
        (REPO / "sample.rs").write_text('pub fn value() -> u32 { 4 } // BAD_FIXTURE\n')
        self.assertIn("Repair", adapter.verify(child, [str(REPO / "sample.rs")])["message"])
        (REPO / "sample.rs").write_text('pub fn value() -> u32 { 4 }\n')
        before = len(REQUESTS)
        self.assertIsNone(adapter.verify(child, [str(REPO / "sample.rs")]))
        self.assertGreater(len(REQUESTS), before)
        adapter.finish(child)
        self.assertFalse(adapter.turns)

    def test_symlink_not_read_as_before_image(self):
        link = REPO / "linked.rs"
        target = HOME / "secret.rs"
        target.write_text("private fixture data")
        link.symlink_to(target)
        try:
            before = len(REQUESTS)
            result = self.dispatch("write_file", {"path": str(link), "content": "replacement"})
            self.assertIn("could not capture", result)
            self.assertEqual(len(REQUESTS), before)
        finally:
            link.unlink(missing_ok=True)


def tearDownModule():
    before = yaml.safe_load(config_path.read_text())
    other = plugin.parent / "unrelated-data"
    other.mkdir()
    sentinel = other / "sentinel"
    sentinel.write_text("keep-me")
    cli(str(BINARY), "integration", "uninstall", "hermes")
    after = yaml.safe_load(config_path.read_text())
    assert not (plugin / "plugin.yaml").exists()
    assert "ordain" not in after["plugins"]["enabled"]
    assert after["plugins"]["entries"]["unrelated"] == unrelated
    assert after["terminal"] == before["terminal"]
    assert sentinel.read_text() == "keep-me"
    # A running process keeps imported hooks: verify removal in a fresh process.
    cli(sys.executable, "-c", "import model_tools; from hermes_cli.plugins import has_middleware; "
        "assert not has_middleware('tool_execution')")
    print("Fresh-user lifecycle: discover, validate, calibrate, integration install/status, native feedback, integration uninstall: passed")


if __name__ == "__main__":
    try:
        unittest.main()
    finally:
        SERVER.shutdown()
        SERVER.server_close()
        SANDBOX.cleanup()
