# Architecture

xgc2-module runs the modules of one entity in one process. This page describes how: the data path, the scheduler,
hot-plug, the clock and the control plane. The module ABI itself is `include/xgc2/module.h`; the manifest is in
[manifest.md](manifest.md) and the control endpoints in [control-api.md](control-api.md).

```text
                    one process = one entity
+---------------------------------------------------------------------------+
| worker pool: min(4, cores) threads, one serial task per instance          |
| timer thread: period timers (deadline heap) and the 10 ms watchdog tick   |
|                                                                           |
|  +-----------+ state  +------------+ state  +----------+  ROS             |
|  | reference |------->| controller |------->| ROS edge |<-----> robot     |
|  +-----------+        +------------+        +----------+                  |
|        ^  events: requests                       |                        |
|        +-----------------------------------------+                        |
|                                                                           |
| channel slots are written in place and read in place: nothing is copied   |
| control plane: XRPC http.v1 on a Unix socket (describe, health, hot-plug) |
+---------------------------------------------------------------------------+
```

## Source map

| File (`crates/xgc2-module-host/src/`) | Responsibility |
|---|---|
| `abi.rs` | Rust mirror of `include/xgc2/module.h`; its layout is checked against the C compiler in `tests/abi_layout.rs` |
| `loader.rs` | sha256 pin, `dlopen`, descriptor validation |
| `channel.rs` | slot arenas, state and event channels, pins and cursors |
| `scheduler.rs` | worker pool, per-instance cells and affinity, worker lanes, dirty bits, run-next, watchdog |
| `timers.rs` | deadline heap, period timers, external clock hook |
| `clock.rs`, `metrics.rs`, `log.rs`, `names.rs` | host clock, latency histogram, logger, identifier grammars |
| `instance.rs`, `api.rs` | one module instance (lifecycle operations, steps, budgets, isolation) and the host function table it gives the module |
| `plan.rs` | which ports may share a channel (used by the manifest, the control plane and `--check`) |
| `host.rs`, `observe.rs` | topology and the hot-plug operations; `describe`, `health`, `modules` |
| `manifest.rs`, `launch.rs` | the TOML manifest; start from it, or check it |
| `control.rs`, `main.rs` | XRPC control plane; the binary |

## Data

**Channels.** Every output port writes into a channel; every input port reads one. A channel carries one payload type,
fixed by the first port bound to it: schema id, size and alignment must match exactly for every later port. Payloads
are plain structs without pointers, defined in the producing product's header, so nothing is serialized inside the
entity and nothing is copied: a producer fills a slot in place (`write_begin`, `write_commit`) and consumers read the
same memory (`read_latest`, `read_next`). An output that the manifest does not connect gets a private channel named
`<instance>.<port>`, so a port can always be written and a consumer can join it later.

**Slots.** A channel owns one arena. A slot is a 64-byte header (sequence, producer stamp, commit time, pin count)
followed by the payload rounded up to whole cache lines, so slots never share a cache line and payloads are 64-byte
aligned. Header and payload of a slot are adjacent in memory.

**State channels** hold the latest value. The arena has `max_readers + 2` slots (default 10): at most `max_readers`
slots are pinned by readers, one is the newest, and one is free, so the single writer never waits and a reader always
gets the newest complete sample. A reader pins the newest slot on its first read in a step and keeps it until the step
ends; a second `read_latest` in the same step returns the same sample. Pinning is an announce-and-recheck on the
`latest` index (sequentially consistent), so a slot that a reader holds is never chosen by the writer. A state channel
accepts exactly one writer.

**Event channels** are bounded broadcast FIFOs of `queue_depth` slots. Any number of writers claim positions under a
short lock, fill their slot and publish it; every reader has its own cursor and sees every event in claim order. A
slot is reused only after every reader has finished the step in which it read it, so a borrowed event stays valid for
the whole step. The queue is lossless until the slowest reader is `queue_depth` events behind; then `write_begin`
returns NULL and the drop is counted per channel. A writer that claimed a slot and has not committed yet hides later
events from the readers until it commits or aborts (FIFO order); a writer that is isolated while it holds a claim has
the claim voided so that readers skip it.

**Readers attach when an instance starts** and detach when it stops, fails or is isolated. A stopped consumer therefore
never holds slots or throttles event writers; it resumes at the newest state sample and at the head of an event queue.
A hot replace is the exception: the replacement inherits the cursors with their unread events.

**Staleness.** When the writer's instance fails, is stopped or is isolated, its channels are flagged stale. Readers
still get the last sample, and the flag shows in `health` (`stale`, `stale_reads`). The next commit clears it.

## Scheduling

**Instances are serial tasks.** Each instance owns one cell in a fixed, contiguous table, 64-byte aligned: one state
word (queued, running, paused, dead, pending reasons and home worker), the input dirty mask, the time of the oldest unprocessed
commit, and the wake counters. A pool of `workers` threads (default `min(4, cores)`) runs the tasks. An instance never
runs on two threads at once, and it keeps no thread of its own.

**Affinity.** Each instance defaults to `affinity = "sticky"`: create, configure, start, step, stop and destroy all
run on its home worker. This supports thread-affine libraries such as xgc2-state-machine (the first update binds a
machine to that OS thread) and keeps instance data in the worker's caches. The home slot is chosen at add time by
the fewest sticky instances, lowest slot on ties, without moving existing instances. Stop/start and a hot replacement
keep the slot. `affinity = "any"` permits migration between workers and retains free dispatch.

One queue mutex protects the shared queue and a lane per pool slot; each slot has its own condition variable.
Sticky tasks enter their home lane and wake only that worker. Shared tasks wake the longest-parked worker.
Instances sharing a home worker wait for each other; balanced sticky chains may cross workers at every hop, which
adds wake-ups and handoff cost. Any affinity lets a chain stay on the worker that made it runnable. The benchmark
compares these costs in [validation/2026-10-10.md](validation/2026-10-10.md#benchmark).

**Reasons.** A task becomes runnable when a reason is raised on an idle cell:

| Reason | Raised by |
|---|---|
| input | a commit on a channel one of its input ports reads (sets the port's dirty bit) |
| timer | its period timer |
| wake | `wake()`, from any thread (module-owned workers, ROS spinners) |
| lifecycle | create, configure, start, stop or destroy was requested |

The step context tells the module why it runs (`XGC2_STEP_INPUT/TIMER/WAKE/CONFIG`) and which inputs changed
(`changed_inputs`, one bit per input port in port-table order).

**Coalescing.** Reasons raised while the task is queued or running only add to the pending set. Many commits before a
step cost one step; commits during a step cost one more step, never one per commit. At step start the dirty bits are
filtered against what each input still has unread, so a commit that an earlier step already consumed causes no step.
`health` counts the commits (`input_commits`) and how many of them were absorbed (`coalesced_dirties`).

**Run-next.** A task made runnable from inside a worker can use its run-next slot when it has any affinity or that
worker is its home. A co-located sticky chain, or an any-affinity chain, therefore runs back to back without a worker
wake-up. Workers take run-next, then their own lane, then the shared queue; after 8 local tasks they serve shared
work first. The watchdog moves only any-affinity run-next tasks to the shared queue when their worker has been busy
for more than 2 ms. Sticky tasks stay on their home worker. Commit as the last thing in a step.

**Timers.** One thread keeps a deadline heap. A period timer fires at `anchor + n * period` without drift. When the
host fell behind, the skipped periods are counted (`missed_periods`) and the next deadline lies in the future: a slow
step never causes a burst of catch-up steps. A module can change its period with `set_period_ns`; 0 disables the
timer, and the shortest period is 100 microseconds.

**Step budgets.** Every instance has a step budget (default: one period, or 50 ms without a period). A step over
budget counts as an overrun and the instance's health becomes `degraded`; the next step within budget clears it. The
instance keeps running.

**Hang isolation.** The timer thread ticks a watchdog every 10 ms. A worker that stays inside one module call
(`create`, `configure`, `start`, `step`, `stop`, `destroy`) longer than the instance's hang limit (default ten budgets,
at least 100 ms; lifecycle calls at least 5 s) is abandoned:

1. the instance becomes `isolated`: its cell is dead, queued operations fail, its readers and pins are released, a
   pending event claim is voided and its outputs are flagged stale;
2. the pool starts a replacement worker in the same slot, inheriting its lane and pending run-next tasks. Co-located
   sticky instances resume there; the OS thread changes once, counted by their `worker_moves`. Other slots and shared
   tasks keep running while that slot is stuck (`workers.abandoned` counts the stuck threads). If spawning the
   replacement fails, surviving workers may serve its orphaned lane;
3. the library is pinned: it cannot be unloaded, and removing the isolated instance does not call into the module
   (the instance record is deliberately kept alive, because the stuck thread, or a thread of the module, may still
   touch it).

If the stuck call returns later, its worker exits quietly and nothing else changes. Recovery is to remove the
instance; restarting the process is only needed when an operator wants the stuck thread gone. A module that returns
`XGC2_ERR_INTERNAL` from `step`, or calls `report(2, ...)`, is `failed` instead: no more steps, outputs stale, and
`stop` followed by `start` restarts it.

## Clock

`now_ns()`, the step context and period timers share one host clock.

* `steady` (default): CLOCK_MONOTONIC.
* `external`: one designated state channel of schema `xgc2.clock.v1` (payload: one native-endian `int64_t`, time in
  nanoseconds) carries the time. The entity's ROS edge module publishes it from `/clock` in simulation. Each commit on
  that channel updates `now_ns()` and fires the timers that became due, on the committing thread, so simulated time
  can run faster or slower than the wall clock and timers follow it. Until the first sample arrives the clock is not
  valid: timers do not run and `describe` reports not ready. The first sample anchors all period timers one period
  later; time moving backwards re-anchors them.

Producer stamps (`write_commit(port, stamp_ns)`) are in the host clock domain by convention; the host does not touch
them. Latency counters in `health` always use CLOCK_MONOTONIC.

## Lifecycle and hot-plug

Lifecycle calls run on the instance's own task: `create`, `configure`, `start`, `stop` and `destroy` are queued on the
instance and executed in order on a worker, between two steps, never concurrently with a step. A live `configure`
therefore needs no pause: the module applies it between steps and the next step carries `XGC2_STEP_CONFIG`.

A change to the topology happens at a quiescent point of the instance it touches. The host pauses only that instance
(no new step is dispatched), waits for its running task to return (bounded by `quiesce_timeout`, default 2 s; else the
request fails with `conflict` and nothing changed), applies the change and resumes. All other instances keep running.
Control operations are serialized with each other; a second one is refused with `conflict` while one runs.

| Operation | What happens |
|---|---|
| load / unload a library | `load` reads the file, checks the sha256 pin, `dlopen`s with RTLD_NOW and validates the descriptor. A file that is already loaded cannot be loaded again, because `dlopen` would hand back the code that is mapped, whatever the file contains now: ship a new version under a new file name. `unload` needs the library unused and not pinned. |
| add an instance | plans the channels first (all-or-nothing), creates the instance, starts it unless `autostart` is false. Any failure leaves no instance, channel or reader behind. |
| remove an instance | quiesce, `stop`, `destroy`, unbind; channels that lose their last port disappear. An isolated instance is dropped without calling the module. |
| replace an instance | the new library version must fit every channel the old instance is bound to (same kind, schema, size, align; a port the new module lacks may only drop a private channel). The new instance is created while the old one runs; then the old one is paused and stopped, the new one takes over the channels, the unread event backlog and the configuration, and starts. If the new instance cannot start, the old one is restarted and the request fails with a message that says so. |
| rebind a port | quiesce, check the new channel against the port, move the binding; an input of a running instance attaches to the new channel at once. |
| live configure | queued on the instance; the module applies the JSON between two steps. Period and budgets change without involving the module. |

Replacing keeps the channel slots and, for sticky affinity, the home worker, so consumers see one uninterrupted
sequence of samples; only the replaced instance misses the steps between its stop and its start. An explicit
`affinity` in `instances/replace` can change it; omission preserves the current affinity. Switching from any to
sticky selects the least-loaded slot at replacement time.

## Control plane

The control plane is the XRPC `http.v1` profile on a Unix socket (the XRPC Rust SDK). The socket's directory must be
owned by the user running the host and have mode 0700; that is the access control, and it is the whole of it: a caller that can reach the socket can make the host load any library the host can read. `GET /v1/describe` is the only
call that works without the instance id of this boot, which the host generates at start and every other call must
carry (`X-Xrpc-Instance-ID`); a client that talks to a restarted host is told so with `conflict`. Reads are computed
from atomics and never wait for a module. Mutations run on the SDK's blocking pool and may take as long as a module
call; the limits are the SDK defaults (32 connections, 32 calls in flight, 1 MiB per request and response, 30 s per
call). All endpoints and their errors are in [control-api.md](control-api.md).

`describe` carries `ready`: every required instance is `running` and not failed, every required input of those has a
running producer, and an external clock has published. The reasons are listed under `facts.not_ready`.

## Observability

No line is logged per step. `GET /v1/health` reports, per instance: state and health, the module's own `report`
detail, `affinity`, `worker` (pool slot), `worker_moves` (runs on a different worker identity from the preceding run),
`steps`, `step_time` (count, p50, p99, max in ns), `handoff_latency` (oldest unprocessed input commit to step
start), `wakeups` (`wake()` calls), `input_commits`, `coalesced_dirties`, `timer_fires`, `missed_periods`, `overruns`,
`step_errors`, `spurious_wakeups` and `misuse` (calls against the API contract, such as a second `write_begin`); per
channel: `commits`, `drops`, `stale`, `stale_reads`, `lag`, readers and writers; and the worker pool
(`configured`, `live`, `abandoned`, `sticky`: sticky instance counts by slot). `describe` includes each instance's
affinity, worker slot and moves too. Percentiles come from log-linear histograms with at most 12.5% error.

## Module author notes

* All lifecycle calls and `step` run on the same host worker under the default sticky affinity, never concurrently
  for one instance. Hang recovery changes the OS thread; any affinity permits migration during normal operation. Reads
  (`read_latest`, `read_next`, `changed`) are valid only inside `step`; the views are borrowed until it returns.
* **Commit as the last thing in a step** and keep steps short: an eligible consumer can run right after the producer
  on the same worker through run-next. Do not block on another module and do not wait for input in a step; use
  `wake()` to hand work back.
* A thread that a module owns may call `wake`, `now_ns`, `log`, `report` and `set_period_ns` at any time and the write calls on ports flagged
  `XGC2_PORT_ASYNC_WRITER` (one writer thread per port). Stop the threads in `stop`; nothing may call the host after
  `destroy` returns.
* `changed_inputs` numbers the inputs on their own (bit i = i-th input port); every host call takes the index into the
  port table. An unconnected input reports `XGC2_ERR_NODATA`.
* Drain an event input with `read_next` until it reports `XGC2_ERR_NODATA`; unread events stay queued.
* `write_begin` on an event port returns NULL when the queue is full; the sample is dropped and counted. A second
  `write_begin` on a port before commit or abort also returns NULL.
* Configuration is a JSON object (UTF-8, NUL-terminated) in `create` and `configure`; keep `configure` idempotent.
* Report health with `report(health, detail)`; `2` stops the instance's steps.
* A crash in a module is a crash of the entity process: there is no memory isolation, only the isolation of time.

## Limits

| What | Value |
|---|---|
| instances per host | 128 |
| ports per module | 64 (inputs: 64) |
| payload size | 1 MiB, alignment at most 64 |
| event queue depth | at most 65536 |
| readers per channel | `max_readers`, default 8, at most 64 |
| memory of one channel | 64 MiB of slots (`queue_depth` times the rounded slot size, or `max_readers + 2` slots) |
| workers | 1 to 64 |
| shortest period | 100 microseconds |
| shortest hang limit | 20 ms |
| manifest size | 1 MiB |
