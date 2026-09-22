# Verification status

Tests establish specific contracts, not a guarantee of correct agent output.

## Deterministic checks

`mise run verify` runs formatting, Clippy with warnings denied, Rust unit/integration
tests and a locked build. Tests use disposable repositories, isolated homes and
labelled loopback judges. They require no provider key or coding-model session.

Coverage includes:

- Git snapshots, quoted paths, same-size edits and real-index preservation.
- Rule validation, scope/policy resolution, evidence completeness and stale-result suppression.
- Partial provider failures, bounded subprocesses, corrupt/truncated history and replay ordering.
- Host installation, repair and removal without overwriting unrelated configuration.
- Preset selection, explicit updates, overrides and conflict-safe calibration.

`mise run test-hermes` additionally exercises an installed Hermes checkout's real
plugin loader, middleware, file tools and final-answer gate. Its judge is synthetic;
it verifies delivery and repair mechanics, not semantic judgement.

## Live evidence and remaining gaps

- **Codex:** bounded live TypeSafe trials demonstrated post-edit findings, Stop-driven
  repair and compliant controls. Some findings did not produce immediate repair.
- **Hermes:** actual development edits have passed through the installed integration
  and live Jev, including project and preset rules. This is activation evidence,
  not a representative accuracy benchmark.
- **Claude/OpenCode:** protocol and adapter contracts are exercised. OpenCode plugin
  callbacks were tested with a fixture client, not a full live server/model session.
  Broad compatibility across host versions remains unverified.
- **Compilation:** agent-generated rubrics need semantic review. A valid schema does
  not guarantee correct boolean polarity or useful rule wording. Validate good/bad
  examples before trusting a generated rule.
- **Presets:** labelled synthetic violations and exceptions are in
  [the fixtures](../tests/fixtures/). Live Jev evaluations informed their wording;
  these development examples are not a representative independent holdout.
- **Replay:** recorded edit reconstruction is tested; arbitrary shell changes and
  unavailable historical sources are explicitly outside its coverage.

Confidence scores are not empirically calibrated error probabilities. Observed
misses and false positives mean that successful delivery must not be advertised
as guaranteed correctness. Future evaluations should retain both violating and
legitimate controls, and measure whether repairs preserve the intended behavior.
