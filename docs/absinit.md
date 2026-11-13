# absinit

`absinit` starts the system's services in dependency order, supervises them,
restarts the ones that fail, and stops them in reverse order at shutdown. Run
as process 1 it also reaps orphaned processes.

## Service files

Each service is one TOML file named `<name>.toml`, read from
`/usr/lib/absolution/services` (provided by packages) and then
`/etc/absolution/services` (the administrator's). A file in `/etc` replaces
the packaged file of the same name. A name is lowercase letters, digits,
`.`, `_` and `-`, starting with a letter or digit.

```toml
description = "nginx web server"
command = ["/usr/bin/nginx", "-g", "daemon off;"]
requires = ["network"]
after = ["sshd"]
ready = "started"
restart = "on-failure"
environment = { LANG = "C" }
working_directory = "/var/www"
stop_timeout = 10
```

| Key | Meaning | Default |
|-----|---------|---------|
| `description` | One line for logs | the name |
| `command` | Program and arguments. The program is an absolute path: init has no `PATH`. Without `command`, the file is a *milestone* that is reached once its requirements are ready, for grouping services. | none |
| `requires` | Services that must be ready before this one starts. If one of them fails for good, this service is skipped. Naming a service that does not exist is an error. | `[]` |
| `after` | Services to wait for if they exist, without depending on them succeeding. | `[]` |
| `ready` | When the service counts as ready: `started` (as soon as it runs), `exited` (when it exits successfully; for one-off tasks), or `notify` (when it writes a byte to the file descriptor named by `ABSOLUTION_NOTIFY_FD`). | `started` |
| `restart` | `never`, `on-failure` (after a non-zero exit or a signal), or `always`. `always` cannot be combined with `ready = "exited"`. | `on-failure`, or `never` for `ready = "exited"` |
| `environment` | Variables for the process. Nothing else is inherited. | `{}` |
| `working_directory` | Directory to start in | `/` |
| `stop_timeout` | Seconds between `SIGTERM` and `SIGKILL` at shutdown | `10` |
| `ready_timeout` | Seconds a `notify` service has to become ready before it is killed and counted as failed | `60` |
| `disabled` | `true` in `/etc` masks a packaged service of the same name | `false` |

Unknown keys, a relative program path, an empty `command`, and settings that
only make sense with a `command` on a milestone are all errors. A file with
an error is reported and its service is not started; services that require
it are skipped, and the rest of the system comes up.

## Behavior

- **Start order.** A service starts once everything it `requires` is ready
  and everything it comes `after` is ready or has failed for good. Services
  that are part of a dependency cycle are reported and skipped.
- **Readiness.** With `ready = "started"`, a service counts as ready as soon
  as its process exists, so services that depend on it may start before it
  has done anything. A service that others rely on being usable (listening
  on a socket, say) should use `ready = "notify"` and write to
  `ABSOLUTION_NOTIFY_FD` once it is.
- **Restarts.** A failed service is restarted after 1, 2, 4 and then 8
  seconds; the fifth failure in a row marks it failed and it is not
  restarted. A service that stayed up for 10 seconds counts as healthy
  again, and its count starts over.
- **Running services keep running** when something they depend on fails
  later: dependencies decide when a service starts, not how long it runs.
- **Shutdown.** On `SIGTERM` or `SIGINT`, services are stopped in reverse
  dependency order: each one receives `SIGTERM` once everything that depends
  on it has stopped, and `SIGKILL` after its `stop_timeout`.
- **Process 1.** As PID 1, absinit reaps every orphaned process, and after
  shutdown keeps reaping instead of exiting. Powering off is not
  implemented yet.

## Command line

```
absinit [--services DIR]... [--check]
```

`--services` replaces the two default directories (repeat it for several;
later ones override earlier ones). `--check` loads and validates the
services, prints the start plan and every problem, and exits non-zero if
there are problems, without starting anything.

## Status

absinit is tested on Linux, including real processes, readiness
notification, restarts and shutdown order, and compiles for the Absolution
target. The `absinit` recipe packages it as `/usr/bin/absinit`; no image
includes it yet, and it is not process 1. The boot keeps using the
inherited init until an image using absinit has been booted and verified.

The source is in [`components/absinit`](../components/absinit):
`config.rs` (service files), `plan.rs` (dependency checks), `supervisor.rs`
(all decisions, tested with a simulated clock) and `runtime.rs` (processes,
pipes and signals).
