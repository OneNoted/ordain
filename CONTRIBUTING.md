# Contributing

Start with the [engineering style guide](docs/style.md). Keep changes focused,
preserve unrelated work, and explain the behavior or defect being addressed.
For capability changes, read [product scope](docs/product.md).

```sh
mise install
mise run verify
```

Use the pinned toolchain and lockfile. Add tests for meaningful contracts and
failure modes, not implementation-shaped assertions or coverage counts. Routine
tests must use isolated repositories/configuration and no live credentials.

For Hermes adapter changes, also run the [native acceptance tests](docs/hermes.md#test)
against an installed Hermes checkout. Identify the host version and distinguish
synthetic-provider results from live semantic evidence.

Before proposing a change:

- Include the smallest useful explanation and verification commands.
- Update the relevant page in [the docs](docs/README.md), rather than duplicating it.
- Keep credentials, private traces and local runtime state out of the patch.
- Do not change another user's host configuration during testing.

Contributions are provided under the repository's [MIT license](LICENSE).
