# Project policy and evidence

Checking policy lives in the repository's `.ordain/config.toml`. Generated questions,
source references and compilation metadata stay in `.ordain/rubric.json`. Compiling
or tuning a rubric must not overwrite the policy file.

```sh
ordain config validate
ordain config explain --rule no-panic-on-malformed-user-syntax
ordain config explain --json
```

These commands run offline without credentials. Explain shows effective settings,
field origins, policy revisions and resolved storage directories. Unknown fields,
invalid values and overrides for missing rule IDs are errors.

## Defaults and overrides

Resolution is built-in/rubric defaults (including notice-only delivery and generated/
vendor exclusions for selected [presets](presets.md)), project `[defaults]`, then the project's
exact `[rules.RULE-ID]` override. Sparse nested fields inherit; arrays replace
rather than append. There is no hidden personal rule-ID override layer, directory
config stack or matching by rule-ID glob. Rule IDs in examples must exist in the
project's merged rubric.

```toml
[defaults]
actions = [
  { at = 0.50, action = "notice" },
  { at = 0.80, action = "block" },
]
max_repairs = 2

[defaults.context]
mode = "changed_files"
max_bytes = 262144

[rules.no-panic-on-malformed-user-syntax]
phases = ["edit"]
actions = [
  { at = 0.35, action = "steer" },
  { at = 0.80, action = "block" },
]

[rules.no-panic-on-malformed-user-syntax.context]
include = ["src/parser/api.rs"]

[limits]
max_repairs = 8
max_stop_checks = 2
```

The thresholds above illustrate user choice, not calibrated recommendations.
Changing delivery thresholds does not make the judge more accurate or alter its
raw score. Different thresholds alone do not split compatible inference requests.

### Delivery

The strongest action whose threshold is met wins, including equality:

- No threshold met: record only.
- `notice`: user-visible uncertainty, not an agent instruction.
- `steer`: advisory context for the agent, without a blocking repair demand.
- `block`: a repair request using the host's blocking mechanism. The edit already
  happened; Ordain does not roll it back or write the repair itself.

Thresholds must be finite in `[0,1]`, strictly increasing, with increasing action
strength. `actions = []` records all results without delivery. `enabled = false`
skips evaluation instead. A threshold of zero warns because it delivers every score.

Nonblocking steering is implemented for post-edit additional context. Configuring
steering for the turn phase is rejected. If the turn-end fallback discovers an
edit-rule finding that requests steering, Ordain records `UNSUPPORTED_DELIVERY`
and shows a notice; it does not claim to have steered the agent or upgrade the
finding to a block. Native host behavior still depends on protocol support;
protocol-level tests do not prove that a host presents or follows advisory context.

Repair requests are bounded by both the rule's `max_repairs` (default 2) and the
turn aggregate `limits.max_repairs` (default 8). Both accept `0..=32`. Exhausted
findings remain recorded with a visible limit notice, not labelled clear or
repeated forever. `limits.max_stop_checks` defaults to 2 and accepts `1..=8`.

### Applicability and timing

Project defaults and individual rules can set:

- `enabled`: otherwise follows the rubric's active status. Explicit true can
  enable an inactive model rule; non-model classifications do not become executors.
- `scope`: repository-relative include globs; otherwise the rubric scope or `**`.
- `exclude`: additional repository-relative exclusions, default empty.
- `phases`: `["edit"]`, `["turn"]` or both; otherwise the rubric phase.
- `deadline_ms`: evaluation deadline cap, default 15000, allowed `1..=15000`.

Deadlines are also constrained by the invocation and native hook budgets. A rule
cannot extend the host's outer deadline. Built-in privacy/generated-file exclusions
remain in force regardless of user scopes or context includes.

## Evidence selection

All context settings can be project defaults or per-rule overrides:

- `mode = "diff"`: complete relevant patch. This remains the built-in default;
  full-file evidence is opt-in while its effects are evaluated on representative work.
- `mode = "changed_ranges"`: patch plus before/after ranges, with line coordinates
  and explicit partial-file status. `surrounding_lines` defaults to 20, up to 500.
- `mode = "changed_files"`: patch plus complete captured affected files, before
  and after where available. It never silently degrades into excerpts.
- `include = ["path", "glob/**"]`: required related files. Matching files are read
  once, sorted/deduplicated and shared across compatible plans. Every pattern must
  match captured, permitted evidence. Missing, excluded, linked or unsafe paths
  make the check incomplete. Includes do not override privacy exclusions.
- `unit = "file"`: independent complete file units. Built-in for edit-origin rules.
- `unit = "changeset"`: preserve the joint view across matching changed files.
  Built-in for turn-origin rules. Changing a rule's phase does not implicitly change
  its unit; configure both when its meaning requires that.
- `max_bytes`: maximum serialized provider request, including rules, task, framing
  and escaping. Default 98304 bytes (96 KiB), allowed 1024 through 1048576 (1 MiB).

The request-byte guard is an application resource bound, **not Jev's token-window
size**. Provider context rejection remains a visible evaluation error. File units
are evaluated independently; an oversized changeset is not split into fragments
that lose the relationship being judged. A required unit that does not fit returns
`CONTEXT_INCOMPLETE`, with no hidden truncation or summary. Narrow the requested
context explicitly or raise that rule's limit within the safety ceiling.

The source reader also caps a captured snapshot at 16 MiB and bounds repository
traversal. These are process-safety limits, not user inference budgets. Selectors
accept at most 64 patterns of 500 bytes each. Task capture is bounded and overlong
tasks produce incomplete evaluation rather than a seemingly complete prefix.

### Consistency and stale findings

Within an evaluation, the patch and source refer to one captured set of bytes.
Complete tool before/after evidence is used when supplied; otherwise the before
image is reconstructed from the patch and captured after image. Native edits may
reuse a previous observation only when forwarding the entire patch reproduces the
current file exactly. Otherwise reconstruction requires unique post-image anchors;
ambiguity is incomplete evidence, not a guessed before image. At turn-end the Git
tree's blob identities are also checked against captured source.

Before delivering a live result, Ordain checks whether captured source, selected
related files, rubric or policy changed. A change suppresses the stale result as
`SUPERSEDED`. Successful edit coverage is tied to a rule/policy revision, so an
older check cannot exempt a changed policy at turn-end.

This is optimistic consistency, not a filesystem transaction or a lock against
concurrent writers. Post-edit patches without an authoritative before image cannot
prove there were no unrelated changes before capture. Configuration is loaded per
hook invocation, not pinned for the whole session; changes between invocations take
effect on the next check. Ordain is not a sandbox against an agent that can rewrite
its own configuration.

Historical checks never borrow today's working files to fill missing evidence.
Git-history calibration retains each sampled commit ID and reads full before/after
files from that commit and its parent; related includes resolve against the sampled
revision. The same context modes and request-size limits still apply. Missing Git
objects (including shallow parents), non-regular files and unavailable required
context fail explicitly; calibration then leaves the rubric unchanged. Root
commits have no before-image, and merge commits are not sampled.

Imported replay and supplied patches without matching source evidence remain
incomplete for policies requiring it. A changeset is not a call graph or proof of
repository-wide usage.

Evaluation events record raw scores, effective actions, policy revisions and an
evidence manifest (snapshot fingerprint, selected paths, mode, completeness and
serialized request size), plus failures and repair-limit suppression. Source text
is not stored in the event log. Detailed ranges travel in the judge payload, not
in the compact event manifest.

## Storage conventions

- `$XDG_CONFIG_HOME/ordain/` (default `~/.config/ordain/`): user `.env` credentials
  and `global.json` rubric. No user `config.toml` settings are read yet.
- `$XDG_STATE_HOME/ordain/projects/<worktree-id>/`: private session state and
  `events.jsonl`, including bounded per-file observations for native edit chains.
  Linked worktrees have separate namespaces. Observations are private source state,
  not proof that a check succeeded; they share the session-state retention policy.
- `$XDG_CACHE_HOME/ordain/` (default `~/.cache/ordain/`): resolved cache location;
  no persistent evidence cache is currently written.
- Project `.ordain/`: checked-in rubric, optional `presets.json` snapshot and policy,
  not runtime event history.

Unset/empty XDG variables use conventional defaults; relative values are ignored.
`ORDAIN_HOME_DIR` remains an explicit alternate-home fallback when valid XDG paths
are absent. It does not override an explicitly configured XDG directory. Old
`~/.ordain` data is not automatically migrated or read. State is owner-private.
Known secret filenames are excluded; this does not detect credentials embedded
in ordinary source. See [privacy and limits](limits.md).

## Deliberate limits

This implements policy and evidence selection, not every possible configuration
surface. Transport concurrency/retries, retention, strict require-complete delivery,
transcript retrieval, automatic suppression, rule-ID semantic migration, invocation
policy overrides and a saved-verdict redecision CLI have not been added. Unsupported
keys are errors. The typed decision API can apply policy to an existing verdict
without calling the judge. Broader accuracy and confidence calibration require
separate labelled evidence; configuration alone cannot provide them.
