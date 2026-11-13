# Absolution design

Absolution is a command-line operating system: a microkernel, a small set of
system services, and a shell-driven userland. This document sets the
direction for replacing the components inherited at the start of the project
with components designed for Absolution, and the rules those components
follow.

## Principles

1. **Text interfaces only.** Every service, tool and configuration is usable
   from a terminal over a serial line. Nothing assumes a display.
2. **Strict, declarative configuration.** Configuration is TOML read with
   unknown keys rejected. A typo is an error at load time, never a silently
   ignored setting.
3. **Degrade, don't halt.** A broken configuration file or a failing service
   is reported and skipped; the rest of the system still comes up. Only the
   kernel may refuse to boot.
4. **POSIX at the edges.** Components talk to each other and to programs
   through POSIX interfaces (processes, pipes, signals, files), so they can
   be developed and tested on any POSIX host before running on Absolution.
5. **Logic separate from effects.** Each component keeps its decisions in
   pure code that is tested without processes, devices or clocks, and keeps
   the code that touches the system thin.
6. **Owned and built here.** New components live in `components/` in this
   repository and are packaged by recipes with a `path` source. Nothing
   Absolution depends on long-term should be fetched from another project.

## Replacement order

Components are replaced from the top of the stack down. Each step leaves a
bootable system, because the components below keep their interfaces.

| Step | Component | Replaces | Status |
|------|-----------|----------|--------|
| 1 | `absinit`: service manager and init | inherited `init` (in `base`) | Written and tested on Linux; packaged, not yet in an image or PID 1 |
| 2 | Login and session handling | `userutils` | Planned |
| 3 | Shell (POSIX `sh`) | `ion` as default shell | Planned |
| 4 | Package manager | `pkgutils` | Planned |
| 5 | Logging and device services | daemons in `base` | Planned |
| 6 | Drivers | drivers in `base` | Planned |
| 7 | C library | `relibc` | Planned |
| 8 | Kernel and bootloader | `kernel`, `bootloader` | Planned |

The C library and kernel come last because everything else depends on their
interfaces. Replacing them also removes the last reasons for the
`*-unknown-redox` compiler target, which needs its own compiler port to
rename.

## Switching a component over

A new component first ships alongside the one it replaces, so it can be
tried without risk (for `absinit`, as `/usr/bin/absinit`). It becomes the
default only after an image using it boots to a shell in QEMU on every
supported architecture. The old component is then removed from the image
configs and, once nothing depends on it, from the recipes.

## Components

- [absinit](absinit.md): the service manager.
