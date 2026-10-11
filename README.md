# xgc2-module

Runs the algorithm modules of **one entity** (one robot) in one process. The modules hand typed samples to each
other in memory instead of through ROS hops, an event-driven scheduler runs them, and single modules can be added,
replaced and removed while the others keep running.

Formerly xgc2-sync-runtime.

## What it is, and what it is not

xgc2-module aggregates mature first-party algorithm modules that belong to the same entity: reference generators,
controllers, and the ROS edge module of that entity. It is a small host (about 5,500 lines of Rust), a C header that
defines the module ABI, and nothing else. It contains no domain logic; products ship their own modules and their own
entity manifests.

It is **not**

* a communication stack: there is no transport between entities and no serialization inside one;
* a simulator host, a station service or a visualizer;
* a place to aggregate **drivers, calibration, estimators (for now), simulators, multi-robot visualization, type
  servers, multi-robot controllers and planners, or paper-experiment code** (the TRO, TASE and RAL DMPC work). Those
  stay separate processes and talk through ROS or XRPC as before.

One host process serves exactly one entity; several entities are never mixed in one host. ROS is only used at the
boundary of the entity, by a ROS edge module that the entity's product ships.

## Platforms

Ubuntu 18.04 (bionic), 20.04 (focal) and 24.04 (noble), amd64 and arm64. The host binary needs glibc 2.27 and is built
once against the oldest of them. Building from source needs Rust 1.85 or newer, a C and a C++ compiler for the test
modules, and cmake for the SDK package.

## Parts

| Path | What |
|---|---|
| `include/xgc2/module.h` | The module ABI (version 2.0), a C11 and C++ header. A module is a shared library that exports `xgc2_module_entry()`. |
| `sdk/` | CMake package `Xgc2Module` (target `Xgc2Module::SDK`) that installs the header; Debian package `libxgc2-module-dev`. |
| `crates/xgc2-module-host` | The host: library and the binary `xgc2-module-host`. |
| `tests/modules/` | Small C and C++ test modules, built by the integration tests. |
| `docs/` | [architecture](docs/architecture.md), [manifest](docs/manifest.md), [control API](docs/control-api.md), validation records, an example manifest. |
| `scripts/bionic-test.sh` | Builds and tests the host against the Ubuntu 18.04 sysroot (glibc 2.27). |
| `.xgc2/` | Product metadata and the Debian package scripts. |

## Using it

```text
xgc2-module-host --manifest entity.toml [--control-socket PATH] [--workers N] [--log-level LEVEL]
xgc2-module-host --manifest entity.toml --check     # load the libraries, plan the channels, start nothing
```

The manifest (see [docs/manifest.md](docs/manifest.md) and [docs/examples/entity.toml](docs/examples/entity.toml))
names the entity, the clock, the module libraries (with an optional sha256 pin), and the instances with their
configuration, period, step budgets, affinity and port-to-channel bindings. Affinity defaults to `"sticky"`: all
lifecycle calls and steps stay on one pool worker for thread-affine libraries and cache locality. `"any"` permits
migration and lets input chains use run-next across instances with fewer worker wake-ups. The process runs until SIGINT or SIGTERM. When a
control socket is configured, [`GET /v1/describe`](docs/control-api.md) answers whether the entity is ready; the optional
`wait_ready_ms=0..30000` query holds an unbound request until readiness or the wait expires, within its XRPC deadline. The
other endpoints change it live.

Writing a module: include `<xgc2/module.h>`, describe the ports (name, direction, `state` or `event`, schema id, size
and alignment of a plain struct), implement `create`, `configure`, `start`, `step`, `stop` and `destroy`, and export
`xgc2_module_entry()`. The rules a module has to follow are in the header and in
[docs/architecture.md](docs/architecture.md#module-author-notes).

## Build, test, measure

```bash
cargo build --release                  # target/release/xgc2-module-host
cargo test                             # unit tests and integration tests with real C modules (needs cc and c++)
cargo bench --bench handoff            # handoff latency and CPU of a 3-module chain at 500 Hz, sticky and any
scripts/bionic-test.sh                 # the same tests against glibc 2.27, on the glibc 2.27 loader
python3 sdk/tests/test_sdk.py --work-dir /tmp/sdk-check    # the SDK as a module author consumes it
```

Measurements and the commands behind them are recorded in [docs/validation/](docs/validation/).

The control plane uses the XRPC Rust SDK, pinned by git revision in the workspace `Cargo.toml`. To build against a
local checkout of the SDK (for example an unreleased branch) without touching the manifest or the lock file, patch it
for one invocation:

```bash
cargo test --config 'patch."https://github.com/XGC-Team/xgc2-xrpc".xgc2-xrpc.path="/path/to/xrpc/rust"'
```

The host uses `Runtime`/`RuntimeOptions`, `Host::bind`, `handler`, `Context` (`query`, `deadline`, `blocking`),
`Fault`, `Limits`, `Method`, `new_instance_id` and `BlockingClient` in its tests. Tokio watch notifications and
timers implement the describe readiness wait.

## Packages

`.xgc2/product.yml` declares the product `xgc2-module` with two Debian packages for bionic, focal and noble:

* `libxgc2-module-dev` (architecture all): `/usr/include/xgc2/module.h` and the `Xgc2Module` CMake package;
* `xgc2-module-host` (amd64, arm64): `/usr/bin/xgc2-module-host`, the documentation and the example manifest.

`.xgc2/scripts/` builds and checks them; CI builds and tests everything on every push.
