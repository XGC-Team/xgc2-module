//! Rust mirror of `include/xgc2/module.h` (module ABI 2.1).
//!
//! Field order and types follow the C header exactly; `tests/abi_layout.rs` compares every
//! size and offset with a C compilation of the header. Fields a module fills in (the
//! descriptor) are `Option` so that a NULL entry is representable and can be rejected
//! instead of called. Enumerations are plain integers: a module may return any value.

use std::ffi::{c_char, c_int, c_void};

pub const ABI_MAJOR: u32 = 2;
pub const ABI_MINOR: u32 = 1;
/// Name of the exported descriptor function, and the same NUL-terminated for `dlsym`.
pub const ENTRY_NAME: &str = "xgc2_module_entry";
pub const ENTRY_SYMBOL: &[u8] = b"xgc2_module_entry\0";
pub const MAX_PORTS: usize = 64;

pub type Status = c_int;
pub const OK: Status = 0;
pub const ERR_INVALID: Status = 1;
pub const ERR_STATE: Status = 2;
pub const ERR_FULL: Status = 3;
pub const ERR_NODATA: Status = 4;
pub const ERR_INTERNAL: Status = 5;

pub const PORT_IN: u32 = 1;
pub const PORT_OUT: u32 = 2;
pub const PORT_STATE: u32 = 1;
pub const PORT_EVENT: u32 = 2;
/// Input flag: the instance is not ready until a producer is bound.
pub const PORT_REQUIRED: u32 = 0x1;
/// Output flag: written from a module-owned thread.
pub const PORT_ASYNC_WRITER: u32 = 0x2;

pub const STEP_INPUT: u32 = 0x1;
pub const STEP_TIMER: u32 = 0x2;
pub const STEP_WAKE: u32 = 0x4;
pub const STEP_CONFIG: u32 = 0x8;
/// Minor 1: the instant set with `wake_at_ns` was reached.
pub const STEP_DEADLINE: u32 = 0x10;

#[repr(C)]
pub struct PortDesc {
    pub name: *const c_char,
    pub direction: u32,
    pub kind: u32,
    pub schema_id: *const c_char,
    pub size: u32,
    pub align: u32,
    pub queue_depth: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SampleView {
    pub data: *const c_void,
    pub size: u32,
    pub seq: u64,
    pub stamp_ns: i64,
}

impl SampleView {
    pub const EMPTY: SampleView = SampleView { data: std::ptr::null(), size: 0, seq: 0, stamp_ns: 0 };
}

#[repr(C)]
pub struct StepCtx {
    pub now_ns: i64,
    pub step_index: u64,
    pub changed_inputs: u64,
    pub reasons: u32,
}

#[repr(C)]
pub struct Config {
    pub json: *const c_char,
    pub length: usize,
}

/// Host function table handed to `create`. Every entry takes the opaque host context that
/// was passed to `create` next to the table.
#[repr(C)]
pub struct HostApi {
    pub abi_major: u32,
    pub abi_minor: u32,
    pub write_begin: unsafe extern "C" fn(*mut c_void, u32) -> *mut c_void,
    pub write_commit: unsafe extern "C" fn(*mut c_void, u32, i64) -> Status,
    pub write_abort: unsafe extern "C" fn(*mut c_void, u32),
    pub read_latest: unsafe extern "C" fn(*mut c_void, u32, *mut SampleView) -> Status,
    pub read_next: unsafe extern "C" fn(*mut c_void, u32, *mut SampleView) -> Status,
    pub changed: unsafe extern "C" fn(*mut c_void, u32) -> c_int,
    pub now_ns: unsafe extern "C" fn(*mut c_void) -> i64,
    pub wake: unsafe extern "C" fn(*mut c_void),
    pub set_period_ns: unsafe extern "C" fn(*mut c_void, i64),
    pub log: unsafe extern "C" fn(*mut c_void, c_int, *const c_char),
    pub report: unsafe extern "C" fn(*mut c_void, c_int, *const c_char),
    /// Minor 1.
    pub wake_at_ns: unsafe extern "C" fn(*mut c_void, i64),
    /// Minor 1.
    pub phase_t0_ns: unsafe extern "C" fn(*mut c_void) -> i64,
}

/// Opaque module instance (`xgc2_instance`).
#[repr(C)]
pub struct Instance {
    _private: [u8; 0],
}

pub type CreateFn = unsafe extern "C" fn(*const HostApi, *mut c_void, *const Config, *mut *mut Instance) -> Status;
pub type ConfigureFn = unsafe extern "C" fn(*mut Instance, *const Config) -> Status;
pub type LifecycleFn = unsafe extern "C" fn(*mut Instance) -> Status;
pub type StepFn = unsafe extern "C" fn(*mut Instance, *const StepCtx) -> Status;
pub type DestroyFn = unsafe extern "C" fn(*mut Instance);

#[repr(C)]
pub struct ModuleDesc {
    pub abi_major: u32,
    pub abi_minor: u32,
    pub name: *const c_char,
    pub version: *const c_char,
    pub ports: *const PortDesc,
    pub port_count: u32,
    pub create: Option<CreateFn>,
    pub configure: Option<ConfigureFn>,
    pub start: Option<LifecycleFn>,
    pub step: Option<StepFn>,
    pub stop: Option<LifecycleFn>,
    pub destroy: Option<DestroyFn>,
}

pub type EntryFn = unsafe extern "C" fn() -> *const ModuleDesc;

/// Log levels of `HostApi::log`.
pub const LOG_DEBUG: c_int = 0;
pub const LOG_INFO: c_int = 1;
pub const LOG_WARN: c_int = 2;
pub const LOG_ERROR: c_int = 3;

/// Health values of `HostApi::report`.
pub const HEALTH_OK: c_int = 0;
pub const HEALTH_DEGRADED: c_int = 1;
pub const HEALTH_FAILED: c_int = 2;
