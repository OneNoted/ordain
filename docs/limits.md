# Privacy and limits

## Data sent to the judge

Ordain sends selected source evidence, rule questions and task context to TypeSafe
Jev, directly or through Vercel AI Gateway, when a command or installed hook evaluates
code. The Gateway adapter requests zero-data-retention; this is not a guarantee of
provider policy. Verify your provider's terms before using sensitive repositories.

Built-in exclusions cover secret filenames such as `.env*`, `.envrc`, `*.pem` and
`*.key`, plus Ordain state, lock files and generated/binary files. **Filename filtering
cannot detect every secret embedded in ordinary source or task text.** Configure
scope carefully; Ordain is not a data-loss-prevention boundary.

Login stores credentials in user `$XDG_CONFIG_HOME/ordain/.env` or project `.env.local`,
with owner-only permissions. It preserves unrelated lines and refuses linked or
non-regular targets. Keep project credentials out of Git.

## Local state and files

Runtime state lives under `$XDG_STATE_HOME/ordain/projects/<worktree-id>/`, outside
the repository. Session state can contain private prompts/source observations; it
expires after seven days. Event logs contain findings and evidence manifests, not
source text. History is capped at 8 MiB and compacts to a complete-line tail of at
most 4 MiB, with truncation made visible. See [storage](configuration.md#storage-conventions).

Turn snapshots use a private scratch Git index and do not stage the real index.
Incomplete or ambiguous evidence, changed source/policy, provider failures and
expired deadlines are surfaced rather than represented as approvals.
[Integration writes](integrations.md#removal-and-file-safety) preserve unrelated
configuration, but are neither a filesystem sandbox nor a multi-file transaction.

## Bounded execution

- Hook budgets: 8 seconds at session/turn start, 18 seconds after edits, 28 seconds at Stop.
- Checks: at most 1,000 files and 16 concurrent provider calls per process; separate
  processes do not share a limiter. A rubric accepts at most 512 rules.
- Audit: at most 10,000 files and 16 workers. Replay: at most 1,000 sessions,
  10,000 edits and 16 workers. Other discovery, input and response limits also apply.
- Required context never silently degrades into a truncated excerpt. Per-rule
  [evidence limits](configuration.md#evidence-selection) can be configured within safety ceilings.
- Subprocess deadlines include pipe draining. Ordain kills its process group, but
  cannot contain descendants that deliberately create another session.

## What this does not guarantee

Ordain is a review aid, not a security gate. An agent with configuration access can
change its own rules. Post-edit feedback does not undo code, and a host may not
follow a repair request. Concurrent writers in one worktree cannot be attributed
perfectly; use separate worktrees.

Keep compilers, linters, tests and human review. Judge quality depends on rule
wording and evidence; false positives and missed violations remain possible.
[Verification status](verification.md) separates delivery tests from semantic evidence.

The implementation uses Unix-specific process and filesystem operations. Linux is
locally exercised; Windows support and cross-platform timing guarantees are not claimed.
