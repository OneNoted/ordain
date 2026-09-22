# Optional project presets

Presets are ordinary model rules evaluated by the existing checker. Nothing is
activated by recognizing a language or installing a host adapter. There is no extra
rules engine, remote registry, dependency detector or automatic version migration.

## Select, inspect and update

```sh
ordain preset list
ordain preset show rust
ordain preset add core
ordain preset add rust
ordain preset validate
ordain config explain --rule preset-rust-borrow-inspection
```

`show` displays the bundled version. The editable selected version is
`.ordain/presets.json`; commit it alongside `.ordain/config.toml`. It stores complete
rules, source revision labels such as `preset:rust@2`, and local calibration status.
A project rubric is not required. When instructions are compiled separately, their
rules are merged with these snapshots rather than replacing them.

Adding a selected pack again is a no-op and retains edits. Binary upgrades do not
rewrite snapshots. An update previews the selected pack's old/new rules:

```sh
ordain preset update rust
ordain preset update rust --apply
ordain preset remove rust
```

Applying replaces that pack's local rule wording, statuses and calibration with the
bundled revision, including when the revision label is unchanged. Other packs and
policy are preserved. Removal/update refuses to leave policy overrides referring
to missing rule IDs; adjust those overrides explicitly first. Invalid or unreadable
snapshots fail visibly rather than silently disabling selected rules. Writes use
atomic replacement with a content conflict check, not a lock against all writers.

## Initial packs

- **core**: low-value forwarding abstractions, required failures disguised as
  success, tests without meaningful observable assertions, and sleeps substituted
  for readiness. Scope: Rust, TypeScript/JavaScript, Python and Go source extensions.
  This scope is applicability, not a claim of equal accuracy across those languages.
- **rust**: per-operation leaks used to evade lifetime ownership, and owned copies
  made solely for inspection. Deliberate process-lifetime initialization and
  independent owned snapshots are explicit exceptions.
- **typescript**: trusting unvalidated external data through type assertions, and
  duplicating or weakening an already available validator for the same contract.
  A schema-validated value and guards for unrelated metadata are legitimate.

There are eight model rules, no bundled lint executors. These are contextual review
questions, not comprehensive language style guides. Clippy, rustfmt, TypeScript,
ESLint and project-specific tooling remain responsible for mechanical checks.

## Delivery and overrides

Selected presets start as **operator notices only**, at the effective rubric `act`
threshold (normally 0.8). A notice does not instruct the agent or demand repair.
Lower scores are recorded without delivery. Scores are model confidence, not
empirically calibrated error probabilities.

Use the existing project policy to opt into agent feedback, tune confidence,
select more evidence, change applicability, or disable a rule:

```toml
[rules.preset-rust-borrow-inspection]
actions = [{ at = 0.85, action = "block" }]

[rules.preset-core-useful-abstractions]
enabled = false

[rules.preset-typescript-reuse-validation.context]
mode = "changed_files"
include = ["src/validation.ts"]
```

The include example requires that file to exist; use paths from your own project.
`steer` is the nonblocking agent-context action for post-edit checks. Host support
and turn-fallback limits are described in [configuration](configuration.md).

Project `[defaults]` then exact `[rules.ID]` overrides take precedence over preset
defaults. An explicit rule in the project rubric wins an exact ID collision, then a
global rubric rule, then the selected preset. Semantically similar rules with
*different* IDs are not deduplicated: disable the unwanted duplicate explicitly.

Presets exclude vendor, node_modules, target, dist and generated directories plus
`*.generated.*`. User exclusion arrays replace these defaults; built-in privacy
exclusions remain mandatory. Default evidence is the diff. Questions involving an
existing contract or validator may need `changed_files` or explicit related context;
missing context is not proof that an abstraction/validator has no purpose.

## Validation and calibration

`preset validate` checks snapshot structure, not semantic accuracy.
`calibrate --presets` explicitly samples local Git history and updates only the
preset snapshot; no usable history means no inference and no mutation. Incomplete
calibration preserves the snapshot. History-based weak/noisy status is a heuristic,
not a labelled accuracy study. `compile` and `tune` still target instruction-derived
rubrics; they do not rewrite curated presets. Edit the snapshot to tune a preset's
question, then validate it and evaluate both violations and legitimate exceptions.

Development controls live in `tests/fixtures/preset-cases.json`: one violation, one
compliant example and one exception per rule. Transfer and exception probes live in
`tests/fixtures/preset-transfer-cases.json`. These are labelled synthetic changes,
not representative production accuracy estimates or an independent holdout. Real
Jev checks of these controls are separate from deterministic offline lifecycle tests.

Core and Rust revision 2 separate focused classification questions from explicit
true/false criteria. Contrastive examples distinguish useless test assertions from
fixed expected outputs, and redundant owned copies from required representation
conversions. The TypeScript pack remains at revision 1. Existing selected snapshots
are not rewritten; inspect `preset update` before explicitly applying a revision.

Prompt experiments improved several boundaries, but legitimate code can still
receive middling scores. Do not lower thresholds without checking project examples
and every applicable rule's findings. These are review aids, not guaranteed leak,
redundant-copy or test-quality detectors. Missing context remains significant; the
lifetime question is narrower than general memory safety.
