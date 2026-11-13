# Contributing

## Before you send a change

Run the checks CI runs:

```bash
cargo fmt
```

```bash
cargo clippy --all-targets -- -D warnings
```

```bash
cargo test
```

`cargo test` includes `tests/recipes.rs`, which parses every recipe and image definition, so it also covers changes that touch only `recipes/` or `config/`.

## Recipes

- Pin sources. Give tar sources a `blake3` hash and git sources a `rev`. Without them a build is neither verified nor reproducible.
- A recipe belongs in `recipes/wip/` until it has been built and run. Recipes outside `wip` must parse, name only dependencies that exist and verify their archives.
- Absolution has no graphical stack. Recipes that need a display server, a GUI toolkit or a GPU library are out of scope.
- Keep patches minimal and prefer sending them upstream.

## Cookbook code

- Return errors; do not `unwrap` or `expect` on anything that can fail at run time. Add path or command context to I/O errors with `wrap_io_err!`.
- Put logic in functions that can be tested without a network or a toolchain, and test it. Git behaviour is tested against temporary repositories.
- Ask `git` for repository state rather than reading files under `.git`.
- Comments say why, not what.

## Commits

Write the subject in the imperative and explain the reason for the change in the body. Keep unrelated changes in separate commits.
