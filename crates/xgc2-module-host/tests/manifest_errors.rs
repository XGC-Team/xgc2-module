//! Manifest validation: one defect per manifest, each reported with the offending name.

use std::path::Path;
use xgc2_module_host::manifest::Manifest;
use xgc2_module_host::scheduler::Affinity;

fn parse(text: &str) -> Result<Manifest, String> {
    Manifest::parse(text, Path::new("/etc/xgc2")).map_err(|e| e.to_string())
}

/// A valid manifest with one module and one instance; `edit` swaps a fragment for a broken one.
fn with(from: &str, to: &str) -> String {
    let base = r#"entity = "e"
[host]
workers = 2
[clock]
mode = "steady"
[[module]]
name = "m"
path = "libm.so"
[[channel]]
name = "c"
depth = 8
[[instance]]
name = "i"
module = "m"
period_ms = 2
[instance.bind]
out = "c"
"#;
    assert!(base.contains(from), "fragment {from:?} not in the base manifest");
    base.replacen(from, to, 1)
}

#[test]
fn the_base_manifest_is_valid() {
    let manifest = parse(&with("workers = 2", "workers = 2")).unwrap();
    assert_eq!(manifest.instance_specs()[0].affinity, Affinity::Sticky);
}

#[test]
fn affinity_options_convert_to_instance_requests() {
    for (text, expected) in [("sticky", Affinity::Sticky), ("any", Affinity::Any)] {
        let manifest = parse(&with("period_ms = 2", &format!("period_ms = 2\naffinity = \"{text}\""))).unwrap();
        assert_eq!(manifest.instance_specs()[0].affinity, expected);
    }
}

#[test]
fn each_defect_is_named() {
    let cases: Vec<(String, &str)> = vec![
        (with("entity = \"e\"", "entity = \"bad entity\""), "entity \"bad entity\""),
        (with("entity = \"e\"", "entity = 5"), "invalid type"),
        (with("workers = 2", "workers = 0"), "host.workers 0 outside 1..=64"),
        (with("workers = 2", "workers = 65"), "host.workers 65 outside 1..=64"),
        (with("workers = 2", "workers = 2\nquiesce_timeout_ms = 0"), "host.quiesce_timeout_ms must be positive"),
        (with("workers = 2", "workers = 2\nspeed = 1"), "unknown field `speed`"),
        (with("mode = \"steady\"", "mode = \"realtime\""), "clock.mode \"realtime\""),
        (with("mode = \"steady\"", "mode = \"external\""), "needs clock.channel"),
        (with("mode = \"steady\"", "mode = \"steady\"\nchannel = \"c\""), "only used with"),
        (with("mode = \"steady\"", "mode = \"external\"\nchannel = \"bad name\""), "clock.channel \"bad name\""),
        (with("name = \"m\"\npath", "name = \"-m\"\npath"), "module \"-m\": the name is not a valid name"),
        (with("path = \"libm.so\"", "path = \"\""), "module m: path is empty"),
        (with("[clock]", "[control]\nsocket = \"\"\n[clock]"), "control.socket is empty"),
        (with("path = \"libm.so\"", "path = \"libm.so\"\nsha256 = \"abc\""), "module m: sha256 must be 64 hex digits"),
        (with("path = \"libm.so\"", "path = \"libm.so\"\nfile = 1"), "unknown field `file`"),
        (with("[[channel]]\nname = \"c\"\ndepth = 8", "[[channel]]\nname = \"c\"\ndepth = 0"), "channel c: depth 0 outside"),
        (with("depth = 8", "depth = 70000"), "channel c: depth 70000 outside"),
        (with("depth = 8", "depth = 8\nmax_readers = 65"), "channel c: max_readers 65 outside 1..=64"),
        (with("[[channel]]\nname = \"c\"", "[[channel]]\nname = \"c a\""), "channel \"c a\": the name is not a valid name"),
        (with("module = \"m\"", "module = \"other\""), "instance \"i\": module \"other\" is not declared"),
        (with("name = \"i\"", "name = \"i i\""), "instance \"i i\": the name is not a valid name"),
        (with("period_ms = 2", "period_ms = 0"), "period_ms must be a positive number"),
        (with("period_ms = 2", "period_ms = 4000000"), "period_ms must be a positive number"),
        (with("period_ms = 2", "period_ms = 2\nstep_budget_ms = -1"), "step_budget_ms must be a positive number"),
        (with("period_ms = 2", "period_ms = 2\nhang_limit_ms = 1"), "hang limit must be at least 20 ms"),
        (with("period_ms = 2", "period_ms = 100\nstep_budget_ms = 500\nhang_limit_ms = 100"), "hang limit must be at least"),
        (with("period_ms = 2", "period_ms = \"fast\""), "invalid type"),
        (with("period_ms = 2", "period_ms = 2\naffinity = \"free\""), "instance \"i\": affinity \"free\" must be \"sticky\" or \"any\""),
        (with("period_ms = 2", "period_ms = 2\naffinity = 1"), "invalid type"),
        (with("out = \"c\"", "Out = \"c\""), "bind key \"Out\" is not a port name"),
        (with("out = \"c\"", "out = \"not a channel\""), "bind out -> \"not a channel\""),
        (with("out = \"c\"", "out = 3"), "invalid type"),
        (with("[instance.bind]", "[instance.config]\nbad = nan\n[instance.bind]"), "config: NaN cannot be written as JSON"),
        (
            with("[[instance]]\nname = \"i\"", "[[instance]]\nname = \"i\"\nmodule = \"m\"\n[[instance]]\nname = \"i\""),
            "instance \"i\" is declared twice",
        ),
        (with("[[module]]\nname = \"m\"", "[[module]]\nname = \"x\"\npath = \"libm.so\"\n[[module]]\nname = \"m\""), "already listed"),
        ("entity = \"e\"\n[[instance]]\nname = \"i\"\n".to_owned(), "missing field `module`"),
        ("".to_owned(), "missing field `entity`"),
        ("entity = ".to_owned(), "line 1"),
        ("entity = \"e\"\n[host\nworkers = 1".to_owned(), "line 2"),
    ];
    for (text, expected) in cases {
        match parse(&text) {
            Ok(_) => panic!("accepted a manifest that should fail with {expected:?}:\n{text}"),
            Err(message) => assert!(message.contains(expected), "expected {expected:?} in {message:?} for\n{text}"),
        }
    }
}

#[test]
fn more_than_128_instances_are_refused() {
    let mut text = String::from("entity = \"e\"\n[[module]]\nname = \"m\"\npath = \"libm.so\"\n");
    for i in 0..129 {
        text.push_str(&format!("[[instance]]\nname = \"i{i}\"\nmodule = \"m\"\n"));
    }
    assert!(parse(&text).unwrap_err().contains("129 instances (at most 128)"));
}

#[test]
fn every_problem_of_a_manifest_is_reported_together() {
    let text = with("workers = 2", "workers = 0").replace("period_ms = 2", "period_ms = -1").replace("module = \"m\"", "module = \"zzz\"");
    let message = parse(&text).unwrap_err();
    for expected in ["host.workers 0", "period_ms must be", "module \"zzz\" is not declared"] {
        assert!(message.contains(expected), "{expected:?} missing from {message}");
    }
    assert_eq!(message.lines().count(), 3, "{message}");
}
