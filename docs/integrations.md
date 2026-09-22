# Agent integrations

Manage installation independently of project rubrics and judge credentials:

```sh
ordain integration install codex
ordain integration install claude --project
ordain integration install opencode
ordain integration install hermes --workspace /absolute/path/to/repository
ordain integration status
ordain integration status codex --json
ordain integration uninstall codex
```

Installation invokes no model and requires no judge key or instruction file.
Before evaluating code, prepare the repository's instructions, compile and validate
its rubric, and configure credentials. `ordain init` remains the project-setup
convenience command with its credential/source checks and direct hook self-test;
that self-test is not proof that the host loaded the integration.

## Targeting

- Claude: `$CLAUDE_CONFIG_DIR/settings.json`, default `~/.claude/settings.json`.
- Codex: `$CODEX_HOME/hooks.json` and `config.toml`, default `~/.codex/`.
- OpenCode: `$OPENCODE_CONFIG_DIR/plugins/ordain.js` when selected, otherwise
  `$XDG_CONFIG_HOME/opencode/plugins/ordain.js`, default
  `~/.config/opencode/plugins/ordain.js`. Relative `XDG_CONFIG_HOME` is ignored.
- Hermes: `$HERMES_HOME/plugins/ordain/` and `config.yaml`, default `~/.hermes/`.
  Point `HERMES_HOME` at the exact profile directory used by Hermes. Ordain does
  not enumerate or modify other profiles. `--workspace` explicitly selects the
  repository that this profile's adapter checks; it does not select a profile.

For Claude, Codex and OpenCode, `--project` uses the current repository's `.claude`,
`.codex` or `.opencode` directory instead of the user-wide target. Hermes does not
support this switch: its adapter is profile-wide and currently checks one local
workspace. Reinstall Hermes with `--workspace` to change that workspace.

## Installation, repair and activation

Re-run `install` to update owned integration files/commands, repair missing entries
and remove stale owned hook registrations. Unrelated hooks and configuration values
remain. Unowned plugin-file collisions and malformed configuration cause an error,
not an overwrite. An exact manually copied bundled Hermes file can be adopted;
an unmarked, modified copy is refused for manual inspection rather than guessed.

Codex installation sets `[features] hooks = true` and removes the obsolete
`features.codex_hooks` key. It preserves other TOML settings and comments. Start
Codex and approve new entries in `/hooks`. Claude and OpenCode should start a new
session/process. Hermes needs a fresh process or its normal controlled gateway
restart. **Ordain never restarts a host for you.** Hermes installation grants no
built-in tool-override permission.

`status` reports the resolved target, bundled integration revision, and one of:

- `missing`: no installed Ordain integration was found.
- `current`: the on-disk configuration matches this binary's integration. For
  Hermes this includes an enabled plugin, matching binary path and existing workspace.
- `needs_repair`: owned files/registrations exist but differ, are incomplete or disabled.
- `conflict`: a plugin target contains unowned files.
- `error`: configuration could not safely be inspected.

An older integration is reported as `needs_repair`; reinstallation is its upgrade
path. Status does not run host code or inference and always reports runtime loading
as `not_verified`. It cannot certify hook trust, a running process's imported code,
project rubric readiness or credentials. A native event/feedback test is separate.
Inspection errors exit 2; other states are available in JSON without being presented
as evaluation successes or failures.

## Removal and file safety

Uninstall removes only owned hook entries or bundled plugin files. It leaves judge
credentials, project rubrics, unrelated files and Hermes plugin settings intact.
Codex's shared `features.hooks` flag remains enabled; another integration may use it.
Hermes cache files or user additions can leave its plugin directory present without
a manifest. Restart a running host to unload previously imported hooks.

Configuration writes use a same-directory temporary file and atomic replacement,
preserve existing permissions, refuse linked/non-regular target files, and detect
content changes between reading and replacement. A new file is owner-only. This is
not a lock against arbitrary concurrent host writers; avoid editing host configuration
while installing. Parent directory links are not a filesystem sandbox.

JSON and Hermes YAML are structurally parsed and rendered: unrelated **values** are
preserved, but formatting/comments in those files are not guaranteed. TOML editing
preserves unrelated formatting. Multi-file installation is not a transaction: parsing
and ownership checks happen before writes, but a later I/O error can leave a partial
installation. Errors are surfaced; inspect `status` and retry rather than assuming
activation succeeded.

## Verification

`mise run verify` exercises lifecycle contracts in isolated homes, including directory
overrides, repeated install/removal, drift repair, foreign entries, malformed config,
project scope and safe file replacement. `mise run test-hermes` exercises the installed
Hermes plugin through its real loader, file tools and final-answer gate; see the
[Hermes guide](hermes.md). The native test's loopback judge is
synthetic, not evidence of semantic judgement quality.
