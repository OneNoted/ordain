# Product scope

## Now

Ordain is a language-agnostic review layer for coding agents: instruction discovery,
rubric compilation and validation, optional presets, edit/turn checks, per-rule
policy, host integration management, calibration, audit, reporting and replay.
It evaluates source as text; it is not a compiler or a language server.

Host adapters share the same rule engine. Model findings, mechanical checks and
unsupported rules remain distinct. See [verification](verification.md) for current
acceptance limits and [engineering style](style.md) for implementation guidance.

## Later

- **Change quality:** task fit, abstraction value, verification adequacy and whether
  a test adds useful protection. Test necessity and test value are separate questions;
  no automatic deletion based on model judgement.
- **Trajectory review:** repeated failures, assumptions contradicted by evidence,
  wasteful polling and avoidable rework. Measure progress rather than raw tool counts;
  distinguish legitimate verification from stalling.

These are directions, not implemented capabilities. Any future assessment needs
bounded evidence, explicit uncertainty and actionable findings—not an aggregate
quality score or a new framework added in anticipation of future needs.
