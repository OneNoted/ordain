# Engineering style

Write ordinary, idiomatic Rust that makes the operation and its failure modes easy
to understand. Prefer the smallest sound design, not the fewest lines. These
principles also apply to host adapters; use the idioms of their implementation language.

## Readable operations

Put principal types and entry points before their supporting details. Keep related
code together. Prefer straightforward control flow and `?`; iterator combinators
are welcome when they describe a transformation more clearly than a loop. Do not
compress branching and mutation into a pipeline merely to avoid statements.
Choose names and intermediate bindings that explain the domain. No function-size,
helper-count or line-count targets.

## Abstractions earn their cost

An abstraction should name a meaningful operation, enforce an invariant, own a
resource or lifecycle, adapt a real interface, simplify callers, or remove meaningful
duplication. Do not add a wrapper that only renames an already clear operation and
moves the same arguments and result through another layer.

- Avoid: a private `execute_check(engine, input)` that only calls
  `engine.check(input)`, with no interface obligation or simplification.
- Prefer: call `engine.check(input)` directly.
- Legitimate exception: a host callback that delegates to the engine while satisfying
  the host's required interface; a public facade that shields callers from internal
  structure; a named multi-step operation even with only one caller.

Single use, forwarding, public visibility and short bodies are not themselves defects.
Do not create traits, builders or newtypes without a concrete benefit. Use mature
libraries and derives when they remove complexity rather than hide it.

## One owner for policy

Rule validation, configuration precedence, confidence decisions and evaluation
semantics belong in the core. Host adapters translate inputs, manage host-specific
lifecycles and deliver the core's outcomes; they must not independently decide the
same policy again.

- Avoid: an adapter hardcoding `probability >= 0.8` when the engine already supplies
  an action resolved using project and per-rule configuration.
- Prefer: map the engine's action to the host's feedback mechanism.
- Legitimate exception: mapping `Block` to a host-specific response, validating the
  shape of a native payload, or defining an independent transport size limit.

Similar syntax or repeated constants are not sufficient evidence of duplicated
policy. Identify the existing owner and the competing decision.

## Preserve failure and absence

Use `Option` for expected absence and `Result` for failure. Preserve the distinction
between clean, not applicable, skipped and failed checks. Add useful operation
context without repeatedly stringifying and rewrapping errors.

- Avoid: `collect_changes().unwrap_or_default()` when collection failure would then
  be reported as a successful check with no changes.
- Prefer: propagate the failure or return an explicitly incomplete outcome.
- Legitimate exception: a documented best-effort cache whose absence or failure
  falls back to the authoritative source, without claiming that the failed operation
  succeeded. A successful lookup with no match is ordinary absence.

Handle expected failures from configuration, external input, I/O and providers.
`unwrap` or `expect` may express a locally established programmer invariant; neither
is a substitute for handling an expected failure. Do not replace panics with silent
fallbacks merely to satisfy a diagnostic.

## States must mean what they say

Represent mutually exclusive states with enums when their data or valid operations
differ. Establish important invariants at construction or parsing boundaries. Do not
use a dummy value that appears ready for use while violating the type's contract.

- Avoid: a `ReadySession::default()` with an empty required session identifier,
  relying on every caller to remember to initialise it later.
- Prefer: construction that requires a valid identifier, or a separate uninitialised
  state that cannot be used as a ready session.
- Legitimate exception: an empty collection, a genuinely usable default policy, or
  an explicit `Option<ReadySession>` representing an absent session.

An empty string, zero, `Default`, or a boolean field is not inherently invalid.
Name the violated invariant and the operation that can observe the invalid state.
Do not introduce type-state machinery for ordinary local sequencing.

## Ownership and useful interfaces

Borrow data when inspecting it; accept ownership when storing or consuming it.
Return borrowed views where appropriate, and owned results when independence is
part of the contract. An owned snapshot is not an inefficient getter. Avoid deep
cloning unchanged inputs just to make plumbing convenient, but do not introduce
sprawling lifetimes to avoid a sensible copy.

Keep useful public APIs and typed, composable entry points. Visibility alone is not
bloat. Prefer standard traits and conventions; custom traits need an actual
substitution boundary. Preserve useful results rather than forcing callers to repeat
work. Do not add speculative extension frameworks, compatibility scaffolding or
premature stability promises.

## Resources and performance

Reuse expensive clients and compiled matchers where lifetimes permit it. Avoid
repeated parsing and allocation of unchanged inputs on the hook path. Bound I/O,
retries, evidence size, process lifetimes and repair loops. An operation-wide deadline
must not silently reset on each retry. Keep unrelated work outside locks.

Prefer safe, ordinary code. Measure before introducing complicated optimisation;
account for maintenance cost as well as runtime cost. Preserve unrelated user work
and protect credentials in storage, logs and submitted evidence.

## Tests and documentation

Tests should protect meaningful observable behaviour and important failure paths.
Use small fixtures and table-driven cases where they share a contract. A focused
regression test for a real defect is useful; a behaviour-preserving refactor may need
only existing coverage. Do not reproduce the production algorithm in the expected
result, assert private call choreography without a contract, or add coverage volume
for its own sake. Do not delete useful tests merely because they look repetitive.

Comments explain invariants, rationale, surprising constraints and costs, not the
syntax beneath them. Document relevant errors, panics and safety obligations. API
examples should demonstrate a useful operation, not just that a method exists.

## Tooling and review

Use Cargo through the repository's mise tasks. Formatting and Clippy handle
mechanical checks; passing them does not establish good design. Keep semantic
review grounded in the changed code and relevant surrounding contracts. State the
specific defect, not a stylistic label. Do not infer intent from a diagnostic fix.

The four example-backed sections on abstractions, policy ownership, failures and
states are candidates for local Ordain enforcement. Other sections guide engineering
review; their presence here does not imply automated coverage. Validate violation,
compliant and exception examples before activating a rule. Configuration and judge
thresholds remain separate from this style document.

## Reading

These are useful references, not additional binding rulebooks:

- [rustls contribution and style guidance](https://github.com/rustls/rustls/blob/main/CONTRIBUTING.md)
- [rust-analyzer style guide](https://rust-analyzer.github.io/book/contributing/style.html)
- [ripgrep searcher interfaces](https://github.com/BurntSushi/ripgrep/blob/master/crates/searcher/src/lib.rs)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
