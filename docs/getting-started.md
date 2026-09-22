# Getting started

## Install from source

Ordain currently targets Unix-like systems and requires Git. From this checkout:

```sh
mise install
mise exec -- cargo install --locked --path .
```

Mise selects the toolchain in `mise.toml`. For local development, use
`mise run build` and `target/debug/ordain` instead of installing.

## Prepare a project

Run these commands inside the Git repository you want to check:

```sh
ordain login
ordain preset add core
ordain preset validate
```

Login prompts for a TypeSafe or Vercel AI Gateway key without echoing it. Choose
user or project storage; never commit credentials. [Storage and privacy](limits.md)
explain what is saved and what is sent to the provider.

Presets work without an instruction-derived rubric. They begin as operator
notices, not agent repair requests. Add `rust` or `typescript` explicitly if wanted;
[configure delivery](presets.md#delivery-and-overrides) to request repairs.

### Use your own instructions

For rules from `AGENTS.md`, `CLAUDE.md` and related project instructions:

```sh
ordain compile --print
```

Give the printed procedure to your coding agent. It creates `.ordain/rubric.json`,
validates it and calibrates against available Git history. Review the resulting
questions against violating and compliant examples. No usable history means
calibration is skipped, not that the rules are proven accurate.

`ordain compile` without `--print` can launch a bounded Claude Code session when
Claude is available; otherwise it prints the procedure. Stale instruction sources
are also detected at session start. Use `--global` for personal instructions;
project rules take precedence on matching IDs.

Commit the project's selected `.ordain/rubric.json`, `.ordain/presets.json` and
`.ordain/config.toml` as applicable. Do not commit credentials or session state.

## Connect your coding agent

Choose one:

```sh
ordain integration install codex
ordain integration install claude --project
ordain integration install opencode
ordain integration install hermes --workspace /absolute/path/to/repository
```

Start a fresh host process/session; Codex also asks you to approve hooks in
`/hooks`. A running Hermes gateway needs its normal restart.
`ordain integration status` checks installed files, **not runtime activation**.
See [integrations](integrations.md) for profile targeting, repair and removal.

## Inspect a check

```sh
ordain check
ordain report
ordain config explain
```

Make an ordinary code change, inspect the finding and its rule, and confirm the
host receives the configured feedback. A clean result alone does not prove the
hook ran: inspect the recorded check with `report`. Error or incomplete outcomes
are not approvals. See [commands](commands.md) for JSON output and exit codes.
