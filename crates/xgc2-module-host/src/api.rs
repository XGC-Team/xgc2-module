//! The host function table modules call (`xgc2_host_api`).
//!
//! Every function receives the instance's host context, which is a pointer to its
//! [`Instance`]. The host keeps the instance alive for as long as the module can call: until
//! `destroy` returns, or for the life of the process when the instance is isolated. None of
//! these functions may panic, because a panic cannot unwind into C.

use crate::abi::{self, HostApi, SampleView, Status};
use crate::instance::Instance;
use crate::log::{self, Level};
use std::ffi::{c_char, c_int, c_void, CStr};

static HOST_API: HostApi = HostApi {
    abi_major: abi::ABI_MAJOR,
    abi_minor: abi::ABI_MINOR,
    write_begin,
    write_commit,
    write_abort,
    read_latest,
    read_next,
    changed,
    now_ns,
    wake,
    set_period_ns,
    log: log_message,
    report,
    wake_at_ns,
    phase_t0_ns,
};

pub fn host_api() -> &'static HostApi {
    &HOST_API
}

/// # Safety
/// `ctx` is NULL or the host context the host passed to `create`.
unsafe fn instance<'a>(ctx: *mut c_void) -> Option<&'a Instance> {
    // SAFETY: the host context is the address of a live `Instance` (see the module docs).
    unsafe { (ctx as *const Instance).as_ref() }
}

unsafe extern "C" fn write_begin(ctx: *mut c_void, port: u32) -> *mut c_void {
    // SAFETY: see `instance`.
    unsafe { instance(ctx) }.map_or(std::ptr::null_mut(), |instance| instance.write_begin(port))
}

unsafe extern "C" fn write_commit(ctx: *mut c_void, port: u32, stamp_ns: i64) -> Status {
    // SAFETY: see `instance`.
    unsafe { instance(ctx) }.map_or(abi::ERR_INVALID, |instance| instance.write_commit(port, stamp_ns))
}

unsafe extern "C" fn write_abort(ctx: *mut c_void, port: u32) {
    // SAFETY: see `instance`.
    if let Some(instance) = unsafe { instance(ctx) } {
        instance.write_abort(port);
    }
}

unsafe fn read(ctx: *mut c_void, port: u32, latest: bool, out: *mut SampleView) -> Status {
    // SAFETY: see `instance`; `out` is NULL or a valid out pointer per the ABI.
    match (unsafe { instance(ctx) }, unsafe { out.as_mut() }) {
        (Some(instance), Some(out)) => instance.read(port, latest, out),
        _ => abi::ERR_INVALID,
    }
}

unsafe extern "C" fn read_latest(ctx: *mut c_void, port: u32, out: *mut SampleView) -> Status {
    // SAFETY: forwarded contract.
    unsafe { read(ctx, port, true, out) }
}

unsafe extern "C" fn read_next(ctx: *mut c_void, port: u32, out: *mut SampleView) -> Status {
    // SAFETY: forwarded contract.
    unsafe { read(ctx, port, false, out) }
}

unsafe extern "C" fn changed(ctx: *mut c_void, port: u32) -> c_int {
    // SAFETY: see `instance`.
    unsafe { instance(ctx) }.map_or(0, |instance| c_int::from(instance.changed(port)))
}

unsafe extern "C" fn now_ns(ctx: *mut c_void) -> i64 {
    // SAFETY: see `instance`.
    unsafe { instance(ctx) }.map_or(0, |instance| instance.env().clock.now_ns())
}

unsafe extern "C" fn wake(ctx: *mut c_void) {
    // SAFETY: see `instance`.
    if let Some(instance) = unsafe { instance(ctx) } {
        instance.wake();
    }
}

unsafe extern "C" fn set_period_ns(ctx: *mut c_void, period_ns: i64) {
    // SAFETY: see `instance`.
    if let Some(instance) = unsafe { instance(ctx) } {
        instance.set_period_ns(period_ns);
    }
}

unsafe extern "C" fn wake_at_ns(ctx: *mut c_void, deadline_ns: i64) {
    // SAFETY: see `instance`.
    if let Some(instance) = unsafe { instance(ctx) } {
        instance.wake_at_ns(deadline_ns);
    }
}

unsafe extern "C" fn phase_t0_ns(ctx: *mut c_void) -> i64 {
    // SAFETY: see `instance`.
    unsafe { instance(ctx) }.map_or(0, |instance| instance.env().clock.phase_t0_ns())
}

/// # Safety
/// `message` is NULL or NUL-terminated.
unsafe fn text(message: *const c_char) -> String {
    if message.is_null() {
        return String::new();
    }
    // SAFETY: caller contract.
    unsafe { CStr::from_ptr(message) }.to_string_lossy().into_owned()
}

unsafe extern "C" fn log_message(ctx: *mut c_void, level: c_int, message: *const c_char) {
    // SAFETY: see `instance`; the message is a NUL-terminated string per the ABI.
    let (instance, message) = unsafe { (instance(ctx), text(message)) };
    let target = instance.map_or("module".to_owned(), |instance| instance.name.clone());
    log::emit(Level::from_abi(level), &target, &message);
}

unsafe extern "C" fn report(ctx: *mut c_void, health: c_int, detail: *const c_char) {
    // SAFETY: see `instance`; the detail is a NUL-terminated string per the ABI.
    let (instance, detail) = unsafe { (instance(ctx), text(detail)) };
    if let Some(instance) = instance {
        instance.set_report(health, &detail);
    }
}
