//! Loading a module library: sha256 pin, dlopen, descriptor validation.
//!
//! Everything the host needs from a descriptor is copied into [`Module`] before the library is
//! used, so later code never dereferences module memory except through the vtable.

use crate::abi::{self, ModuleDesc, PortDesc};
use crate::channel::{Kind, PayloadSpec};
use crate::names;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::{c_char, CStr};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

/// Largest payload a port may declare.
pub const MAX_PAYLOAD_SIZE: u32 = 1 << 20;
/// Largest event queue a port may ask for.
pub const MAX_QUEUE_DEPTH: u32 = 1 << 16;

#[derive(Debug)]
pub enum LoadError {
    Io(String),
    PinMismatch { expected: String, actual: String },
    Open(String),
    NoEntry(String),
    Descriptor(String),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(m) | LoadError::Open(m) | LoadError::NoEntry(m) | LoadError::Descriptor(m) => f.write_str(m),
            LoadError::PinMismatch { expected, actual } => {
                write!(f, "sha256 {actual} does not match the pinned {expected}")
            }
        }
    }
}

impl std::error::Error for LoadError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortSpec {
    pub name: String,
    pub dir: Dir,
    pub kind: Kind,
    pub payload: PayloadSpec,
    /// Events: the queue length this port asks for; state ports: 0.
    pub queue_depth: u32,
    pub required: bool,
    pub async_writer: bool,
}

/// The six lifecycle entry points, copied out of the descriptor.
#[derive(Clone, Copy, Debug)]
pub struct Vtable {
    pub create: abi::CreateFn,
    pub configure: abi::ConfigureFn,
    pub start: abi::LifecycleFn,
    pub step: abi::StepFn,
    pub stop: abi::LifecycleFn,
    pub destroy: abi::DestroyFn,
}

/// The validated contents of a descriptor.
#[derive(Debug)]
pub struct Descriptor {
    pub name: String,
    pub version: String,
    pub abi_minor: u32,
    pub ports: Vec<PortSpec>,
    pub vtable: Vtable,
}

/// A loaded library. The vtable's function pointers are valid exactly as long as this value
/// is alive; `Arc<Module>` is therefore held by every instance created from it.
pub struct Module {
    pub name: String,
    pub version: String,
    pub path: PathBuf,
    pub canonical: PathBuf,
    pub sha256: String,
    pub abi_minor: u32,
    pub ports: Vec<PortSpec>,
    pub vtable: Vtable,
    /// Set when an abandoned instance may still run code of this library, which must then
    /// stay mapped until the process exits.
    pub pinned: AtomicBool,
    /// For every port: its bit in `changed_inputs` (inputs in port table order).
    input_bits: Vec<Option<u8>>,
    _library: libloading::os::unix::Library,
}

// SAFETY: the library handle and the function pointers are process-wide immutable data;
// the module code itself is called under the ABI's threading rules.
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    pub fn port_index(&self, name: &str) -> Option<usize> {
        self.ports.iter().position(|port| port.name == name)
    }

    /// Bit of this input port in `xgc2_step_ctx.changed_inputs`.
    pub fn input_bit(&self, port: usize) -> Option<u8> {
        self.input_bits.get(port).copied().flatten()
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        text.push(HEX[usize::from(byte >> 4)] as char);
        text.push(HEX[usize::from(byte & 15)] as char);
    }
    text
}

/// Read `path`, check the pin, dlopen it and validate its descriptor.
pub fn load(path: &Path, pin: Option<&str>) -> Result<Module, LoadError> {
    let io = |what: &str, error: std::io::Error| LoadError::Io(format!("{}: {what}: {error}", path.display()));
    let canonical = path.canonicalize().map_err(|e| io("resolve", e))?;
    let metadata = std::fs::metadata(&canonical).map_err(|e| io("stat", e))?;
    if !metadata.is_file() {
        return Err(LoadError::Io(format!("{}: not a regular file", path.display())));
    }
    let bytes = std::fs::read(&canonical).map_err(|e| io("read", e))?;
    let sha256 = sha256_hex(&bytes);
    drop(bytes);
    if let Some(expected) = pin {
        if !expected.eq_ignore_ascii_case(&sha256) {
            return Err(LoadError::PinMismatch { expected: expected.to_ascii_lowercase(), actual: sha256 });
        }
    }
    // RTLD_NOW: unresolved symbols fail the load instead of crashing a step later.
    // SAFETY: loading runs the library's initializers. Only libraries the operator named
    // (and optionally pinned by digest) are loaded.
    let library = unsafe { libloading::os::unix::Library::open(Some(&canonical), libc::RTLD_NOW | libc::RTLD_LOCAL) }
        .map_err(|e| LoadError::Open(format!("{}: {e}", path.display())))?;
    // SAFETY: the symbol has the declared signature by the ABI contract.
    let entry: abi::EntryFn = *unsafe { library.get::<abi::EntryFn>(abi::ENTRY_SYMBOL) }
        .map_err(|e| LoadError::NoEntry(format!("{}: no {} entry point: {e}", path.display(), abi::ENTRY_NAME)))?;
    // SAFETY: the entry function takes no arguments and returns a pointer that stays valid
    // while the library is loaded.
    let descriptor = unsafe { entry() };
    if descriptor.is_null() {
        return Err(LoadError::Descriptor(format!("{}: descriptor is NULL", path.display())));
    }
    // SAFETY: non-null, and descriptors are immutable static data of the library.
    let parsed = unsafe { parse_descriptor(&*descriptor) }.map_err(|m| LoadError::Descriptor(format!("{}: {m}", path.display())))?;
    let mut input_bits = Vec::with_capacity(parsed.ports.len());
    let mut next_bit = 0u8;
    for port in &parsed.ports {
        if port.dir == Dir::In {
            input_bits.push(Some(next_bit));
            next_bit += 1;
        } else {
            input_bits.push(None);
        }
    }
    Ok(Module {
        name: parsed.name,
        version: parsed.version,
        path: path.to_owned(),
        canonical,
        sha256,
        abi_minor: parsed.abi_minor,
        ports: parsed.ports,
        vtable: parsed.vtable,
        pinned: AtomicBool::new(false),
        input_bits,
        _library: library,
    })
}

fn text(pointer: *const c_char, what: &str) -> Result<String, String> {
    if pointer.is_null() {
        return Err(format!("{what} is NULL"));
    }
    // SAFETY: descriptor strings are NUL-terminated and stay valid while the library is loaded.
    unsafe { CStr::from_ptr(pointer) }.to_str().map(str::to_owned).map_err(|_| format!("{what} is not UTF-8"))
}

/// Validate a descriptor and copy what the host needs.
///
/// # Safety
/// `desc.ports` must point at `desc.port_count` readable port descriptors and every string
/// pointer must be NUL-terminated (or NULL, which is rejected).
pub unsafe fn parse_descriptor(desc: &ModuleDesc) -> Result<Descriptor, String> {
    if desc.abi_major != abi::ABI_MAJOR {
        return Err(format!("ABI major {} (this host speaks {})", desc.abi_major, abi::ABI_MAJOR));
    }
    if desc.abi_minor > abi::ABI_MINOR {
        return Err(format!("ABI minor {} is newer than this host's {}.{}", desc.abi_minor, abi::ABI_MAJOR, abi::ABI_MINOR));
    }
    let name = text(desc.name, "module name")?;
    if !names::valid_name(&name) {
        return Err(format!("module name {name:?} is not a valid name"));
    }
    let version = text(desc.version, "module version")?;
    if version.is_empty() || version.len() > 64 {
        return Err("module version must be 1..64 characters".into());
    }
    if desc.port_count as usize > abi::MAX_PORTS {
        return Err(format!("{} ports (maximum {})", desc.port_count, abi::MAX_PORTS));
    }
    if desc.port_count > 0 && desc.ports.is_null() {
        return Err("port table is NULL".into());
    }
    let table: &[PortDesc] = if desc.port_count == 0 {
        &[]
    } else {
        // SAFETY: checked non-null; the caller guarantees `port_count` entries.
        unsafe { std::slice::from_raw_parts(desc.ports, desc.port_count as usize) }
    };
    let mut ports = Vec::with_capacity(table.len());
    let mut seen = HashSet::new();
    let mut inputs = 0;
    for entry in table {
        let port = parse_port(entry)?;
        if !seen.insert(port.name.clone()) {
            return Err(format!("port {} is declared twice", port.name));
        }
        inputs += usize::from(port.dir == Dir::In);
        ports.push(port);
    }
    if inputs > 64 {
        return Err(format!("{inputs} input ports (changed_inputs has 64 bits)"));
    }
    let missing = |entry: &str| format!("vtable entry {entry} is NULL");
    let vtable = Vtable {
        create: desc.create.ok_or_else(|| missing("create"))?,
        configure: desc.configure.ok_or_else(|| missing("configure"))?,
        start: desc.start.ok_or_else(|| missing("start"))?,
        step: desc.step.ok_or_else(|| missing("step"))?,
        stop: desc.stop.ok_or_else(|| missing("stop"))?,
        destroy: desc.destroy.ok_or_else(|| missing("destroy"))?,
    };
    Ok(Descriptor { name, version, abi_minor: desc.abi_minor, ports, vtable })
}

fn parse_port(entry: &PortDesc) -> Result<PortSpec, String> {
    let name = text(entry.name, "port name")?;
    if !names::valid_port_name(&name) {
        return Err(format!("port name {name:?} must match [a-z0-9_]{{1,63}}"));
    }
    let fail = |what: String| format!("port {name}: {what}");
    let dir = match entry.direction {
        abi::PORT_IN => Dir::In,
        abi::PORT_OUT => Dir::Out,
        other => return Err(fail(format!("direction {other} is neither in (1) nor out (2)"))),
    };
    let kind = match entry.kind {
        abi::PORT_STATE => Kind::State,
        abi::PORT_EVENT => Kind::Event,
        other => return Err(fail(format!("kind {other} is neither state (1) nor event (2)"))),
    };
    let schema = text(entry.schema_id, "schema id").map_err(fail)?;
    if !names::valid_id(&schema) {
        return Err(fail(format!("schema id {schema:?} must match [A-Za-z0-9._:-]{{1,128}}")));
    }
    if entry.size == 0 || entry.size > MAX_PAYLOAD_SIZE {
        return Err(fail(format!("payload size {} outside 1..={MAX_PAYLOAD_SIZE}", entry.size)));
    }
    if !entry.align.is_power_of_two() || entry.align > 64 {
        return Err(fail(format!("payload align {} is not a power of two <= 64", entry.align)));
    }
    if entry.size % entry.align != 0 {
        return Err(fail(format!("payload size {} is not a multiple of its align {}", entry.size, entry.align)));
    }
    match kind {
        Kind::Event if entry.queue_depth == 0 || entry.queue_depth > MAX_QUEUE_DEPTH => {
            return Err(fail(format!("event queue_depth {} outside 1..={MAX_QUEUE_DEPTH}", entry.queue_depth)));
        }
        Kind::State if entry.queue_depth != 0 => return Err(fail("state ports must declare queue_depth 0".into())),
        _ => {}
    }
    if entry.flags & !(abi::PORT_REQUIRED | abi::PORT_ASYNC_WRITER) != 0 {
        return Err(fail(format!("unknown flag bits {:#x}", entry.flags)));
    }
    let required = entry.flags & abi::PORT_REQUIRED != 0;
    let async_writer = entry.flags & abi::PORT_ASYNC_WRITER != 0;
    if required && dir == Dir::Out {
        return Err(fail("the required flag applies to inputs only".into()));
    }
    if async_writer && dir == Dir::In {
        return Err(fail("the async writer flag applies to outputs only".into()));
    }
    Ok(PortSpec {
        name,
        dir,
        kind,
        payload: PayloadSpec { schema, size: entry.size, align: entry.align },
        queue_depth: entry.queue_depth,
        required,
        async_writer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{c_void, CString};

    unsafe extern "C" fn create(_: *const abi::HostApi, _: *mut c_void, _: *const abi::Config, _: *mut *mut abi::Instance) -> abi::Status {
        abi::OK
    }
    unsafe extern "C" fn configure(_: *mut abi::Instance, _: *const abi::Config) -> abi::Status {
        abi::OK
    }
    unsafe extern "C" fn lifecycle(_: *mut abi::Instance) -> abi::Status {
        abi::OK
    }
    unsafe extern "C" fn step(_: *mut abi::Instance, _: *const abi::StepCtx) -> abi::Status {
        abi::OK
    }
    unsafe extern "C" fn destroy(_: *mut abi::Instance) {}

    /// A port table entry as plain numbers, so a test can break one field at a time.
    #[derive(Clone, Copy)]
    struct Raw {
        name: &'static str,
        direction: u32,
        kind: u32,
        size: u32,
        align: u32,
        depth: u32,
        flags: u32,
    }

    const STATE_IN: Raw = Raw { name: "p", direction: abi::PORT_IN, kind: abi::PORT_STATE, size: 8, align: 8, depth: 0, flags: 0 };

    struct Fixture {
        strings: Vec<CString>,
        ports: Vec<PortDesc>,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture { strings: Vec::new(), ports: Vec::new() }
        }
        fn cstr(&mut self, text: &str) -> *const c_char {
            self.strings.push(CString::new(text).unwrap());
            self.strings.last().unwrap().as_ptr()
        }
        fn port(&mut self, raw: Raw) {
            let (name, schema) = (self.cstr(raw.name), self.cstr("test.v1"));
            self.ports.push(PortDesc {
                name,
                direction: raw.direction,
                kind: raw.kind,
                schema_id: schema,
                size: raw.size,
                align: raw.align,
                queue_depth: raw.depth,
                flags: raw.flags,
            });
        }
        fn descriptor(&mut self, major: u32, minor: u32) -> ModuleDesc {
            ModuleDesc {
                abi_major: major,
                abi_minor: minor,
                name: self.cstr("demo"),
                version: self.cstr("1.0"),
                ports: self.ports.as_ptr(),
                port_count: self.ports.len() as u32,
                create: Some(create),
                configure: Some(configure),
                start: Some(lifecycle),
                step: Some(step),
                stop: Some(lifecycle),
                destroy: Some(destroy),
            }
        }
    }

    fn parse(fixture: &mut Fixture, major: u32, minor: u32) -> Result<Descriptor, String> {
        let desc = fixture.descriptor(major, minor);
        // SAFETY: the fixture owns the strings and the port table.
        unsafe { parse_descriptor(&desc) }
    }

    #[test]
    fn accepts_a_valid_descriptor() {
        let mut f = Fixture::new();
        f.port(Raw { name: "pose", size: 24, flags: abi::PORT_REQUIRED, ..STATE_IN });
        f.port(Raw {
            name: "cmd",
            direction: abi::PORT_OUT,
            kind: abi::PORT_EVENT,
            size: 16,
            depth: 8,
            flags: abi::PORT_ASYNC_WRITER,
            ..STATE_IN
        });
        let parsed = parse(&mut f, 2, 0).unwrap();
        assert_eq!((parsed.name.as_str(), parsed.version.as_str()), ("demo", "1.0"));
        assert_eq!(parsed.ports.len(), 2);
        assert!(parsed.ports[0].required && parsed.ports[1].async_writer);
        assert_eq!(parsed.ports[1].kind, Kind::Event);
        assert_eq!(parsed.ports[1].payload.size, 16);
    }

    #[test]
    fn rejects_bad_versions_and_tables() {
        let mut f = Fixture::new();
        assert!(parse(&mut f, 1, 0).unwrap_err().contains("ABI major 1"));
        assert!(parse(&mut f, 2, 2).unwrap_err().contains("newer"));
        let mut f = Fixture::new();
        f.port(Raw { name: "a", ..STATE_IN });
        f.port(Raw { name: "a", direction: abi::PORT_OUT, ..STATE_IN });
        assert!(parse(&mut f, 2, 0).unwrap_err().contains("declared twice"));
        let mut f = Fixture::new();
        for i in 0..65 {
            f.port(Raw { name: Box::leak(format!("p{i}").into_boxed_str()), direction: abi::PORT_OUT, ..STATE_IN });
        }
        assert!(parse(&mut f, 2, 0).unwrap_err().contains("65 ports"));
        let mut f = Fixture::new();
        for i in 0..64 {
            f.port(Raw { name: Box::leak(format!("p{i}").into_boxed_str()), ..STATE_IN });
        }
        assert!(parse(&mut f, 2, 0).is_ok());
    }

    #[test]
    fn rejects_bad_ports() {
        let cases = [
            (Raw { name: "Bad", ..STATE_IN }, "must match"),
            (Raw { direction: 3, ..STATE_IN }, "direction"),
            (Raw { kind: 3, ..STATE_IN }, "kind"),
            (Raw { size: 0, ..STATE_IN }, "payload size"),
            (Raw { align: 3, ..STATE_IN }, "power of two"),
            (Raw { align: 128, ..STATE_IN }, "power of two"),
            (Raw { size: 12, ..STATE_IN }, "multiple"),
            (Raw { kind: abi::PORT_EVENT, ..STATE_IN }, "queue_depth"),
            (Raw { depth: 4, ..STATE_IN }, "queue_depth 0"),
            (Raw { direction: abi::PORT_OUT, flags: abi::PORT_REQUIRED, ..STATE_IN }, "inputs only"),
            (Raw { flags: abi::PORT_ASYNC_WRITER, ..STATE_IN }, "outputs only"),
            (Raw { flags: 0x40, ..STATE_IN }, "unknown flag"),
        ];
        for (raw, expect) in cases {
            let mut f = Fixture::new();
            f.port(raw);
            let error = parse(&mut f, 2, 0).unwrap_err();
            assert!(error.contains(expect), "{}: {error}", raw.name);
        }
    }

    #[test]
    fn rejects_missing_entry_points() {
        let mut f = Fixture::new();
        let mut desc = f.descriptor(2, 0);
        desc.step = None;
        // SAFETY: fixture-owned data.
        let error = unsafe { parse_descriptor(&desc) }.err().unwrap();
        assert!(error.contains("step is NULL"));
        desc.step = Some(step);
        desc.name = std::ptr::null();
        assert!(unsafe { parse_descriptor(&desc) }.err().unwrap().contains("module name is NULL"));
    }

    #[test]
    fn load_reports_missing_files_and_bad_pins() {
        let missing = load(Path::new("/nonexistent/libx.so"), None).err().unwrap();
        assert!(matches!(missing, LoadError::Io(_)), "{missing}");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libnot.so");
        std::fs::write(&path, b"not an elf").unwrap();
        let wrong = "0".repeat(64);
        assert!(matches!(load(&path, Some(&wrong)).err().unwrap(), LoadError::PinMismatch { .. }));
        assert!(matches!(load(&path, None).err().unwrap(), LoadError::Open(_)));
        assert!(matches!(load(dir.path(), None).err().unwrap(), LoadError::Io(_)));
    }
}
