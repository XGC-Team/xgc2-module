//! Loading real libraries: descriptors the host refuses and what it learns from good ones.

mod common;

use common::*;
use xgc2_module_host::abi;
use xgc2_module_host::channel::Kind;
use xgc2_module_host::loader::{self, Dir, LoadError};

#[test]
fn good_descriptors_are_read_completely() {
    let path = module("consumer");
    let module = loader::load(&path, None).unwrap();
    assert_eq!((module.name.as_str(), module.version.as_str(), module.abi_minor), ("test_consumer", "1.0.0", 1));
    assert_eq!(module.canonical, path.canonicalize().unwrap());
    assert_eq!(module.sha256, loader::sha256_hex(&std::fs::read(&path).unwrap()));
    assert_eq!(module.ports.len(), 2);
    let state = &module.ports[0];
    assert_eq!((state.name.as_str(), state.dir, state.kind), ("state_in", Dir::In, Kind::State));
    assert_eq!((state.payload.schema.as_str(), state.payload.size, state.payload.align, state.queue_depth), ("test.sample.v1", 64, 8, 0));
    let event = &module.ports[1];
    assert_eq!((event.kind, event.payload.size, event.queue_depth), (Kind::Event, 16, 16));
    // Inputs are numbered in port table order for changed_inputs.
    assert_eq!((module.input_bit(0), module.input_bit(1)), (Some(0), Some(1)));
    let stage = loader::load(&self::module("passthrough"), None).unwrap();
    assert_eq!((stage.input_bit(0), stage.input_bit(1)), (Some(0), None));
    assert!(stage.ports[0].required && !stage.ports[1].required);
    let clock = loader::load(&self::module("sim_clock"), None).unwrap();
    assert!(clock.ports[0].async_writer);
    assert_eq!(clock.ports[0].payload.schema, "xgc2.clock.v1");
    let cxx = loader::load(&self::module("wake_thread"), None).unwrap();
    assert_eq!(cxx.name, "test_wake_thread");
}

#[test]
fn broken_descriptors_are_refused_with_a_reason() {
    for (variant, expected) in [
        ("ABI1", "ABI major 1"),
        ("NEWER_MINOR", "newer than this host"),
        ("NO_STEP", "step is NULL"),
        ("DUPLICATE_PORT", "declared twice"),
        ("BAD_ALIGN", "not a power of two"),
        ("EVENT_WITHOUT_DEPTH", "queue_depth"),
    ] {
        let error = loader::load(&broken(variant), None).err().unwrap_or_else(|| panic!("{variant} was accepted"));
        assert!(matches!(error, LoadError::Descriptor(_)), "{variant}: {error:?}");
        assert!(error.to_string().contains(expected), "{variant}: {error}");
    }
}

#[test]
fn a_library_without_the_entry_point_is_refused() {
    // The layout probe is a valid shared library that exports no xgc2_module_entry.
    let error = loader::load(&build("layout_probe.c", "layout_probe", &[]), None).err().unwrap();
    assert!(matches!(error, LoadError::NoEntry(_)), "{error:?}");
    assert!(error.to_string().contains("xgc2_module_entry"));
}

#[test]
fn the_abi_constants_match_the_header() {
    assert_eq!((abi::ABI_MAJOR, abi::ABI_MINOR, abi::MAX_PORTS), (2, 1, 64));
}
