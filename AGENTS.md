# Working on Ordain

Read [the engineering style guide](docs/style.md) before changing code, and
[product scope](docs/product.md) when making architectural or capability decisions.

Use `mise run verify` for Rust formatting, Clippy, tests and build. For Hermes
adapter changes, also follow the native acceptance instructions in
[integrations/hermes/README.md](integrations/hermes/README.md).

Preserve unrelated work. Keep changes focused; do not perform a repository-wide
style rewrite as part of an unrelated task.
