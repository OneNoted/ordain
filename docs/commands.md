# Commands

Use `ordain --help` and `ordain <command> --help` for complete arguments and defaults.

## Setup and rules

- `login`: securely store a judge key.
- `integration install|status|uninstall`: manage native [host adapters](integrations.md).
- `init`: convenience setup with credential/source checks and a direct hook self-test.
- `compile`: turn instructions into a rubric through a coding agent; `--print` avoids launching Claude.
- `rubric validate`: validate the rubric and its source references.
- `preset list|show|add|update|remove|validate`: manage [optional snapshots](presets.md).
- `config validate|explain`: inspect [effective policy](configuration.md), offline.
- `calibrate`: sample Git history and mark weak/noisy model rules. `--presets` targets
  only selected presets; `--global` targets personal rules. Failed evaluation leaves
  the previous rubric unchanged. History calibration is a heuristic, not an accuracy study.
- `tune`: ask a coding agent to rewrite weak/noisy instruction-derived rules.

Only active model rules are evaluated by default. `lint`, `deferred` and
`unenforceable` entries are classifications, not checks Ordain executes.

## Review and inspect

```sh
ordain check                       # staged, unstaged and untracked changes
ordain check --diff change.patch   # supplied patch; no invented original source
ordain audit src                   # existing in-scope files, in bounded chunks
ordain report --json               # findings, failures and history completeness
ordain replay codex --repo .       # explicitly evaluate recorded edits
ordain bench                      # labelled synthetic checks; uses your judge key
```

Audit uses 150-line added-file chunks, not a whole-repository semantic analysis.
Reports expose skipped/error outcomes and bounded-history truncation as well as
findings. Benchmark calls measure implementation behavior, not judge accuracy.

Replay supports Claude JSONL, Codex rollout JSONL and OpenCode's read-only SQLite
database (requires `sqlite3`). It cannot recover arbitrary shell edits from a
transcript or substitute today's files for missing historical content. Reconstructed
turns are ordered accumulated patches, not verified final-tree diffs. Omitted edits,
malformed records and unreplayed calls are reported; a zero-verdict run is incomplete.

## Exit codes

- `check` and `audit`: **0** complete with no blocking finding; **1** blocking finding;
  **2** incomplete evaluation or infrastructure failure. A notice can accompany exit 0.
- `calibrate` and `replay`: **0** complete; **2** incomplete.
- Installed hooks: host-compatible exit **0**, with findings/errors in protocol JSON.
  The exit code alone does not indicate a clean check.

A finding carries model confidence, not an empirically calibrated probability of a
bug. [Policy](configuration.md) determines whether it is recorded, shown as a
notice, sent as steering, or delivered as a bounded repair request.
