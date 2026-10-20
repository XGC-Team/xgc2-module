//! What the host reports about itself: readiness (`describe`), counters (`health`) and the
//! loaded libraries (`modules`). Everything here is computed from atomics and short locks; no
//! call waits for a module.

use crate::channel::Channel;
use crate::host::{ModuleHost, Topology};
use crate::instance::{Health, Instance, State};
use crate::loader::{Dir, Module};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;

impl ModuleHost {
    fn readiness(&self, topo: &Topology) -> (bool, Vec<String>, Vec<Value>) {
        let mut reasons = Vec::new();
        let mut summaries = Vec::new();
        let mut producers: HashMap<String, usize> = HashMap::new();
        for instance in topo.slots.iter().flatten() {
            if instance.state() == State::Running && instance.health() != Health::Failed {
                for port in instance.ports.iter().filter(|p| p.spec.dir == Dir::Out) {
                    if let Some(channel) = port.bound_channel() {
                        *producers.entry(channel.name().to_owned()).or_default() += 1;
                    }
                }
            }
        }
        for idx in &topo.order {
            let Some(instance) = &topo.slots[*idx as usize] else { continue };
            let mut missing = Vec::new();
            for port in instance.ports.iter().filter(|p| p.spec.dir == Dir::In && p.spec.required) {
                match port.bound_channel() {
                    None => missing.push(format!("required input {} is not bound", port.spec.name)),
                    Some(channel) if !producers.contains_key(channel.name()) => {
                        missing.push(format!("required input {} (channel {}) has no running producer", port.spec.name, channel.name()))
                    }
                    Some(_) => {}
                }
            }
            let running = instance.state() == State::Running && instance.health() != Health::Failed;
            if instance.required {
                if !running {
                    reasons.push(format!("instance {} is {}", instance.name, instance.state().as_str()));
                }
                for message in &missing {
                    reasons.push(format!("instance {}: {message}", instance.name));
                }
            }
            summaries.push(json!({
                "name": instance.name,
                "module": instance.module_handle,
                "state": instance.state().as_str(),
                "health": instance.health().as_str(),
                "required": instance.required,
                "affinity": instance.affinity.as_str(),
                "worker": instance.worker_slot(),
                "worker_moves": instance.worker_moves.load(Ordering::Relaxed),
                "ready": running && missing.is_empty(),
                "missing": missing,
            }));
        }
        if !self.core.clock.valid() {
            reasons.push("the external clock has not published a time yet".to_owned());
        }
        (reasons.is_empty(), reasons, summaries)
    }

    fn clock_json(&self) -> Value {
        json!({
            "mode": self.core.clock.mode().as_str(),
            "valid": self.core.clock.valid(),
            "now_ns": self.core.clock.now_ns(),
            "channel": self.core.options.clock.channel,
            "phase_t0_ns": self.core.clock.grid_origin(),
        })
    }

    /// Identity and readiness facts for `GET /v1/describe` (the control plane adds the
    /// service envelope): ready means every required instance is running and every required
    /// input of those has a running producer.
    pub fn describe(&self) -> (bool, Value) {
        let topo = self.topo();
        let (ready, reasons, instances) = self.readiness(&topo);
        let facts = json!({
            "entity": self.core.options.entity,
            "host_version": env!("CARGO_PKG_VERSION"),
            "abi": {"major": crate::abi::ABI_MAJOR, "minor": crate::abi::ABI_MINOR},
            "clock": self.clock_json(),
            "instances": instances,
            "modules": topo.modules.keys().collect::<Vec<_>>(),
            "not_ready": reasons,
        });
        (ready, facts)
    }

    pub fn is_ready(&self) -> bool {
        self.describe().0
    }

    /// Subscribe before checking readiness so a concurrent change cannot be missed.
    /// The request owns this receiver; cancellation drops it with the handler future.
    pub(crate) async fn wait_ready(&self, deadline: tokio::time::Instant) {
        let mut changed = self.core.readiness_changed.subscribe();
        let _ = tokio::time::timeout_at(deadline, async {
            while !self.is_ready() {
                if changed.changed().await.is_err() {
                    break;
                }
            }
        })
        .await;
    }

    pub fn health(&self) -> Value {
        let topo = self.topo();
        let (configured, live, abandoned) = self.core.sched.worker_counts();
        let instances: Vec<Value> =
            topo.order.iter().filter_map(|idx| topo.slots[*idx as usize].as_ref()).map(|i| self.instance_health(i)).collect();
        let channels: Vec<Value> = topo.channels.values().map(|channel| channel_json(channel)).collect();
        json!({
            "entity": self.core.options.entity,
            "uptime_ms": self.core.started.elapsed().as_millis() as u64,
            "clock": self.clock_json(),
            "workers": {"configured": configured, "live": live, "abandoned": abandoned, "sticky": self.core.sched.sticky_counts()},
            "instances": instances,
            "channels": channels,
        })
    }

    pub fn modules(&self) -> Value {
        let topo = self.topo();
        let list: Vec<Value> = topo
            .modules
            .iter()
            .map(|(handle, module)| {
                let users: Vec<String> = topo
                    .slots
                    .iter()
                    .flatten()
                    .filter(|instance| Arc::ptr_eq(&instance.module, module))
                    .map(|instance| instance.name.clone())
                    .collect();
                module_json(handle, module, &users, module.pinned.load(Ordering::Acquire))
            })
            .collect();
        json!({"modules": list})
    }

    pub(crate) fn instance_json(&self, instance: &Instance) -> Value {
        let params = instance.params();
        json!({
            "name": instance.name,
            "module": instance.module_handle,
            "state": instance.state().as_str(),
            "health": instance.health().as_str(),
            "required": instance.required,
            "affinity": instance.affinity.as_str(),
            "worker": instance.worker_slot(),
            "worker_moves": instance.worker_moves.load(Ordering::Relaxed),
            "period_ns": params.period_ns,
            "step_budget_ns": params.step_budget_ns,
            "hang_limit_ns": params.hang_limit_ns,
        })
    }

    fn instance_health(&self, instance: &Instance) -> Value {
        let params = instance.params();
        let cell = self.core.sched.cell(instance.idx);
        let report = instance.report();
        let ports: Vec<Value> = instance
            .ports
            .iter()
            .map(|port| {
                json!({
                    "name": port.spec.name,
                    "dir": if port.spec.dir == Dir::In { "in" } else { "out" },
                    "kind": port.spec.kind.as_str(),
                    "schema": port.spec.payload.schema,
                    "required": port.spec.required,
                    "channel": port.bound_channel().map(|c| c.name().to_owned()),
                })
            })
            .collect();
        let stats = &instance.stats;
        json!({
            "name": instance.name,
            "module": instance.module_handle,
            "library": {"name": instance.module.name, "version": instance.module.version, "sha256": instance.module.sha256},
            "state": instance.state().as_str(),
            "health": instance.health().as_str(),
            "required": instance.required,
            "affinity": instance.affinity.as_str(),
            "worker": instance.worker_slot(),
            "worker_moves": instance.worker_moves.load(Ordering::Relaxed),
            "last_error": instance.last_error(),
            "reported": {"health": report.health, "detail": report.detail},
            "period_ns": params.period_ns,
            "step_budget_ns": params.step_budget_ns,
            "hang_limit_ns": params.hang_limit_ns,
            "steps": stats.steps.load(Ordering::Relaxed),
            "step_time": stats.step_time.summary(),
            "handoff_latency": stats.handoff.summary(),
            "wakeups": cell.wakes.load(Ordering::Relaxed),
            "input_commits": cell.dirty_commits.load(Ordering::Relaxed),
            "coalesced_dirties": cell.coalesced.load(Ordering::Relaxed),
            "timer_fires": cell.timer_fires.load(Ordering::Relaxed),
            "missed_periods": cell.missed_periods.load(Ordering::Relaxed),
            "overruns": stats.overruns.load(Ordering::Relaxed),
            "step_errors": stats.step_errors.load(Ordering::Relaxed),
            "spurious_wakeups": stats.spurious.load(Ordering::Relaxed),
            "misuse": stats.misuse.load(Ordering::Relaxed),
            "ports": ports,
        })
    }
}

fn channel_json(channel: &Channel) -> Value {
    let info = channel.info();
    json!({
        "name": info.name,
        "kind": info.kind.as_str(),
        "schema": info.spec.schema,
        "size": info.spec.size,
        "align": info.spec.align,
        "depth": info.depth,
        "max_readers": info.max_readers,
        "readers": info.readers,
        "writers": info.writers,
        "commits": info.commits,
        "drops": info.drops,
        "stale": info.stale,
        "stale_reads": info.stale_reads,
        "lag": info.lag,
        "stalls": info.stalls,
    })
}

pub(crate) fn module_json(handle: &str, module: &Module, instances: &[String], pinned: bool) -> Value {
    let ports: Vec<Value> = module
        .ports
        .iter()
        .map(|port| {
            json!({
                "name": port.name,
                "dir": if port.dir == Dir::In { "in" } else { "out" },
                "kind": port.kind.as_str(),
                "schema": port.payload.schema,
                "size": port.payload.size,
                "align": port.payload.align,
                "queue_depth": port.queue_depth,
                "required": port.required,
                "async_writer": port.async_writer,
            })
        })
        .collect();
    json!({
        "module": handle,
        "name": module.name,
        "version": module.version,
        "path": module.canonical,
        "sha256": module.sha256,
        "abi": format!("{}.{}", crate::abi::ABI_MAJOR, module.abi_minor),
        "ports": ports,
        "instances": instances,
        "pinned": pinned,
    })
}
