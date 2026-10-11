# Manifest

The manifest is a TOML file that describes the start state of one host. The control plane changes the running host;
nothing is written back to the file. `xgc2-module-host --manifest FILE --check` validates it, loads the libraries and
plans every channel without starting anything. Unknown keys are errors, and all problems of a manifest are reported
together. A complete example is [examples/entity.toml](examples/entity.toml).

Relative `path` and `socket` values are resolved against the directory of the manifest file.

## Top level

| Key | Meaning |
|---|---|
| `entity` | Entity id, `[A-Za-z0-9._:-]{1,128}`. Required. One host process serves one entity. |

## `[host]`

| Key | Default | Meaning |
|---|---|---|
| `workers` | `min(4, cores)` | Worker threads, 1 to 64. The command line `--workers` overrides it. |
| `quiesce_timeout_ms` | 2000 | How long a hot-plug change waits for the step of the instance it touches. |
| `op_timeout_ms` | 10000 | How long the control plane waits for one lifecycle call of a module. |

## `[clock]`

| Key | Meaning |
|---|---|
| `mode` | `"steady"` (default, CLOCK_MONOTONIC) or `"external"`. |
| `channel` | With `external`: the state channel of schema `xgc2.clock.v1` that carries the time. The module that writes it is bound to this channel like any other. Not allowed with `steady`. |

## `[control]`

| Key | Meaning |
|---|---|
| `socket` | Unix socket of the XRPC control plane. Its directory must be owned by the user and have mode 0700. `--control-socket` overrides it. Without one, the host runs without a control plane. |

## `[[module]]`

One table per library. A library file is loaded once.

| Key | Meaning |
|---|---|
| `name` | Handle that instances use, a name of up to 64 characters (`[A-Za-z0-9]`, then `._/-`). Required. |
| `path` | The shared library. Required. |
| `sha256` | Optional pin, 64 hex digits. The file is hashed before it is loaded and a mismatch refuses it. |

The descriptor of the library (module name, version, ports) is read at load time and shown by `GET /v1/modules`.

## `[[channel]]`

Optional overrides. A channel exists as soon as a port is bound to it and takes its payload type from the first port;
this table only sets capacities for channels with these names.

| Key | Meaning |
|---|---|
| `name` | Channel name. |
| `depth` | Event channels: queue length, 1 to 65536; at least what every port bound to the channel asks for (without this key the largest `queue_depth` among the manifest's ports is used). |
| `max_readers` | Readers that can be attached at the same time, 1 to 64 (default 8). A state channel has `max_readers + 2` slots. The slots of one channel may take at most 64 MiB. |

## `[[instance]]`

Instances start in document order. List the consumers of an event channel before its producers if the events that are
published while the entity starts must not be missed.

| Key | Default | Meaning |
|---|---|---|
| `name` | | Instance name. Required, unique. |
| `module` | | A `[[module]]` name. Required. |
| `affinity` | `"sticky"` | `"sticky"` keeps all lifecycle calls and steps on one pool worker; `"any"` permits dispatch on any worker. |
| `period_ms` | none | Period timer in milliseconds (a float is fine), up to 3600000. Without it the instance runs only for inputs, `wake()` and configure. |
| `step_budget_ms` | one period, else 50 | A longer step marks the instance `degraded`. |
| `hang_limit_ms` | ten budgets, at least 100 | A module call longer than this isolates the instance. At least 20 and not below the budget. |
| `required` | `true` | A required instance has to be running for `describe` to report ready. |
| `autostart` | `true` | `false` creates the instance and leaves it stopped until `instances/start`. |
| `[instance.config]` | `{}` | A TOML table, passed to `create` and `configure` as a JSON object text. Dates become RFC 3339 strings; NaN and infinity are errors. |
| `[instance.bind]` | | `port = "channel"` for every port that is not private. Output ports left out get a private channel `<instance>.<port>`; input ports left out stay unconnected. |

Sticky affinity supports thread-affine libraries such as xgc2-state-machine, which binds a machine to the first
thread that updates it, and retains worker cache locality without a thread per module. The home worker is the slot
with the fewest sticky instances when the instance is added (lowest slot on ties); it stays fixed across stop/start
and replacement. `instances/replace` preserves affinity when omitted and accepts an explicit change. Changing an
`any` instance to sticky chooses a home slot then.

Balanced sticky instances in a chain can occupy different workers, so each hop wakes the next worker. With `any`,
run-next lets a chain execute on one worker with fewer wake-ups and lower handoff cost. Instances sharing a sticky
worker wait for each other's calls, including a hung call until the watchdog replaces that worker in the same slot.
That recovery changes the OS thread; a failed replacement spawn lets surviving workers serve the orphaned lane.
See the measured sticky/any comparison in [validation/2026-10-10.md](validation/2026-10-10.md#benchmark).

## What is checked, and where

* Syntax, unknown keys, names, numbers, duplicates, references from instances to modules: when the manifest is parsed.
* Libraries (file, pin, entry point, descriptor), ports named in `bind`, channel compatibility (kind, schema id, size,
  align, event depth, one writer per state channel, reader capacity): at start and by `--check`.
* A required input without a producer is not an error: the entity just is not ready until one exists (`--check`
  prints it as a warning).
