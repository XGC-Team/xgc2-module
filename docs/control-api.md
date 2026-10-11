# Control API

XRPC `http.v1` over the Unix socket named in the manifest (`[control] socket`) or by `--control-socket`. On start the
binary prints one line of JSON with the service reference, including the `instance_id` of this boot:

```json
{"service_ref":{"service":"xgc2-module","api_version":"v1","instance_id":"3f0c...","profile":"http.v1",
                "endpoint":{"kind":"unix","address":"/run/xgc2/scout1/module.sock"}}}
```

Every call except `describe` carries that instance id (`X-Xrpc-Instance-ID`) and a request id and timeout, as the XRPC
SDKs do. Requests and responses are JSON objects; unknown request fields are refused. Errors use the XRPC vocabulary:

| Code | When |
|---|---|
| `invalid_argument` | malformed request; a binding that does not fit; a name or value that is not valid |
| `not_found` | no such instance, module or route |
| `conflict` | the name exists; a state channel already has a writer; the instance is busy; the library is in use or pinned; another control operation is running; the instance id of another boot |
| `unavailable` | the host is shutting down; a module call did not finish in time |
| `internal` | a module refused the operation (the message names the call and the status it returned) |

Reads never wait for a module. Mutations are serialized: while one runs, another fails at once with `conflict`
("another control operation is in progress") and can be retried.

## Reads

| Call | Answer |
|---|---|
| `GET /v1/describe` | `{service, api_version, instance_id, ready, facts}`. `facts`: `entity`, `host_version`, `abi`, `clock` (`mode`, `valid`, `now_ns`, `channel`), `instances` (name, module, state, health, required, affinity, worker, worker_moves, ready, missing inputs), `modules`, `not_ready` (the reasons). Needs no instance id. |
| `GET /v1/health` | `entity`, `uptime_ms`, `clock`, `workers` (`configured`, `live`, `abandoned`, `sticky`: sticky instance counts by slot), `instances` (state, health, affinity, worker, worker_moves, the module's `reported` detail, `last_error`, timing, `steps`, `step_time`, `handoff_latency`, `wakeups`, `input_commits`, `coalesced_dirties`, `timer_fires`, `missed_periods`, `overruns`, `step_errors`, `spurious_wakeups`, `misuse`, `ports` with their channel), `channels` (kind, schema, size, depth, readers, writers, `commits`, `drops`, `stale`, `stale_reads`, `lag`, `stalls`). |
| `GET /v1/modules` | the loaded libraries: handle, name, version, path, sha256, ABI, ports, the instances that use them, `pinned`. |

Instance `state` is one of `new`, `created`, `starting`, `running`, `stopping`, `stopped`, `failed`, `isolated`;
`health` is `ok`, `degraded` (over budget, or reported by the module) or `failed`.

`worker` is the last pool slot that ran the instance; `worker_moves` counts changes of worker identity between
runs, including an OS thread replaced in the same slot after a hang. A fresh replacement starts its counters at zero.

## Libraries

```text
POST /v1/modules/load     {"path": "/opt/xgc2/modules/libctl.so", "name": "ctl", "sha256": "..."}   name and sha256 optional
POST /v1/modules/unload   {"module": "ctl"}
```

`path` must be absolute. Loading answers with the module as `GET /v1/modules` lists it. A file that is already
loaded, or a handle that is taken, is a `conflict`.

## Instances

```text
POST /v1/instances/add        {"name": "ctl", "module": "ctl", "config": {...}, "period_ms": 2,
                               "step_budget_ms": 1.5, "hang_limit_ms": 200, "required": true,
                               "autostart": true, "affinity": "sticky", "bind": {"pose": "pose", "command": "cmd"}}
POST /v1/instances/remove     {"name": "ctl"}
POST /v1/instances/replace    {"name": "ctl", "module": "ctl_next", "config": {...}, "affinity": "sticky"}    all except name optional
POST /v1/instances/configure  {"name": "ctl", "config": {...}, "period_ms": 4, "step_budget_ms": 3, "hang_limit_ms": 300}
POST /v1/instances/start      {"name": "ctl"}
POST /v1/instances/stop       {"name": "ctl"}
POST /v1/bindings             {"instance": "ctl", "port": "pose", "channel": "other_pose"}     channel null disconnects
```

* `add` answers with the instance (`name`, `module`, `state`, `health`, `affinity`, `worker`, `worker_moves`, timing). All fields except `name` and
  `module` are optional and mean what they mean in the [manifest](manifest.md#instance). Failure leaves nothing
  behind.
* `replace` creates a fresh instance of `module` (default: the current one), moves the channels, the unread event
  backlog and (unless `config` is given) the configuration over, and restarts the old instance if the new one cannot
  start. The new library must be loaded first, under its own file name and handle. `affinity` is `"sticky"` or
  `"any"`; omission preserves the old affinity. Sticky replacements retain the old home slot; a change from any to
  sticky chooses the least-loaded slot. An explicit change affects the fresh instance from `create` onwards.
* `configure` takes at least one field. `config` is applied by the module between two steps (the module may refuse it:
  `internal`); the timing fields take effect from the next step on.
* `bindings` connects an input port to a channel, moves an output port, disconnects an input (`null`) or returns an
  output to its private channel (`null`). The channel must fit the port exactly.
* `start` on a failed instance is refused; stop it first.
