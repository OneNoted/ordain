"""Local Hermes adapter. Rule evaluation and durable review state belong to Ordain."""

from dataclasses import dataclass
import json
import logging
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
from threading import RLock
from uuid import uuid4

MAX_SOURCE_BYTES = 16 * 1024 * 1024
EVENTS = {
    "session-start": "SessionStart",
    "turn-start": "UserPromptSubmit",
    "post-tool-use": "PostToolUse",
    "stop": "Stop",
}


def local_backend(task_id):
    from tools.terminal_tool import _get_env_config, resolve_task_overrides

    return (resolve_task_overrides(task_id).get("env_type") or _get_env_config()["env_type"]) == "local"


def read_source(path):
    """Never infer an empty before-image from an unreadable file."""
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except FileNotFoundError:
        return None
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("not a regular file")
        data = stream.read(MAX_SOURCE_BYTES + 1)
    if len(data) > MAX_SOURCE_BYTES:
        raise ValueError("source exceeds capture limit")
    return data.decode("utf-8")


def feedback(output):
    # systemMessage is operator-facing, not an instruction to repair or continue.
    if output.get("systemMessage"):
        logging.getLogger(__name__).warning("%s", output["systemMessage"])
    parts = [output.get("reason") if output.get("decision") == "block" else None,
             output.get("hookSpecificOutput", {}).get("additionalContext")]
    return "\n".join(dict.fromkeys(p for p in parts if p))


def attach(result, message):
    if not message:
        return result
    try:
        value = json.loads(result)
    except (TypeError, ValueError):
        return f"{result}\n\n[Ordain review]\n{message}"
    if isinstance(value, dict) and "ordain" not in value:
        value["ordain"] = {"feedback": message}
        return json.dumps(value, ensure_ascii=False)
    return f"{result}\n\n[Ordain review]\n{message}"


@dataclass(frozen=True)
class Turn:
    session_id: str
    turn_id: str
    task_id: str


class Adapter:
    def __init__(self, binary, project):
        self.binary = str(Path(shutil.which(binary) or binary).expanduser().resolve(strict=True))
        self.root = Path(project).expanduser().resolve(strict=True)
        if not self.root.is_dir():
            raise ValueError("Ordain project must be a directory")
        self.turns = {}
        # Serializes this adapter's explicit edits, not arbitrary external writers.
        self.lock = RLock()

    def invoke(self, command, session, turn, **payload):
        payload.update(hook_event_name=EVENTS[command], session_id=session,
                       turn_id=turn, cwd=str(self.root))
        try:
            proc = subprocess.run(
                [self.binary, "__hook", command], input=json.dumps(payload),
                text=True, capture_output=True, cwd=self.root, timeout=35,
            )
            if proc.returncode:
                raise ValueError("hook process failed")
            output = json.loads(proc.stdout) if proc.stdout.strip() else {}
            if not isinstance(output, dict):
                raise ValueError("invalid hook response")
            return output
        except (OSError, ValueError, subprocess.TimeoutExpired):
            # Do not expose stderr: a third-party endpoint can echo credentials.
            return {"hookSpecificOutput": {"hookEventName": EVENTS[command], "additionalContext":
                "Ordain review incomplete: hook unavailable or invalid response; no clean result was established."}}

    def start(self, session_id, user_message, task_id=None, **kwargs):
        task_id = task_id or session_id
        if not local_backend(task_id):
            self.finish(session_id)
            return {"context": "Ordain is inactive: this adapter supports local workspaces only."}
        turn = uuid4().hex
        with self.lock:
            self.finish(session_id)
            self.turns[session_id] = Turn(session_id, turn, task_id)
            notice = self.invoke("session-start", session_id, turn)
            baseline = self.invoke("turn-start", session_id, turn, prompt=user_message)
        text = "\n".join(filter(None, [feedback(notice), feedback(baseline)]))
        return {"context": text} if text else None

    def paths(self, tool_name, args):
        if tool_name == "write_file" or (tool_name == "patch" and args.get("mode", "replace") == "replace"):
            names = [args.get("path", "")]
        elif tool_name == "patch":
            names = re.findall(r"^\*\*\* (?:Add File|Update File|Delete File|Move to): (.+)$",
                               args.get("patch", ""), re.MULTILINE)
        else:
            return []
        paths = []
        for name in names:
            path = Path(name).expanduser()
            # Hermes may resolve relative paths against a task-specific backend.
            # Never guess from the gateway process cwd; Stop covers project changes.
            if not path.is_absolute():
                continue
            parent = path.parent.resolve()
            if parent.is_relative_to(self.root):
                paths.append(parent / path.name)
        return list(dict.fromkeys(paths))

    def execute(self, tool_name, args, next_call, session_id="", task_id=None, **kwargs):
        if not local_backend(task_id or session_id):
            return next_call(args)
        with self.lock:
            state = self.turns.get(session_id)
            if state is None and task_id:
                # Compaction changes session IDs, but preserves the running task.
                state = next((turn for turn in self.turns.values() if turn.task_id == task_id), None)
                if state is not None:
                    self.turns[session_id] = state
        paths = self.paths(tool_name, args)
        if not paths:
            return next_call(args)
        with self.lock:
            if state is None:
                return attach(next_call(args), "Ordain review incomplete: this session has no turn baseline.")
            originals, notes = {}, []
            for path in paths:
                try:
                    originals[path] = read_source(path)
                except (OSError, ValueError):
                    notes.append(f"Ordain could not capture {path.name}; edit checking is incomplete. The turn check remains eligible.")
            result = next_call(args)
            for path, before in originals.items():
                try:
                    after = read_source(path)
                except (OSError, ValueError):
                    notes.append(f"Ordain could not read {path.name} after the tool; edit checking is incomplete.")
                    continue
                if before == after:
                    continue
                if after is None:
                    notes.append(f"Ordain deferred deletion of {path.name} to the turn check.")
                    continue
                output = self.invoke(
                    "post-tool-use", state.session_id, state.turn_id, tool_name="Write",
                    tool_input={"file_path": str(path), "content": after},
                    tool_response={"originalFile": before},
                )
                notes.append(feedback(output))
            return attach(result, "\n".join(filter(None, notes)))

    def verify(self, session_id, changed_paths=(), **kwargs):
        with self.lock:
            state = self.turns.get(session_id)
            if state is None:
                return None
            if not local_backend(state.task_id):
                return None
            # A global plugin must not review this repository for an unrelated edit.
            relevant = any(path.is_absolute() and path.resolve().is_relative_to(self.root)
                           for path in map(Path, changed_paths))
            if not relevant:
                return None
            output = self.invoke("stop", state.session_id, state.turn_id)
        text = feedback(output)
        # Only core-requested context reaches this gate. Operator notices are logged
        # by feedback() and cannot consume another agent round.
        if text:
            return {"action": "continue", "message": text}
        return None

    def finish(self, session_id, **kwargs):
        with self.lock:
            state = self.turns.get(session_id)
            if state is not None:
                self.turns = {key: turn for key, turn in self.turns.items() if turn is not state}


def register(ctx):
    project = ctx.get_config("project")
    if not project:
        return
    adapter = Adapter(ctx.get_config("binary", "ordain"), project)
    ctx.register_hook("pre_llm_call", adapter.start)
    ctx.register_middleware("tool_execution", adapter.execute)
    ctx.register_hook("pre_verify", adapter.verify)
    ctx.register_hook("on_session_finalize", adapter.finish)
