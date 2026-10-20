//! The Rust mirror of the ABI against the C header, compiled by the system C compiler: every
//! size, offset and constant must agree.

mod common;

use common::*;
use std::mem::{offset_of, size_of};
use xgc2_module_host::abi::*;

#[test]
fn rust_and_c_agree_on_the_layout() {
    let path = build("layout_probe.c", "layout_probe", &[]);
    // SAFETY: the probe exports two plain functions with these signatures.
    let (count, probe, entry_symbol) = unsafe {
        let library = libloading::Library::new(&path).unwrap();
        let count: unsafe extern "C" fn() -> u64 = *library.get(b"xgc2_probe_count\0").unwrap();
        let probe: unsafe extern "C" fn(u64) -> u64 = *library.get(b"xgc2_probe\0").unwrap();
        let symbol: unsafe extern "C" fn() -> *const std::os::raw::c_char = *library.get(b"xgc2_probe_entry_symbol\0").unwrap();
        let values: Vec<u64> = (0..count()).map(|i| probe(i)).collect();
        let entry = std::ffi::CStr::from_ptr(symbol()).to_str().unwrap().to_owned();
        std::mem::forget(library);
        (values.len(), values, entry)
    };
    assert_eq!(entry_symbol, ENTRY_NAME, "the entry function name of the header");
    assert_eq!(ENTRY_SYMBOL, format!("{ENTRY_NAME}\0").as_bytes());
    let expected: Vec<usize> = vec![
        size_of::<PortDesc>(),
        offset_of!(PortDesc, direction),
        offset_of!(PortDesc, kind),
        offset_of!(PortDesc, schema_id),
        offset_of!(PortDesc, size),
        offset_of!(PortDesc, align),
        offset_of!(PortDesc, queue_depth),
        offset_of!(PortDesc, flags),
        size_of::<SampleView>(),
        offset_of!(SampleView, size),
        offset_of!(SampleView, seq),
        offset_of!(SampleView, stamp_ns),
        size_of::<StepCtx>(),
        offset_of!(StepCtx, step_index),
        offset_of!(StepCtx, changed_inputs),
        offset_of!(StepCtx, reasons),
        size_of::<Config>(),
        offset_of!(Config, length),
        size_of::<HostApi>(),
        offset_of!(HostApi, abi_minor),
        offset_of!(HostApi, write_begin),
        offset_of!(HostApi, write_commit),
        offset_of!(HostApi, write_abort),
        offset_of!(HostApi, read_latest),
        offset_of!(HostApi, read_next),
        offset_of!(HostApi, changed),
        offset_of!(HostApi, now_ns),
        offset_of!(HostApi, wake),
        offset_of!(HostApi, set_period_ns),
        offset_of!(HostApi, log),
        offset_of!(HostApi, report),
        offset_of!(HostApi, wake_at_ns),
        offset_of!(HostApi, phase_t0_ns),
        size_of::<ModuleDesc>(),
        offset_of!(ModuleDesc, abi_minor),
        offset_of!(ModuleDesc, name),
        offset_of!(ModuleDesc, version),
        offset_of!(ModuleDesc, ports),
        offset_of!(ModuleDesc, port_count),
        offset_of!(ModuleDesc, create),
        offset_of!(ModuleDesc, configure),
        offset_of!(ModuleDesc, start),
        offset_of!(ModuleDesc, step),
        offset_of!(ModuleDesc, stop),
        offset_of!(ModuleDesc, destroy),
        ABI_MAJOR as usize,
        ABI_MINOR as usize,
        MAX_PORTS,
        OK as usize,
        ERR_INVALID as usize,
        ERR_STATE as usize,
        ERR_FULL as usize,
        ERR_NODATA as usize,
        ERR_INTERNAL as usize,
        PORT_IN as usize,
        PORT_OUT as usize,
        PORT_STATE as usize,
        PORT_EVENT as usize,
        PORT_REQUIRED as usize,
        PORT_ASYNC_WRITER as usize,
        STEP_INPUT as usize,
        STEP_TIMER as usize,
        STEP_WAKE as usize,
        STEP_CONFIG as usize,
        STEP_DEADLINE as usize,
    ];
    assert_eq!(count, expected.len(), "probe list and expectation list differ in length");
    for (index, (c, rust)) in probe.iter().zip(&expected).enumerate() {
        assert_eq!(*c as usize, *rust, "probe #{index}");
    }
}
