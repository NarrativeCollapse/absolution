# Absolution

Absolution is a command-line operating system with a microkernel and a userspace written mostly in Rust. It has no display server, window manager or graphical applications: it boots to a text console and is driven from the shell.

This repository is the build system. It fetches the sources of each component, cross-compiles them, packages them and assembles a bootable disk image.

## What is in an image

| Image | Contents |
|-------|----------|
| `minimal` | Kernel, drivers, core utilities, the Ion shell, a text editor and the package manager |
| `server` (default) | `minimal` plus bash, curl, git, findutils, diffutils and a process monitor |
| `dev` | `server` plus compilers and development libraries for building software on the system itself |

Image definitions live in `config/`. Each one lists packages and may include another.

## Building

The build runs inside a Podman container by default, so the host needs only `git`, `make`, `podman` and FUSE:

```bash
./podman_bootstrap.sh
```

```bash
make all
```

```bash
make qemu
```

`make qemu gpu=no` runs the image on the serial console, without a QEMU window. To build on the host instead of in a container, run `./native_bootstrap.sh` and set `PODMAN_BUILD=0`.

Common settings go in a `.config` file at the root of the tree, for example:

```make
CONFIG_NAME?=minimal
ARCH?=x86_64
```

## Design

Absolution is replacing the components it started from with its own, from
the top of the stack down. [docs/DESIGN.md](docs/DESIGN.md) has the
principles and the order. Components written for Absolution live in
`components/`:

- [absinit](docs/absinit.md), the service manager.

## Layout

| Path | Purpose |
|------|---------|
| `src/` | The cookbook: the Rust tool (`repo`) that fetches, builds and packages recipes |
| `components/` | Components written for Absolution, each a Rust crate with its own tests |
| `docs/` | Design and component documentation |
| `recipes/` | One directory per package, each with a `recipe.toml` |
| `recipes/wip/` | Recipes that are incomplete or untested |
| `config/` | Image definitions |
| `mk/`, `Makefile` | The make targets that drive the cookbook, the installer and QEMU |
| `tests/` | Checks that run over every recipe and image definition |

## Recipes

A recipe says where the source comes from and how to build it:

```toml
[source]
tar = "https://example.org/hello-1.0.tar.gz"
blake3 = "<64 hex digits>"

[build]
template = "configure"
dependencies = ["zlib"]
```

Recipes are validated strictly. An unknown key, a key that does not belong to the chosen source kind or build template, or a malformed hash is an error. `cargo test` checks every recipe in the tree.

Useful commands:

```bash
make r.bash
```

```bash
make c.bash
```

```bash
./target/release/repo cook-list bash
```

The first builds one recipe, the second cleans it and the third shows what building it involves.

## Developing the cookbook

```bash
cargo fmt
```

```bash
cargo clippy --all-targets -- -D warnings
```

```bash
cargo test
```

CI runs the same three commands. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
