//! The entity manifest (TOML): the start state of a host. The control plane changes it live;
//! nothing is written back.
//!
//! ```toml
//! entity = "scout1"
//!
//! [host]
//! workers = 2                      # default min(4, cores)
//!
//! [clock]
//! mode = "steady"                  # "utc", or "external" with channel = "<state channel of xgc2.clock.v1>"
//! # phase_t0_ns = 1760000000000000000   # origin of the timer grid (utc or external)
//!
//! [control]
//! socket = "/run/xgc2/scout1/module.sock"
//!
//! [[module]]
//! name = "controller"              # handle used by instances
//! path = "libugv_unicycle_controller.so"
//! sha256 = "..."                   # optional pin
//!
//! [[channel]]                      # optional overrides
//! name = "command"
//! depth = 32
//!
//! [[instance]]
//! name = "ctl"
//! module = "controller"
//! period_ms = 2
//! [instance.config]                # becomes the JSON object passed to create/configure
//! gain = 1.5
//! [instance.bind]                  # port -> channel
//! pose = "pose"
//! ```

use crate::clock::Mode;
use crate::host::{check_limits, effective_limits, ClockSpec, HostOptions, InstanceSpec};
use crate::names;
use crate::plan::{Hint, MAX_READERS_LIMIT};
use crate::scheduler::{Affinity, MAX_INSTANCES};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_PERIOD_MS: f64 = 3_600_000.0;
const MAX_WORKERS: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub entity: String,
    #[serde(default)]
    pub host: HostSection,
    #[serde(default)]
    pub clock: ClockSection,
    #[serde(default)]
    pub control: ControlSection,
    #[serde(default, rename = "module")]
    pub modules: Vec<ModuleSection>,
    #[serde(default, rename = "channel")]
    pub channels: Vec<ChannelSection>,
    #[serde(default, rename = "instance")]
    pub instances: Vec<InstanceSection>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostSection {
    pub workers: Option<usize>,
    pub quiesce_timeout_ms: Option<u64>,
    pub op_timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockSection {
    pub mode: Option<String>,
    pub channel: Option<String>,
    /// Origin of the timer grid in nanoseconds of the clock (UTC for `utc`).
    pub phase_t0_ns: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlSection {
    pub socket: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleSection {
    pub name: String,
    pub path: PathBuf,
    pub sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelSection {
    pub name: String,
    pub depth: Option<u32>,
    pub max_readers: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceSection {
    pub name: String,
    pub module: String,
    pub affinity: Option<String>,
    pub period_ms: Option<f64>,
    pub step_budget_ms: Option<f64>,
    pub hang_limit_ms: Option<f64>,
    pub required: Option<bool>,
    pub autostart: Option<bool>,
    pub config: Option<toml::Table>,
    #[serde(default)]
    pub bind: BTreeMap<String, String>,
}

/// Everything wrong with a manifest, in document order.
#[derive(Debug)]
pub struct ManifestError {
    pub problems: Vec<String>,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.problems.join("\n"))
    }
}

impl std::error::Error for ManifestError {}

fn millis_to_ns(value: f64) -> i64 {
    (value * 1e6).round() as i64
}

/// A TOML value as JSON. Dates become RFC 3339 strings; NaN and infinity have no JSON form.
pub fn toml_to_json(value: &toml::Value) -> Result<Value, String> {
    Ok(match value {
        toml::Value::String(text) => Value::String(text.clone()),
        toml::Value::Integer(number) => Value::from(*number),
        toml::Value::Float(number) => {
            Value::Number(serde_json::Number::from_f64(*number).ok_or_else(|| format!("{number} cannot be written as JSON"))?)
        }
        toml::Value::Boolean(flag) => Value::Bool(*flag),
        toml::Value::Datetime(date) => Value::String(date.to_string()),
        toml::Value::Array(items) => Value::Array(items.iter().map(toml_to_json).collect::<Result<_, _>>()?),
        toml::Value::Table(table) => {
            Value::Object(table.iter().map(|(key, item)| toml_to_json(item).map(|json| (key.clone(), json))).collect::<Result<_, _>>()?)
        }
    })
}

impl Manifest {
    /// Parse and validate. Relative paths (library files, control socket) resolve against
    /// `base`, normally the directory of the manifest file.
    pub fn parse(text: &str, base: &Path) -> Result<Manifest, ManifestError> {
        let mut manifest: Manifest =
            toml::from_str(text).map_err(|e| ManifestError { problems: vec![e.to_string().trim_end().to_owned()] })?;
        // An empty path stays empty so that validation can name it.
        for module in &mut manifest.modules {
            if module.path.is_relative() && !module.path.as_os_str().is_empty() {
                module.path = base.join(&module.path);
            }
        }
        if let Some(socket) = manifest.control.socket.as_mut() {
            if socket.is_relative() && !socket.as_os_str().is_empty() {
                *socket = base.join(&*socket);
            }
        }
        let problems = manifest.problems();
        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(ManifestError { problems })
        }
    }

    fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut note = |message: String| problems.push(message);
        if !names::valid_id(&self.entity) {
            note(format!("entity {:?} must match [A-Za-z0-9._:-]{{1,128}}", self.entity));
        }
        if let Some(workers) = self.host.workers {
            if !(1..=MAX_WORKERS).contains(&workers) {
                note(format!("host.workers {workers} outside 1..={MAX_WORKERS}"));
            }
        }
        for (key, value) in [("quiesce_timeout_ms", self.host.quiesce_timeout_ms), ("op_timeout_ms", self.host.op_timeout_ms)] {
            if value == Some(0) {
                note(format!("host.{key} must be positive"));
            }
        }
        if self.control.socket.as_ref().is_some_and(|socket| socket.as_os_str().is_empty()) {
            note("control.socket is empty".into());
        }
        match (self.clock.mode.as_deref().unwrap_or("steady"), &self.clock.channel) {
            ("steady", None) => {}
            ("steady", Some(_)) | ("utc", Some(_)) => note("clock.channel is only used with clock.mode = \"external\"".into()),
            ("utc", None) => {}
            ("external", None) => note("clock.mode = \"external\" needs clock.channel".into()),
            ("external", Some(channel)) => {
                if !names::valid_name(channel) {
                    note(format!("clock.channel {channel:?} is not a valid name"));
                }
            }
            (other, _) => note(format!("clock.mode {other:?} is not \"steady\", \"utc\" or \"external\"")),
        }
        if self.clock.phase_t0_ns.is_some() && self.clock.mode.as_deref().unwrap_or("steady") == "steady" {
            note("clock.phase_t0_ns needs clock.mode = \"utc\" or \"external\" (a steady clock has no timer grid)".into());
        }
        if self.clock.phase_t0_ns.is_some_and(|t0| t0 < 0) {
            note("clock.phase_t0_ns must not be negative".into());
        }
        let mut seen = HashSet::new();
        let mut paths = HashSet::new();
        for module in &self.modules {
            if !names::valid_name(&module.name) {
                note(format!("module {:?}: the name is not a valid name", module.name));
            }
            if !seen.insert(module.name.as_str()) {
                note(format!("module {:?} is declared twice", module.name));
            }
            if module.path.as_os_str().is_empty() {
                note(format!("module {}: path is empty", module.name));
            } else if !paths.insert(module.path.clone()) {
                note(format!("module {}: {} is already listed", module.name, module.path.display()));
            }
            if let Some(pin) = &module.sha256 {
                if !names::valid_sha256(pin) {
                    note(format!("module {}: sha256 must be 64 hex digits", module.name));
                }
            }
        }
        let mut channels = HashSet::new();
        for channel in &self.channels {
            if !names::valid_name(&channel.name) {
                note(format!("channel {:?}: the name is not a valid name", channel.name));
            }
            if !channels.insert(channel.name.as_str()) {
                note(format!("channel {:?} is declared twice", channel.name));
            }
            if let Some(depth) = channel.depth {
                if !(1..=crate::loader::MAX_QUEUE_DEPTH).contains(&depth) {
                    note(format!("channel {}: depth {depth} outside 1..={}", channel.name, crate::loader::MAX_QUEUE_DEPTH));
                }
            }
            if let Some(readers) = channel.max_readers {
                if !(1..=MAX_READERS_LIMIT).contains(&readers) {
                    note(format!("channel {}: max_readers {readers} outside 1..={MAX_READERS_LIMIT}", channel.name));
                }
            }
        }
        if self.instances.len() > MAX_INSTANCES {
            note(format!("{} instances (at most {MAX_INSTANCES})", self.instances.len()));
        }
        let mut instances = HashSet::new();
        for instance in &self.instances {
            let who = format!("instance {:?}", instance.name);
            if !names::valid_name(&instance.name) {
                note(format!("{who}: the name is not a valid name"));
            }
            if !instances.insert(instance.name.as_str()) {
                note(format!("{who} is declared twice"));
            }
            if !seen.contains(instance.module.as_str()) {
                note(format!("{who}: module {:?} is not declared", instance.module));
            }
            if let Some(affinity) = &instance.affinity {
                if affinity != "sticky" && affinity != "any" {
                    note(format!("{who}: affinity {affinity:?} must be \"sticky\" or \"any\""));
                }
            }
            let mut timing_ok = true;
            for (key, value) in
                [("period_ms", instance.period_ms), ("step_budget_ms", instance.step_budget_ms), ("hang_limit_ms", instance.hang_limit_ms)]
            {
                if value.is_some_and(|value| !value.is_finite() || value <= 0.0 || value > MAX_PERIOD_MS) {
                    note(format!("{who}: {key} must be a positive number of at most {MAX_PERIOD_MS} ms"));
                    timing_ok = false;
                }
            }
            if timing_ok {
                let (period, budget, hang) = instance.timing();
                let (budget, hang) = effective_limits(period, budget, hang);
                if let Err(message) = check_limits(budget, hang) {
                    note(format!("{who}: {message}"));
                }
            }
            for (port, channel) in &instance.bind {
                if !names::valid_port_name(port) {
                    note(format!("{who}: bind key {port:?} is not a port name"));
                }
                if !names::valid_name(channel) {
                    note(format!("{who}: bind {port} -> {channel:?} is not a valid channel name"));
                }
            }
            if let Some(config) = &instance.config {
                if let Err(message) = toml_to_json(&toml::Value::Table(config.clone())) {
                    note(format!("{who}: config: {message}"));
                }
            }
        }
        problems
    }

    pub fn clock_mode(&self) -> Mode {
        match self.clock.mode.as_deref() {
            Some("external") => Mode::External,
            Some("utc") => Mode::Utc,
            _ => Mode::Steady,
        }
    }

    pub fn host_options(&self) -> HostOptions {
        let mut options = HostOptions::new(&self.entity);
        if let Some(workers) = self.host.workers {
            options.workers = workers;
        }
        if let Some(ms) = self.host.quiesce_timeout_ms {
            options.quiesce_timeout = Duration::from_millis(ms);
        }
        if let Some(ms) = self.host.op_timeout_ms {
            options.op_timeout = Duration::from_millis(ms);
        }
        options.clock = ClockSpec { mode: self.clock_mode(), channel: self.clock.channel.clone(), phase_t0_ns: self.clock.phase_t0_ns };
        options.hints = self.channel_hints();
        options
    }

    pub fn channel_hints(&self) -> HashMap<String, Hint> {
        self.channels.iter().map(|c| (c.name.clone(), Hint { depth: c.depth, max_readers: c.max_readers })).collect()
    }

    /// Instance requests in document order.
    pub fn instance_specs(&self) -> Vec<InstanceSpec> {
        self.instances
            .iter()
            .map(|section| {
                let (period, budget, hang) = section.timing();
                let config = section.config.as_ref().map_or_else(
                    || Value::Object(Default::default()),
                    |table| toml_to_json(&toml::Value::Table(table.clone())).unwrap_or_default(),
                );
                InstanceSpec {
                    name: section.name.clone(),
                    module: section.module.clone(),
                    affinity: if section.affinity.as_deref() == Some("any") { Affinity::Any } else { Affinity::Sticky },
                    config_json: config.to_string(),
                    period_ns: period,
                    step_budget_ns: budget,
                    hang_limit_ns: hang,
                    required: section.required.unwrap_or(true),
                    autostart: section.autostart.unwrap_or(true),
                    bind: section.bind.clone(),
                }
            })
            .collect()
    }
}

impl InstanceSection {
    /// (period, budget, hang limit) in nanoseconds; 0 period means event driven.
    fn timing(&self) -> (i64, Option<i64>, Option<i64>) {
        (self.period_ms.map_or(0, millis_to_ns), self.step_budget_ms.map(millis_to_ns), self.hang_limit_ms.map(millis_to_ns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Manifest, ManifestError> {
        Manifest::parse(text, Path::new("/etc/xgc2"))
    }

    const GOOD: &str = r#"
entity = "scout1"
[host]
workers = 2
[[module]]
name = "ctl"
path = "libctl.so"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
[[channel]]
name = "command"
depth = 32
[[instance]]
name = "controller"
module = "ctl"
period_ms = 2
step_budget_ms = 1.5
[instance.config]
gain = 1.5
name = "x"
list = [1, 2]
[instance.config.nested]
on = true
[instance.bind]
pose = "pose"
"#;

    #[test]
    fn a_complete_manifest_converts_to_requests() {
        let manifest = parse(GOOD).unwrap();
        assert_eq!(manifest.modules[0].path, PathBuf::from("/etc/xgc2/libctl.so"));
        let options = manifest.host_options();
        assert_eq!((options.entity.as_str(), options.workers), ("scout1", 2));
        assert_eq!(options.hints["command"].depth, Some(32));
        let specs = manifest.instance_specs();
        assert_eq!(specs.len(), 1);
        let spec = &specs[0];
        assert_eq!((spec.period_ns, spec.step_budget_ns, spec.hang_limit_ns), (2_000_000, Some(1_500_000), None));
        assert!(spec.required && spec.autostart);
        assert_eq!(spec.bind["pose"], "pose");
        let config: Value = serde_json::from_str(&spec.config_json).unwrap();
        assert_eq!(config["gain"], 1.5);
        assert_eq!(config["list"][1], 2);
        assert_eq!(config["nested"]["on"], true);
    }

    #[test]
    fn the_shipped_example_parses() {
        let manifest = parse(include_str!("../../../docs/examples/entity.toml")).unwrap();
        assert_eq!((manifest.entity.as_str(), manifest.modules.len(), manifest.instances.len()), ("scout1", 3, 3));
        assert_eq!(manifest.instance_specs()[0].period_ns, 2_000_000);
        assert_eq!(manifest.control.socket, Some(PathBuf::from("/run/xgc2/scout1/module.sock")));
    }

    #[test]
    fn toml_values_convert_to_json() {
        let table: toml::Table =
            toml::from_str("a = 1\nb = 2.5\nc = 'x'\nd = true\ne = 1979-05-27T07:32:00Z\nf = [1, 'a']\n[g]\nh = 1\n").unwrap();
        let json = toml_to_json(&toml::Value::Table(table)).unwrap();
        assert_eq!(json["a"], 1);
        assert_eq!(json["e"], "1979-05-27T07:32:00Z");
        assert_eq!(json["g"]["h"], 1);
        let infinite: toml::Table = toml::from_str("x = inf").unwrap();
        assert!(toml_to_json(&toml::Value::Table(infinite)).unwrap_err().contains("JSON"));
    }

    #[test]
    fn syntax_and_unknown_fields_are_reported_with_a_position() {
        let error = parse("entity = ").unwrap_err().to_string();
        assert!(error.contains("line 1"), "{error}");
        let error = parse("entity = \"a\"\nbogus = 1\n").unwrap_err().to_string();
        assert!(error.contains("unknown field") && error.contains("bogus"), "{error}");
        let error = parse("entity = \"a\"\n[[instance]]\nname = \"i\"\nmodule = \"m\"\nperiod = 3\n").unwrap_err().to_string();
        assert!(error.contains("unknown field") && error.contains("period"), "{error}");
        assert!(parse("").unwrap_err().to_string().contains("entity"));
    }

    #[test]
    fn every_problem_is_listed() {
        let text = r#"
entity = "bad id"
[host]
workers = 0
op_timeout_ms = 0
[clock]
mode = "wall"
[[module]]
name = "m"
path = "a.so"
sha256 = "xyz"
[[module]]
name = "m"
path = "a.so"
[[channel]]
name = "c"
depth = 0
[[channel]]
name = "c"
max_readers = 100
[[instance]]
name = "i"
module = "missing"
period_ms = -1
hang_limit_ms = 5
[instance.bind]
Bad = "x"
ok = "bad channel"
[instance.config]
v = nan
[[instance]]
name = "i"
module = "m"
step_budget_ms = 500
hang_limit_ms = 100
"#;
        let problems = parse(text).unwrap_err().problems;
        let all = problems.join("\n");
        for expected in [
            "entity",
            "host.workers 0",
            "host.op_timeout_ms must be positive",
            "clock.mode \"wall\"",
            "module \"m\" is declared twice",
            "already listed",
            "sha256 must be 64 hex digits",
            "channel \"c\" is declared twice",
            "depth 0",
            "max_readers 100",
            "module \"missing\" is not declared",
            "period_ms must be a positive number",
            "bind key \"Bad\"",
            "channel name",
            "cannot be written as JSON",
            "instance \"i\" is declared twice",
            "hang limit must be at least",
        ] {
            assert!(all.contains(expected), "missing {expected:?} in:\n{all}");
        }
    }

    #[test]
    fn clock_rules() {
        assert!(parse("entity = \"a\"\n[clock]\nmode = \"external\"\n").unwrap_err().to_string().contains("needs clock.channel"));
        assert!(parse("entity = \"a\"\n[clock]\nchannel = \"c\"\n").unwrap_err().to_string().contains("only used with"));
        let manifest = parse("entity = \"a\"\n[clock]\nmode = \"external\"\nchannel = \"sim\"\n").unwrap();
        assert_eq!(manifest.clock_mode(), Mode::External);
        assert_eq!(manifest.host_options().clock.channel.as_deref(), Some("sim"));
    }

    #[test]
    fn utc_clock_and_phase_rules() {
        let manifest = parse("entity = \"a\"\n[clock]\nmode = \"utc\"\nphase_t0_ns = 1760000000000000000\n").unwrap();
        assert_eq!(manifest.clock_mode(), Mode::Utc);
        assert_eq!(manifest.host_options().clock.phase_t0_ns, Some(1_760_000_000_000_000_000));
        assert_eq!(parse("entity = \"a\"\n[clock]\nmode = \"utc\"\n").unwrap().host_options().clock.phase_t0_ns, None);
        let error = parse("entity = \"a\"\n[clock]\nphase_t0_ns = 5\n").unwrap_err().to_string();
        assert!(error.contains("a steady clock has no timer grid"), "{error}");
        let error = parse("entity = \"a\"\n[clock]\nmode = \"utc\"\nchannel = \"c\"\n").unwrap_err().to_string();
        assert!(error.contains("only used with"), "{error}");
        let error = parse("entity = \"a\"\n[clock]\nmode = \"utc\"\nphase_t0_ns = -1\n").unwrap_err().to_string();
        assert!(error.contains("must not be negative"), "{error}");
    }

    #[test]
    fn defaults_and_relative_socket() {
        let manifest = parse("entity = \"a\"\n[control]\nsocket = \"run/m.sock\"\n[[module]]\nname = \"m\"\npath = \"/abs/m.so\"\n[[instance]]\nname = \"i\"\nmodule = \"m\"\nautostart = false\nrequired = false\n").unwrap();
        assert_eq!(manifest.control.socket, Some(PathBuf::from("/etc/xgc2/run/m.sock")));
        assert_eq!(manifest.modules[0].path, PathBuf::from("/abs/m.so"));
        let spec = &manifest.instance_specs()[0];
        assert!(!spec.required && !spec.autostart);
        assert_eq!(spec.config_json, "{}");
        assert_eq!(spec.period_ns, 0);
    }
}
