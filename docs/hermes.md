# Hermes integration

A local-workspace plugin connecting Hermes to the existing Ordain CLI. It adds no
model, rule engine, tool, or Hermes core patch.

## Install

With Ordain installed on your PATH:

```sh
ordain integration install hermes --workspace /absolute/path/to/repository
ordain integration status hermes
```

For development, build with `mise run build` and invoke `target/debug/ordain`.
The installer bundles the plugin, enables it and stores the workspace and binary
paths in Hermes configuration. It targets `$HERMES_HOME` (default `~/.hermes`);
select the exact profile directory used by Hermes, not a different profile.
Re-run installation to upgrade or repair. It requires no judge key or model call.
See [integration lifecycle and file safety](integrations.md).

Prepare that repository's Ordain rubric and credentials normally. Use
`ordain compile --print` to obtain the compilation instructions for the current
agent, then validate and calibrate the resulting rubric. A repository without
commits skips calibration without requiring credentials; this is not a calibrated
rubric. Configuration, rule thresholds, evidence selection, exclusions and repair
policy remain owned by Ordain. No credential belongs in plugin settings.

The installer explicitly grants no tool-override permission; the adapter uses
middleware and does not need permission to replace built-in tools.

Validate with `hermes plugins doctor /path/to/plugin --ci`. Start a fresh Hermes
process after enabling. A running gateway requires its normal controlled restart;
changing configuration does not load code into an existing conversation.

## Delivery

- `pre_llm_call` establishes the repository's turn baseline.
- Native `tool_execution` middleware captures absolute-path `write_file` and
  `patch` changes, then calls Ordain's existing post-tool hook. Feedback is added
  under `ordain.feedback` in the original JSON result; success/error fields remain
  intact. The original tool executes exactly once. An edit has already happened:
  feedback requests repair, it does not undo the edit.
- Native `pre_verify` invokes the existing turn check for edits in the configured
  repository. A finding returns a continuation request through Hermes' native
  final-answer gate. Hermes' `agent.max_verify_nudges` and Ordain's own repair
  policy bound retries. This is not an unbypassable security gate.
- The running task identity reconnects middleware calls after a Hermes compaction
  changes the session ID. The original Ordain baseline and repair budget are kept;
  a new session ID alone never creates a replacement baseline.
- `on_session_finalize` releases the turn and its session aliases.

## Boundaries

This first adapter supports one configured local repository per process. Remote
backends are not inspected through local paths. Relative paths, shell edits and
deletions rely on the turn baseline rather than guessed before-images. **The turn
backstop only runs when Hermes records a code mutation in that repository**;
arbitrary commands or opaque tools that escape Hermes' mutation tracking are not
claimed as covered. Absolute paths are recommended for immediate feedback.

Unreadable, non-UTF-8, symlinked or oversized before-images are not invented.
Capture is capped at 16 MiB per file and each Ordain invocation at 35 seconds;
incomplete review is visible, not a pass. Operator-only notices (`systemMessage`)
are logged and do not enter model context or request another agent round. Blocking
reasons and explicit agent context (including incomplete-review diagnostics) remain
visible. At the final gate, only that agent-directed feedback requests continuation.
A completed turn check with no agent-directed feedback closes Ordain's turn state.
Incomplete checks retain it so a continuation can be checked against the same
baseline. Hermes and Ordain's existing retry limits still apply.

Do not run concurrent independent agents in the same worktree and assume edits
can be attributed perfectly. Use separate worktrees. The adapter serializes its
own explicit file operations, not arbitrary outside writers.

Hermes' middleware and verification APIs are required. Backend detection also
uses its native task overrides and `_get_env_config`; test against the actual
Hermes installation when upgrading rather than duplicating its backend semantics.

## Uninstall

Run `ordain integration uninstall hermes` with the same `HERMES_HOME`.
Start a fresh process (or use the gateway's normal controlled restart); removal
cannot unload hooks from an already running process. Ordain retains plugin settings
for reinstallation and removes only its bundled assets and enable/disable entries.
Unrelated files, project rubrics and credentials remain. Python cache files can
leave the plugin directory present without its manifest.

## Test

```sh
HERMES_SOURCE=/path/to/hermes-agent \
HERMES_PYTHON=/path/to/hermes-agent/venv/bin/python mise run test-hermes
```

The test installs the plugin in an isolated Hermes home, uses real Hermes file-tool
dispatch and its final-answer nudge path, and exercises the real Ordain binary.
The loopback judge is explicitly synthetic and proves adapter contracts, not
semantic judgement. No coding-model session or provider credential is used.
