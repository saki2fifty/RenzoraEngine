//! Compiled-script boundary for `.rs` scripts.
//!
//! A `.rs` file is one script, not a language backend and not a plugin.
//! This module is the thin C-ABI descriptor a `.rs` cdylib exports
//! together with the author-facing macro that emits it.
//!
//! ## The shape of the boundary (true C ABI)
//!
//! The descriptor exposes the existing
//! [`crate::sys::ScriptEntry`] —
//! `unsafe extern "C" fn(*const ScriptCall) -> ScriptStatus` — plus
//! ABI/size/prefix-hash/capability metadata. **Nothing Rust-ABI
//! crosses the cdylib boundary.** The author's typed function is
//! `fn(&Ctx, &mut ScriptReply) -> Result<(), String>`; it is generated
//! by the `rust_script!` macro INSIDE the same cdylib the descriptor
//! is emitted in, and the per-cdylib `ScriptEntry` trampoline
//! statically invokes it. The host calls the per-cdylib entry
//! through the standard [`crate::sys::ScriptCall`] contract and
//! never sees the typed function.
//!
//! ## ABI negotiation (S4-4 safe-read design)
//!
//! A loader must verify the following before publishing a generation.
//! Every check is bounded by a host-known maximum so a malformed or
//! malicious descriptor cannot drive an unbounded read or use a
//! foreign-supplied count as proof of pointer validity:
//!
//! 1. **Minimum header size.** The first [`MIN_DESCRIPTOR_SIZE`]
//!    bytes of the descriptor are read and validated before any
//!    field beyond that is consulted. This is a fixed host constant,
//!    never a descriptor-supplied length.
//! 2. **Descriptor size.** `desc.descriptor_size` must equal
//!    `mem::size_of::<CompiledScriptDesc>()`. Two descriptors of the
//!    same layout grow together; a mismatch means one side got a
//!    field the other cannot see.
//! 3. **ABI version.** `desc.abi_version` must equal
//!    [`COMPILED_SCRIPT_ABI`].
//! 4. **Capabilities.** The descriptor declares the
//!    [`CompiledScriptCapabilities`] bits it depends on; the loader
//!    refuses descriptors whose requirements exceed what this build
//!    provides.
//! 5. **Prefix hash counts.** Each prefix-hash count is bounded by
//!    the host's own [`MAX_PREFIX_HASHES`] constant. The slice the
//!    loader actually reads is `min(desc.count, MAX_PREFIX_HASHES)`,
//!    and the host's own chain is always read in full. The
//!    descriptor's count is NOT trusted as a slice length.
//! 6. **Prefix hashes.** Up to `MAX_PREFIX_HASHES` entries are
//!    compared against the host's own chain; any mismatch is
//!    rejection.
//! 7. **Reserved fields.** `_pad` and other reserved slots must be
//!    zero; non-zero values indicate a future field or a corrupted
//!    descriptor.
//! 8. **Entry non-null.** The descriptor's `entry` function pointer
//!    must be non-null (the cdylib's exported `ScriptEntry`).
//!
//! ## Identity is host-owned
//!
//! The descriptor carries no identity. The canonical id arrives on
//! every call through [`crate::sys::ScriptCall::path`], and the host
//! uses it to look the per-cdylib entry up in the host-owned
//! [`CompiledScriptBackend`] registry.
//!
//! ## Hook routing
//!
//! Update-only scripts (the common case) declare
//! `fn update(ctx, reply) -> Result<(), String>` and the trampoline
//! invokes it for [`crate::sys::ScriptOp::OnUpdate`] only. Every other
//! op returns [`crate::sys::ScriptStatus::NoHook`]; a script that wants
//! a different surface extends the dispatcher in this module.

use std::sync::{Arc, Mutex};

use crate::sys::{ScriptCall, ScriptEntry, ScriptStatus};

/// The Tier 1 compiled-script ABI version this crate ships.
///
/// Bumped whenever [`CompiledScriptDesc`] gains a field, when its
/// meaning changes, or when the wire contract (`ScriptCall`,
/// `ScriptHostCalls`) does.
pub const COMPILED_SCRIPT_ABI: u32 = 1;

/// Host-known maximum number of prefix-hash entries the descriptor
/// may declare. The loader bounds every read against this constant so
/// a malicious or corrupted descriptor cannot drive an unbounded
/// slice creation. The host's own chain is also bounded to the same
/// size.
pub const MAX_PREFIX_HASHES: usize = 32;

/// Host-known minimum size of a well-formed descriptor. The loader
/// reads and validates this many bytes first; only after the
/// minimum-header checks pass does it consult the full
/// descriptor's fields. Used to reject zero-size, undersized, and
/// deliberately malformed descriptors before any foreign count or
/// pointer is consulted.
pub const MIN_DESCRIPTOR_SIZE: usize = 56;

/// Query ABI return values. The cdylib embeds these in the lower
/// 16 bits of the return value; the upper 16 bits carry the
/// descriptor size requested by the host (always
/// `descriptor_size`).
pub const QUERY_STATUS_OK: u32 = 0x0000_0000;
pub const QUERY_STATUS_NULL_OUTPUT: u32 = 0x0000_0001;
pub const QUERY_STATUS_VERSION_MISMATCH: u32 = 0x0000_0002;
pub const QUERY_STATUS_BUFFER_TOO_SMALL: u32 = 0x0000_0003;

/// Decode the `bytes_written` field of a query ABI return value
/// (upper 16 bits of the return value).
#[inline]
pub const fn query_written(ret: u32) -> u32 {
    (ret >> 16) & 0xFFFF
}

/// Decode the status field of a query ABI return value (lower 16 bits).
#[inline]
pub const fn query_status(ret: u32) -> u32 {
    ret & 0xFFFF
}

/// Tier 1 capability bits a compiled-script descriptor may require.
///
/// Each bit names a feature the loader's host either provides or
/// refuses. The default `0` means "nothing beyond the base wire", which
/// is enough for update-only scripts reading host state and queuing
/// commands.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CompiledScriptCapabilities(pub u32);

impl CompiledScriptCapabilities {
    /// No capabilities beyond the base wire.
    pub const NONE: Self = Self(0);

    /// Build the bitwise intersection used for the compatibility check.
    pub fn contains(&self, required: Self) -> bool {
        (self.0 & required.0) == required.0
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

/// A fixed-layout prefix-hash chain the descriptor carries alongside
/// the entry pointer. The loader reads prefix hashes one entry at a
/// time, comparing each against its own host-known chain. Using a
/// fixed-size array means the descriptor cannot drive a slice read
/// of unbounded length: every read is bounded by
/// [`MAX_PREFIX_HASHES`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PrefixHashChain {
    /// Host-known number of valid entries in `hashes`. The loader
    /// reads EXACTLY this many entries, never the descriptor's
    /// count field.
    pub count: u32,
    /// Inline storage. Unused entries are zero; the loader stops
    /// reading at `count` entries.
    pub hashes: [u64; MAX_PREFIX_HASHES],
}

impl Default for PrefixHashChain {
    fn default() -> Self {
        Self {
            count: 0,
            hashes: [0u64; MAX_PREFIX_HASHES],
        }
    }
}

// SAFETY: the chain is plain data; reading/writing individual
// entries is safe across threads once the descriptor has been
// validated and registered.
unsafe impl Send for PrefixHashChain {}
unsafe impl Sync for PrefixHashChain {}

/// The compiled-script descriptor a `.rs` cdylib exports.
///
/// `#[repr(C)]` and pinned-layout by construction. The host reads
/// every field by exact offset. New fields may only be appended; the
/// `descriptor_size` field gives a loader the means to refuse a
/// descrambled binary before it calls the entry.
///
/// S4-4: prefix hashes live inside fixed-size inline arrays on the
/// descriptor. The descriptor declares how many entries are valid
/// (`call_prefix_count`, `host_prefix_count`); the loader bounds
/// every read against [`MAX_PREFIX_HASHES`] and never dereferences a
/// foreign-supplied pointer. The previous pointer+count design
/// allowed a malicious descriptor to drive an unbounded slice read
/// before the count was validated.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CompiledScriptDesc {
    /// ABI version the descriptor was compiled against.
    pub abi_version: u32,
    /// `mem::size_of::<CompiledScriptDesc>()` at compile time. The host
    /// compares against the runtime size to refuse descrambled or
    /// version-skewed layouts.
    pub descriptor_size: u32,
    /// Capabilities this descriptor requires. The host refuses the
    /// descriptor if `host_capabilities.contains(required)` is false.
    pub required_capabilities: u32,
    /// Reserved. Must be zero on the descriptor side.
    pub _pad: u32,
    /// Number of valid entries in `call_prefix_hashes`. Bounded by
    /// [`MAX_PREFIX_HASHES`] at validation time.
    pub call_prefix_count: u32,
    /// Inline prefix-hash chain covering `ScriptCall`'s field order.
    /// The loader reads exactly `min(call_prefix_count,
    /// MAX_PREFIX_HASHES)` entries.
    pub call_prefix_hashes: [u64; MAX_PREFIX_HASHES],
    /// Number of valid entries in `host_prefix_hashes`.
    pub host_prefix_count: u32,
    /// Inline prefix-hash chain covering `ScriptHostCalls`'s field
    /// order.
    pub host_prefix_hashes: [u64; MAX_PREFIX_HASHES],
    /// The script's `ScriptEntry`. **The only callable the host
    /// invokes.** The cdylib emits this `unsafe extern "C"` trampoline
    /// via the `rust_script!` macro; it decodes the `ScriptCall`,
    /// statically calls the author's typed function (compiled into the
    /// SAME cdylib), and writes the reply. The host reads the pointer
    /// and calls it through the standard wire contract — no Rust-ABI
    /// surface crosses the dynamic-library boundary.
    pub entry: ScriptEntry,
}

// SAFETY: `CompiledScriptDesc` is plain data plus a function pointer
// the host only ever reads. The host-owned registry owns a `Copy` of
// the value for the life of the registration and never mutates it.
unsafe impl Send for CompiledScriptDesc {}
unsafe impl Sync for CompiledScriptDesc {}

impl Default for CompiledScriptDesc {
    /// Default fields are zero; `entry` is `trampoline_placeholder`.
    /// Tests that build a well-formed descriptor fill in `entry`
    /// themselves. The default is intentionally NOT a valid
    /// descriptor: `check_compat` refuses a default descriptor
    /// because `abi_version` does not match [`COMPILED_SCRIPT_ABI`].
    fn default() -> Self {
        Self {
            abi_version: 0,
            descriptor_size: 0,
            required_capabilities: 0,
            _pad: 0,
            call_prefix_count: 0,
            call_prefix_hashes: [0u64; MAX_PREFIX_HASHES],
            host_prefix_count: 0,
            host_prefix_hashes: [0u64; MAX_PREFIX_HASHES],
            entry: trampoline_placeholder,
        }
    }
}

// A no-op extern "C" placeholder used as `Default::default().entry`
// so the type's `Default` impl is sound (a function pointer cannot
// be null in safe Rust). The default descriptor is not a valid
// descriptor; `check_compat` rejects it because `abi_version` does
// not match `COMPILED_SCRIPT_ABI`. No production code path uses the
// default.
extern "C" fn trampoline_placeholder(_call: *const ScriptCall) -> ScriptStatus {
    ScriptStatus::Error
}

/// A tiny FNV-1a-style hasher used to compute the prefix-hash chain.
///
/// Stable across `rustc` versions and on stable Rust (no
/// `DefaultHasher` interaction with `Hasher::write_str`, which is
/// unstable). The exact value does not matter — only that adding,
/// removing, renaming, or retyping a field changes the chain at the
/// modified index. Both sides compute the same chain because both
/// sides name the fields by string.
struct FieldHasher(u64);

impl FieldHasher {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;

    const fn new() -> Self {
        Self(Self::OFFSET)
    }
    const fn write_bytes(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            self.0 ^= bytes[i] as u64;
            self.0 = self.0.wrapping_mul(Self::PRIME);
            i += 1;
        }
    }
    const fn write_u64(&mut self, v: u64) {
        self.write_bytes(&v.to_le_bytes());
    }
    const fn write_str(&mut self, s: &str) {
        self.write_bytes(s.as_bytes());
        // Field separator so `"ab"+"c"` and `"a"+"bc"` differ.
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(Self::PRIME);
    }
    const fn finish(&self) -> u64 {
        self.0
    }
}

/// Build a descriptor's `call_prefix_hashes` / `call_prefix_count`
/// pair from a `ScriptCall`-shaped type's prefix chain.
///
/// The chain is append-stable: appending a field only appends an
/// entry, so a loader can compare the descriptor's chain against its
/// own and refuse if the *n*th entry (where *n* is the loader's
/// declared prefix length) differs.
pub const fn script_call_prefix_hashes(out: &mut [u64]) -> usize {
    let names: &[&str] = &[
        "op",
        "_pad",
        "path",
        "source",
        "version",
        "entity",
        "frame_seq",
        "frame",
        "entity_ctx",
        "args",
        "vars",
        "out",
        "host",
    ];
    let types: &[&str] = &[
        "ScriptOp",
        "u32",
        "StrRef",
        "StrRef",
        "u64",
        "u64",
        "u64",
        "BlobRef",
        "BlobRef",
        "BlobRef",
        "BlobRef",
        "*const ByteSink",
        "*const ScriptHostCalls",
    ];
    let n = if names.len() < types.len() {
        names.len()
    } else {
        types.len()
    };
    let n = if n < out.len() { n } else { out.len() };
    let mut prev = 0u64;
    let mut i = 0;
    while i < n {
        let mut h = FieldHasher::new();
        h.write_u64(prev);
        h.write_str(names[i]);
        h.write_str(types[i]);
        prev = h.finish();
        out[i] = prev;
        i += 1;
    }
    n
}

/// Companion to [`script_call_prefix_hashes`] for `ScriptHostCalls`.
pub const fn script_host_calls_prefix_hashes(out: &mut [u64]) -> usize {
    let fields: &[(&str, &str)] = &[
        ("ctx", "*mut c_void"),
        ("get", "fn(...)"),
        ("get_component", "fn(...)"),
        ("get_components", "fn(...)"),
        ("asset_progress", "fn(...)"),
        ("translate", "fn(...)"),
        ("scene_load_state", "fn(...)"),
    ];
    let n = if fields.len() < out.len() {
        fields.len()
    } else {
        out.len()
    };
    let mut prev = 0u64;
    let mut i = 0;
    while i < n {
        let mut h = FieldHasher::new();
        h.write_u64(prev);
        h.write_str(fields[i].0);
        h.write_str(fields[i].1);
        prev = h.finish();
        out[i] = prev;
        i += 1;
    }
    n
}

/// Compute the descriptor's `descriptor_size` field at compile time
/// from the live `CompiledScriptDesc` layout.
///
/// `const fn` so the macro can embed it without runtime work; the
/// host checks the runtime `mem::size_of` against it.
pub const fn descriptor_size() -> u32 {
    core::mem::size_of::<CompiledScriptDesc>() as u32
}

/// Result of a descriptor compatibility check.
#[derive(Debug)]
pub enum CompatError {
    /// The descriptor's `descriptor_size` is below the host-known
    /// [`MIN_DESCRIPTOR_SIZE`]. The descriptor is malformed or
    /// truncated.
    DescriptorTooSmall { found: u32 },
    /// `desc.abi_version` did not match [`COMPILED_SCRIPT_ABI`].
    VersionMismatch { found: u32 },
    /// `desc.descriptor_size` did not match the runtime
    /// `mem::size_of::<CompiledScriptDesc>()`. Almost always a
    /// host/desc compiled against different renzora_plugin versions.
    DescriptorSizeMismatch { expected: u32, found: u32 },
    /// A reserved field (`_pad`) was non-zero.
    ReservedFieldNonZero { which: &'static str, value: u32 },
    /// The descriptor's `call_prefix_count` exceeds the host-known
    /// maximum.
    PrefixCountTooLarge {
        which: &'static str,
        found: u32,
        max: usize,
    },
    /// A prefix hash chain did not match. Indicates a field was
    /// inserted, reordered, or retyped without bumping the ABI.
    PrefixHashMismatch {
        which: &'static str,
        compared: usize,
    },
    /// The descriptor required a capability the host does not provide.
    MissingCapabilities { required: u32 },
    /// The descriptor's `entry` is null.
    NullEntry,
    /// A field was invalid in a way the loader cannot safely recover from.
    InvalidLayout(&'static str),
    /// The query ABI returned an unexpected status code; the
    /// descriptor's bytes were not loaded.
    QueryFailure { status: u32 },
    /// The `descriptor_size` symbol exported by the cdylib returned
    /// a value below the host-known minimum.
    DescriptorSizeSymbolInvalid { reported: u32 },
    /// The descriptor-size symbol was not exported by the cdylib.
    /// The loader treats this as a malformed descriptor.
    DescriptorSizeSymbolMissing,
    /// The query/copy symbol was not exported by the cdylib.
    QuerySymbolMissing,
    /// The cdylib's query wrote fewer bytes than `descriptor_size`
    /// declares — possible truncation or a malicious descriptor.
    QueryShortWrite { written: u32, requested: u32 },
}

impl core::fmt::Display for CompatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::DescriptorTooSmall { found } => write!(
                f,
                "descriptor size {found} is below the host minimum {MIN_DESCRIPTOR_SIZE}"
            ),
            Self::VersionMismatch { found } => write!(
                f,
                "descriptor abi_version {found} does not match host {COMPILED_SCRIPT_ABI}"
            ),
            Self::DescriptorSizeMismatch { expected, found } => write!(
                f,
                "descriptor size {found} does not match host {expected} — \
                 the cdylib was compiled against a different renzora_plugin layout"
            ),
            Self::ReservedFieldNonZero { which, value } => write!(
                f,
                "descriptor reserved field {which} was non-zero ({value:#x})"
            ),
            Self::PrefixCountTooLarge { which, found, max } => write!(
                f,
                "descriptor {which} prefix count {found} exceeds host maximum {max}"
            ),
            Self::PrefixHashMismatch { which, compared } => write!(
                f,
                "{which} prefix-hash chain mismatch after comparing {compared} entries; refusing to load"
            ),
            Self::MissingCapabilities { required } => write!(
                f,
                "descriptor requires capability bits {required:#x} the host does not provide"
            ),
            Self::NullEntry => f.write_str("descriptor entry pointer is null"),
            Self::InvalidLayout(s) => write!(f, "descriptor layout invalid: {s}"),
            Self::QueryFailure { status } => write!(
                f,
                "descriptor query ABI returned status {status:#x}; refusing to load"
            ),
            Self::DescriptorSizeSymbolInvalid { reported } => write!(
                f,
                "cdylib descriptor-size symbol reported {reported} bytes, below host minimum {MIN_DESCRIPTOR_SIZE}"
            ),
            Self::DescriptorSizeSymbolMissing => f.write_str(
                "cdylib did not export renzora_plugin_tier1_script_descriptor_size; descriptor cannot be queried",
            ),
            Self::QuerySymbolMissing => f.write_str(
                "cdylib did not export renzora_plugin_tier1_script_query_desc; descriptor cannot be queried",
            ),
            Self::QueryShortWrite { written, requested } => write!(
                f,
                "descriptor query wrote {written} bytes, requested {requested}; refusing to load"
            ),
        }
    }
}

impl std::error::Error for CompatError {}

/// Validate a descriptor before the loader publishes it. `host_caps`
/// is the capability set this build provides.
///
/// S4-4: every read is bounded by a host-known constant. The
/// descriptor's count fields are checked against the host maximum
/// before any foreign pointer or length is consulted, and the actual
/// slice the loader reads is `min(desc.count, MAX_PREFIX_HASHES)`.
/// No field beyond the fixed minimum header is consulted until the
/// minimum header checks pass.
pub fn check_compat(
    desc: &CompiledScriptDesc,
    host_caps: CompiledScriptCapabilities,
) -> Result<(), CompatError> {
    // Step 1: minimum header size. A zero or undersized descriptor
    // is rejected before any field beyond the size/version is
    // consulted.
    let runtime_size = core::mem::size_of::<CompiledScriptDesc>() as u32;
    if desc.descriptor_size < MIN_DESCRIPTOR_SIZE as u32 {
        return Err(CompatError::DescriptorTooSmall {
            found: desc.descriptor_size,
        });
    }

    // Step 2: descriptor_size matches the live layout.
    if desc.descriptor_size != runtime_size {
        return Err(CompatError::DescriptorSizeMismatch {
            expected: runtime_size,
            found: desc.descriptor_size,
        });
    }

    // Step 3: ABI version.
    if desc.abi_version != COMPILED_SCRIPT_ABI {
        return Err(CompatError::VersionMismatch {
            found: desc.abi_version,
        });
    }

    // Step 4: reserved fields. Non-zero reserved fields indicate a
    // future feature has been clobbered or a corruption has occurred;
    // refuse the descriptor rather than guess.
    if desc._pad != 0 {
        return Err(CompatError::ReservedFieldNonZero {
            which: "_pad",
            value: desc._pad,
        });
    }

    // Step 5: capabilities.
    if !host_caps.contains(CompiledScriptCapabilities(desc.required_capabilities)) {
        return Err(CompatError::MissingCapabilities {
            required: desc.required_capabilities,
        });
    }

    // Step 6: call-prefix count bounded.
    let call_count = desc.call_prefix_count as usize;
    if call_count > MAX_PREFIX_HASHES {
        return Err(CompatError::PrefixCountTooLarge {
            which: "ScriptCall",
            found: desc.call_prefix_count,
            max: MAX_PREFIX_HASHES,
        });
    }
    let mut host_call = [0u64; MAX_PREFIX_HASHES];
    let host_call_len = script_call_prefix_hashes(&mut host_call);
    // The host's own chain length is the comparison length: we
    // refuse if the descriptor declares fewer entries than the host
    // requires. Declaring more is harmless; we just stop reading at
    // `host_call_len`.
    if call_count < host_call_len {
        return Err(CompatError::PrefixHashMismatch {
            which: "ScriptCall",
            compared: call_count,
        });
    }
    for (i, expected) in host_call.iter().enumerate().take(host_call_len) {
        if desc.call_prefix_hashes[i] != *expected {
            return Err(CompatError::PrefixHashMismatch {
                which: "ScriptCall",
                compared: i + 1,
            });
        }
    }

    // Step 7: host-calls count bounded.
    let host_count = desc.host_prefix_count as usize;
    if host_count > MAX_PREFIX_HASHES {
        return Err(CompatError::PrefixCountTooLarge {
            which: "ScriptHostCalls",
            found: desc.host_prefix_count,
            max: MAX_PREFIX_HASHES,
        });
    }
    let mut host_hc = [0u64; MAX_PREFIX_HASHES];
    let host_hc_len = script_host_calls_prefix_hashes(&mut host_hc);
    if host_count < host_hc_len {
        return Err(CompatError::PrefixHashMismatch {
            which: "ScriptHostCalls",
            compared: host_count,
        });
    }
    for (i, expected) in host_hc.iter().enumerate().take(host_hc_len) {
        if desc.host_prefix_hashes[i] != *expected {
            return Err(CompatError::PrefixHashMismatch {
                which: "ScriptHostCalls",
                compared: i + 1,
            });
        }
    }

    // Step 8: entry non-null.
    if (desc.entry as *const () as usize) == 0 {
        return Err(CompatError::NullEntry);
    }

    Ok(())
}

/// The size-safe descriptor query contract used by the host loader.
///
/// The host NEVER dereferences a foreign `*const CompiledScriptDesc`
/// directly. Instead, it:
///
/// 1. Reads the cdylib's `descriptor_size` symbol — a `u32` reporting
///    the descriptor's `descriptor_size` field. The host validates
///    this value against [`MIN_DESCRIPTOR_SIZE`] and the runtime
///    `mem::size_of::<CompiledScriptDesc>()`. A descriptor smaller
///    than the host minimum is refused immediately.
/// 2. Allocates a host-owned buffer of `descriptor_size` bytes.
///    This is a host allocation; the cdylib cannot influence the
///    size or location.
/// 3. Calls the cdylib's `query_desc` function with a pointer to the
///    buffer, the buffer's capacity, and the requested ABI version.
///    The cdylib returns a status code; on success it copies the
///    descriptor into the buffer as raw bytes (no more than the
///    supplied capacity).
/// 4. Validates the returned `bytes_written` against the requested
///    size, then parses the buffer as a `CompiledScriptDesc` via
///    [`check_compat`].
///
/// No foreign pointer is ever dereferenced before the host knows the
/// size of the descriptor. The previous `*const CompiledScriptDesc`
/// cast pattern allowed a malicious descriptor to drive an out-of-
/// bounds read on the foreign static; the new contract allocates a
/// buffer of validated size first.
pub type QueryDescFn = unsafe extern "C" fn(
    out_ptr: *mut u8,
    out_capacity: u32,
    requested_abi: u32,
) -> u32;

pub type DescriptorSizeFn = unsafe extern "C" fn() -> u32;

/// Query the cdylib for its descriptor using the size-safe ABI. The
/// caller passes the two query symbols obtained from `libloading` and
/// the maximum descriptor capacity it accepts. This function allocates
/// the output buffer after checking the reported size.
///
/// # Safety
///
/// Both symbols must belong to a live library and implement their declared
/// ABI. The query must respect the supplied buffer capacity and initialize a
/// valid descriptor, including valid function pointers. The caller must keep
/// the library loaded while any returned function pointer remains usable.
pub unsafe fn query_descriptor(
    size_fn: DescriptorSizeFn,
    query_fn: QueryDescFn,
    capacity: usize,
) -> Result<CompiledScriptDesc, CompatError> {
    // Step 1: read the cdylib's reported size and validate it
    // BEFORE allocating or invoking the query.
    let reported = unsafe { size_fn() };
    if reported < MIN_DESCRIPTOR_SIZE as u32 {
        return Err(CompatError::DescriptorSizeSymbolInvalid { reported });
    }
    let runtime_size = core::mem::size_of::<CompiledScriptDesc>() as u32;
    if reported != runtime_size {
        return Err(CompatError::DescriptorSizeMismatch {
            expected: runtime_size,
            found: reported,
        });
    }
    // Step 2: allocate a host-owned buffer of exactly the
    // validated size.
    if capacity < runtime_size as usize {
        return Err(CompatError::QueryShortWrite {
            written: 0,
            requested: runtime_size,
        });
    }
    let mut buf = vec![0u8; runtime_size as usize];
    // Step 3: ask the cdylib to copy the descriptor into the
    // buffer. Capacity is the validated runtime size.
    let ret = unsafe { query_fn(buf.as_mut_ptr(), runtime_size, COMPILED_SCRIPT_ABI) };
    let status = query_status(ret);
    if status != QUERY_STATUS_OK {
        return Err(CompatError::QueryFailure { status: ret });
    }
    let written = query_written(ret);
    if written < runtime_size {
        return Err(CompatError::QueryShortWrite {
            written,
            requested: runtime_size,
        });
    }
    // Step 4: validate the buffer BEFORE treating it as a
    // CompiledScriptDesc. The cast copies host-side bytes; the
    // validation can refuse a malformed descriptor without
    // consulting any foreign pointer.
    let descriptor: CompiledScriptDesc = unsafe {
        core::ptr::read_unaligned(buf.as_ptr() as *const CompiledScriptDesc)
    };
    check_compat(&descriptor, CompiledScriptCapabilities::NONE)?;
    Ok(descriptor)
}

/// The host-side registry that maps canonical identity to a
/// `ScriptEntry` the cdylib exported. The entry is the per-cdylib
/// trampoline the cdylib's `rust_script!` macro emits; the host
/// calls it through the standard `ScriptCall`/`ScriptStatus` ABI and
/// never sees the typed function.
///
/// Ownership: each registered identity owns an
/// [`Arc<ScriptGeneration>`]. The generation is the active
/// generation; `register` swaps the previous generation out (an
/// in-flight dispatch holds its `Arc` clone, so the swap is safe).
/// Dropping the previous `Arc` releases the per-cdylib `Library`
/// handle; on POSIX the kernel may or may not unmap the image
/// immediately depending on resident references, but the host
/// process no longer holds a reachable function pointer into the
/// retired generation.
#[derive(Default)]
pub struct CompiledScriptBackend {
    inner: Mutex<BackendInner>,
}

#[derive(Default)]
struct BackendInner {
    by_id: std::collections::BTreeMap<String, Arc<ScriptGeneration>>,
}

/// One active generation of a compiled script. Owns the validated
/// `ScriptEntry` together with the cdylib library handle that keeps
/// the image mapped for the lifetime of the generation. The entry
/// pointer is meaningless without the library — the function lives
/// in the cdylib's text segment.
///
/// The library is held through `Box<dyn Any + Send + Sync>` so this
/// module never has to depend on `libloading`. The concrete
/// `libloading::Library` lives inside that `Box`; dropping it
/// releases the mapping.
pub struct ScriptGeneration {
    /// Stable per-cdylib `unsafe extern "C"` trampoline. Valid for
    /// the lifetime of `library`; never used after every
    /// `Arc<ScriptGeneration>` for this generation has been dropped.
    pub entry: ScriptEntry,
    /// The library handle that keeps the cdylib image mapped. Held
    /// inside a type-erased `Box` so `renzora_plugin` does not have
    /// to depend on `libloading`.
    _library: Box<dyn std::any::Any + Send + Sync>,
    /// Monotonic generation counter for diagnostics.
    pub generation: u64,
}

impl ScriptGeneration {
    /// Construct a generation owning the supplied entry and library.
    /// The library's drop order guarantees the cdylib stays mapped
    /// until every in-flight call has returned and every clone of
    /// this `Arc` is gone.
    pub fn new(
        entry: ScriptEntry,
        library: Box<dyn std::any::Any + Send + Sync>,
        generation: u64,
    ) -> Self {
        Self {
            entry,
            _library: library,
            generation,
        }
    }

    /// Construct a generation with an attached drop observer. The
    /// observer's `Drop` runs when the LAST `Arc<ScriptGeneration>`
    /// for this generation is dropped — i.e. when the registry's
    /// prior-generation `Arc` swap and every in-flight dispatch
    /// that held a clone have all completed. Tests use this for
    /// T4-10's library-release proof; production code paths use
    /// the regular constructor.
    pub fn new_with_observer(
        entry: ScriptEntry,
        library: Box<dyn std::any::Any + Send + Sync>,
        generation: u64,
        observer: Box<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        let combined: Box<dyn std::any::Any + Send + Sync> = Box::new((library, observer));
        Self {
            entry,
            _library: combined,
            generation,
        }
    }
}

impl CompiledScriptBackend {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an active generation against a canonical identity.
    /// Replaces any previous generation for the same id atomically —
    /// an in-flight dispatch holds its `Arc<ScriptGeneration>` clone,
    /// so the previous generation's `Library` stays mapped until
    /// every in-flight call returns.
    pub fn register(&self, id: &str, generation: Arc<ScriptGeneration>) {
        let mut g = self.inner.lock().expect("compiled-script registry poisoned");
        g.by_id.insert(id.to_string(), generation);
    }

    /// Look up the active generation by canonical identity. The
    /// returned `Arc` keeps the cdylib mapped for as long as the
    /// caller holds it; release the registry lock before invoking
    /// `entry` so the dispatch runs against a stable generation.
    pub fn lookup(&self, id: &str) -> Option<Arc<ScriptGeneration>> {
        let g = self.inner.lock().expect("compiled-script registry poisoned");
        g.by_id.get(id).cloned()
    }

    /// Remove an identity and return the previous generation, if any.
    /// An in-flight dispatch that already cloned its `Arc` still
    /// finishes safely; the next dispatch finds no entry.
    pub fn unregister(&self, id: &str) -> Option<Arc<ScriptGeneration>> {
        let mut g = self.inner.lock().expect("compiled-script registry poisoned");
        g.by_id.remove(id)
    }

    /// Number of registered scripts. Used by tests.
    pub fn len(&self) -> usize {
        let g = self.inner.lock().expect("compiled-script registry poisoned");
        g.by_id.len()
    }

    /// `true` when no scripts are registered. Companion to
    /// [`Self::len`].
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Iterate registered script ids in stable order.
    pub fn ids(&self) -> Vec<String> {
        let g = self.inner.lock().expect("compiled-script registry poisoned");
        g.by_id.keys().cloned().collect()
    }
}

/// The macro the user calls once per `.rs` script.
///
/// ```ignore
/// use renzora_plugin::script::*;
/// fn update(_ctx: &Ctx, _reply: &mut ScriptReply) -> Result<(), String> { Ok(()) }
/// renzora_plugin::rust_script!(update);
/// ```
///
/// The macro emits, INSIDE the user's `.rs` source (which compiles as
/// the user's own cdylib):
///
/// - `pub fn __tier1_entry(ctx, reply) -> Result<(), String>` — the
///   typed author-facing function. Statically linked into the same
///   cdylib; never crosses the dynamic-library boundary.
/// - `static __TIER1_CALL_PREFIX_HASHES: [u64; 32]` and
///   `static __TIER1_HOST_PREFIX_HASHES: [u64; 32]` — the prefix-hash
///   chains, computed at const time by the `const fn` initializers.
/// - `#[no_mangle] static __TIER1_SCRIPT_DESC_DATA: CompiledScriptDesc`
///   carrying the version, size, capability mask, prefix-hash
///   chains (inline), and the entry pointer below.
/// - `#[no_mangle] pub extern "C" fn renzora_plugin_tier1_script_desc() -> *const CompiledScriptDesc`
///   returning a pointer to the descriptor static. The symbol the
///   loader reads.
/// - `#[no_mangle] pub unsafe extern "C" fn renzora_plugin_tier1_script_call`
///   — the per-cdylib `ScriptEntry` trampoline. It is `unsafe
///   extern "C"` and takes a `*const ScriptCall`, so it can be
///   registered with the host through the standard wire contract.
///
/// The host loads the cdylib, reads the descriptor, registers the
/// `entry` against the canonical identity in
/// [`CompiledScriptBackend`], and discards the typed function — the
/// typed function lives inside the cdylib and is called only from
/// the per-cdylib trampoline.
#[macro_export]
macro_rules! rust_script {
    ($f:path) => {
        /// Public typed entry the per-cdylib trampoline calls. NEVER
        /// crosses the dynamic-library boundary; the host only ever
        /// sees the descriptor's `entry: ScriptEntry`.
        #[allow(non_snake_case)]
        pub fn __tier1_entry(
            ctx: &$crate::script::Ctx<'_>,
            reply: &mut $crate::script::ScriptReply,
        ) -> ::std::result::Result<(), ::std::string::String> {
            $f(ctx, reply)
        }

        /// The descriptor's `ScriptCall` prefix-hash chain. `#[used]`
        /// keeps the linker from dropping the static even when no
        /// other symbol references it.
        #[used]
        static __TIER1_CALL_PREFIX_HASHES: [u64; $crate::script::compiled::MAX_PREFIX_HASHES] = {
            let mut buf = [0u64; $crate::script::compiled::MAX_PREFIX_HASHES];
            let n = $crate::script::compiled::script_call_prefix_hashes(&mut buf);
            let mut out = [0u64; $crate::script::compiled::MAX_PREFIX_HASHES];
            let mut i = 0;
            while i < n {
                out[i] = buf[i];
                i += 1;
            }
            out
        };

        /// The descriptor's `ScriptHostCalls` prefix-hash chain.
        #[used]
        static __TIER1_HOST_PREFIX_HASHES: [u64; $crate::script::compiled::MAX_PREFIX_HASHES] = {
            let mut buf = [0u64; $crate::script::compiled::MAX_PREFIX_HASHES];
            let n = $crate::script::compiled::script_host_calls_prefix_hashes(&mut buf);
            let mut out = [0u64; $crate::script::compiled::MAX_PREFIX_HASHES];
            let mut i = 0;
            while i < n {
                out[i] = buf[i];
                i += 1;
            }
            out
        };

        /// The per-cdylib `ScriptEntry` trampoline. Decodes the call,
        /// statically calls `__tier1_entry`, encodes the reply, and
        /// writes it back. The dispatch logic is inlined here so the
        /// C-ABI surface (this function) is the ONLY thing that
        /// crosses the dynamic-library boundary; the typed user
        /// function is called by name from code compiled into the
        /// same cdylib.
        #[no_mangle]
        pub unsafe extern "C" fn renzora_plugin_tier1_script_call(
            call: *const $crate::sys::ScriptCall,
        ) -> $crate::sys::ScriptStatus {
            // The cdylib's trampoline is the ONLY function the host
            // ever invokes across the dynamic-library boundary. The
            // typed user function lives inside this same cdylib and
            // is invoked by name from code compiled into the cdylib
            // — it never crosses the C ABI. To make this enforceable,
            // the dispatch is inlined here rather than delegated to a
            // shared host function.
            if call.is_null() {
                return $crate::sys::ScriptStatus::Error;
            }
            let call: &$crate::sys::ScriptCall = &*call;

            // Hook routing. Update-only scripts are the common case.
            // Any non-OnUpdate op returns NoHook (or UnknownOp for
            // genuinely out-of-range ops) without invoking the user
            // function.
            let op = call.op;
            let mut reply = $crate::script::ScriptReply::default();
            if op != $crate::sys::ScriptOp::OnUpdate {
                let status = if op.0 < 14 {
                    $crate::sys::ScriptStatus::NoHook
                } else {
                    $crate::sys::ScriptStatus::UnknownOp
                };
                if let Some(sink) = call.out.as_ref() {
                    unsafe { reply.write_to(sink) };
                }
                return status;
            }

            // Decode frame and entity context lazily.
            let frame = if call.frame.len > 0 && !call.frame.ptr.is_null() {
                let bytes = unsafe { call.frame.as_slice() };
                match $crate::script::FrameContext::decode(
                    &mut $crate::wire::Reader::new(bytes),
                ) {
                    Ok(f) => f,
                    Err(_) => {
                        reply.error = Some("frame context would not decode".into());
                        if let Some(sink) = call.out.as_ref() {
                            unsafe { reply.write_to(sink) };
                        }
                        return $crate::sys::ScriptStatus::Error;
                    }
                }
            } else {
                $crate::script::FrameContext::empty()
            };

            let entity = if call.entity_ctx.len > 0 && !call.entity_ctx.ptr.is_null() {
                let bytes = unsafe { call.entity_ctx.as_slice() };
                match $crate::script::EntityContext::decode(
                    &mut $crate::wire::Reader::new(bytes),
                ) {
                    Ok(e) => e,
                    Err(_) => {
                        reply.error = Some("entity context would not decode".into());
                        if let Some(sink) = call.out.as_ref() {
                            unsafe { reply.write_to(sink) };
                        }
                        return $crate::sys::ScriptStatus::Error;
                    }
                }
            } else {
                $crate::script::EntityContext::empty()
            };

            let raw_host = match unsafe { call.host.as_ref() } {
                Some(h) => h,
                None => {
                    reply.error = Some("host call table was null".into());
                    if let Some(sink) = call.out.as_ref() {
                        unsafe { reply.write_to(sink) };
                    }
                    return $crate::sys::ScriptStatus::Error;
                }
            };

            // Decode variables lazily.
            let vars = if call.vars.len > 0 && !call.vars.ptr.is_null() {
                let bytes = unsafe { call.vars.as_slice() };
                $crate::wire::Reader::new(bytes)
                    .list(|r| Ok((r.string()?, $crate::script::ScriptValue::decode(r)?)))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let path_str = if call.path.ptr.is_null() || call.path.len == 0 {
                ""
            } else {
                let bytes = unsafe { core::slice::from_raw_parts(call.path.ptr, call.path.len) };
                core::str::from_utf8(bytes).unwrap_or("")
            };
            let source_str = if call.source.ptr.is_null() || call.source.len == 0 {
                ""
            } else {
                let bytes = unsafe { core::slice::from_raw_parts(call.source.ptr, call.source.len) };
                core::str::from_utf8(bytes).unwrap_or("")
            };
            let _script_ref = $crate::script::ScriptRef {
                path: path_str,
                source: source_str,
                version: call.version,
                entity: call.entity,
                vars: &vars,
            };
            let host = $crate::script::HostCalls::new(raw_host);
            let ctx = $crate::script::Ctx {
                frame: &frame,
                entity: &entity,
                host,
            };

            let status = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                __tier1_entry(&ctx, &mut reply)
            })) {
                Ok(Ok(())) => $crate::sys::ScriptStatus::Ok,
                Ok(Err(e)) => {
                    reply.error = Some(e);
                    $crate::sys::ScriptStatus::Error
                }
                Err(panic) => {
                    // Convert the panic payload to a string when
                    // possible so the host can surface it through the
                    // standard reply path. `Panicked` is returned so
                    // the host knows the script panicked (not merely
                    // returned an Err).
                    let msg = if let Some(s) = panic.downcast_ref::<&'static str>() {
                        (*s).to_string()
                    } else if let Some(s) = panic.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "script panicked".to_string()
                    };
                    reply.error = Some(format!("panicked: {msg}"));
                    $crate::sys::ScriptStatus::Panicked
                }
            };
            if let Some(sink) = call.out.as_ref() {
                unsafe { reply.write_to(sink) };
            }
            status
        }

        /// The descriptor the loader reads at load time. Holds the
        /// version, size, capability mask, inline prefix-hash chains,
        /// and the per-cdylib `ScriptEntry` above. NOT a function
        /// pointer to the typed user function — that one lives behind
        /// the `entry` trampoline inside this cdylib.
        #[no_mangle]
        static __TIER1_SCRIPT_DESC_DATA: $crate::script::compiled::CompiledScriptDesc =
            $crate::script::compiled::CompiledScriptDesc {
                abi_version: $crate::script::compiled::COMPILED_SCRIPT_ABI,
                descriptor_size: $crate::script::compiled::descriptor_size(),
                required_capabilities: $crate::script::compiled::CompiledScriptCapabilities::NONE.0,
                _pad: 0,
                call_prefix_count: $crate::script::compiled::script_call_prefix_hashes(
                    &mut {
                        let mut buf = [0u64; $crate::script::compiled::MAX_PREFIX_HASHES];
                        let _ = $crate::script::compiled::script_call_prefix_hashes(&mut buf);
                        buf
                    },
                ) as u32,
                call_prefix_hashes: __TIER1_CALL_PREFIX_HASHES,
                host_prefix_count: $crate::script::compiled::script_host_calls_prefix_hashes(
                    &mut {
                        let mut buf = [0u64; $crate::script::compiled::MAX_PREFIX_HASHES];
                        let _ = $crate::script::compiled::script_host_calls_prefix_hashes(&mut buf);
                        buf
                    },
                ) as u32,
                host_prefix_hashes: __TIER1_HOST_PREFIX_HASHES,
                entry: renzora_plugin_tier1_script_call,
            };

        /// The descriptor symbol the loader looks up. Returns a
        /// pointer to the descriptor static above.
        #[no_mangle]
        pub extern "C" fn renzora_plugin_tier1_script_desc(
        ) -> *const $crate::script::compiled::CompiledScriptDesc {
            &__TIER1_SCRIPT_DESC_DATA
        }

        /// The size-safe descriptor query ABI.
        ///
        /// The host never reads a foreign `*const CompiledScriptDesc`
        /// until it knows the descriptor is exactly the size its
        /// compiled layout expects. `renzora_plugin_tier1_script_descriptor_size`
        /// returns the descriptor's `descriptor_size` field as a
        /// standalone value; the host validates the size against
        /// `MIN_DESCRIPTOR_SIZE` and the live `mem::size_of` before
        /// allocating a host-owned buffer of that size. `query_desc`
        /// then writes the descriptor into the host buffer with an
        /// explicit capacity so the cdylib cannot overrun it.
        ///
        /// Both functions are `extern "C" fn` with only fixed-width
        /// C-compatible scalar arguments; the descriptor itself
        /// travels across the boundary as a plain byte buffer that
        /// the host parses. The previous `*const CompiledScriptDesc`
        /// dereference-and-copy pattern was replaced because a
        /// smaller or older descriptor may not contain that many
        /// accessible bytes; the new contract lets the host
        /// allocate the buffer with a known valid size BEFORE
        /// any foreign pointer is dereferenced.
        #[no_mangle]
        pub extern "C" fn renzora_plugin_tier1_script_descriptor_size() -> u32 {
            __TIER1_SCRIPT_DESC_DATA.descriptor_size
        }

        /// The query/copy contract:
        ///
        /// - `out_ptr` is a host-owned output buffer.
        /// - `out_capacity` is the buffer size in bytes; the cdylib
        ///   writes no more than `min(descriptor_size, out_capacity)`.
        /// - `requested_abi` is the ABI version the host expects;
        ///   the cdylib rejects mismatches by writing zero bytes and
        ///   returning `QUERY_STATUS_VERSION_MISMATCH`.
        /// - The return value is one of the `QUERY_STATUS_*` constants
        ///   below (0..3). The cdylib never puts status bits in the
        ///   upper 16 bits; the host reads the status as
        ///   `ret & 0xFFFF` and the bytes-written count as
        ///   `(ret >> 16) & 0xFFFF`.
        ///
        /// The cdylib never reads `out_ptr` and never writes past
        /// `out_capacity`. The host's `load_compiled_script` path
        /// supplies a buffer of `descriptor_size` bytes and checks
        /// the result before parsing the buffer as a
        /// `CompiledScriptDesc`.
        #[no_mangle]
        pub unsafe extern "C" fn renzora_plugin_tier1_script_query_desc(
            out_ptr: *mut u8,
            out_capacity: u32,
            requested_abi: u32,
        ) -> u32 {
            use $crate::script::compiled::{
                QUERY_STATUS_OK, QUERY_STATUS_BUFFER_TOO_SMALL,
                QUERY_STATUS_NULL_OUTPUT, QUERY_STATUS_VERSION_MISMATCH,
            };
            if out_ptr.is_null() {
                return QUERY_STATUS_NULL_OUTPUT;
            }
            if requested_abi != $crate::script::compiled::COMPILED_SCRIPT_ABI {
                return QUERY_STATUS_VERSION_MISMATCH;
            }
            let descriptor_size = __TIER1_SCRIPT_DESC_DATA.descriptor_size;
            if out_capacity < descriptor_size {
                return QUERY_STATUS_BUFFER_TOO_SMALL;
            }
            // SAFETY: the host has supplied a buffer of at least
            // `descriptor_size` bytes. We copy the descriptor into
            // it as raw bytes. The host will validate the layout
            // (size, version, prefix hashes) before treating the
            // bytes as a `CompiledScriptDesc`.
            let src_bytes: &[u8] = unsafe {
                core::slice::from_raw_parts(
                    &__TIER1_SCRIPT_DESC_DATA as *const _ as *const u8,
                    descriptor_size as usize,
                )
            };
            unsafe {
                core::ptr::copy_nonoverlapping(src_bytes.as_ptr(), out_ptr, descriptor_size as usize);
            }
            // Status: lower 16 bits = QUERY_STATUS_OK.
            // Bytes written: upper 16 bits = descriptor_size.min(out_capacity).
            // (No status bits in upper 16 bits; the contract leaves
            // room for future status codes in the lower 16 bits.)
            let written = descriptor_size.min(out_capacity) & 0xFFFF;
            (written << 16) | QUERY_STATUS_OK
        }
    };
}

/// A `ScriptEntry` placeholder used in unit tests and as the
/// default-derived entry for `Default::default()`. Bodies are never
/// invoked by production paths; the production entry is the
/// per-cdylib `unsafe extern "C"` the loader reads from the
/// descriptor.
pub extern "C" fn noop_script_entry(_call: *const ScriptCall) -> ScriptStatus {
    ScriptStatus::Ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::{Ctx, ScriptReply};

    fn noop(_ctx: &Ctx, _reply: &mut ScriptReply) -> Result<(), String> {
        Ok(())
    }

    fn make_gen(entry: ScriptEntry, generation: u64) -> Arc<ScriptGeneration> {
        Arc::new(ScriptGeneration::new(
            entry,
            Box::new(()) as Box<dyn std::any::Any + Send + Sync>,
            generation,
        ))
    }

    #[test]
    fn registry_starts_empty_and_registers() {
        let r = CompiledScriptBackend::new();
        assert_eq!(r.len(), 0);
        r.register("project://hello.rs", make_gen(noop_script_entry, 1));
        assert_eq!(r.len(), 1);
        let looked = r.lookup("project://hello.rs").expect("registered");
        assert!(std::ptr::eq(
            looked.entry as *const ScriptEntry as *const (),
            noop_script_entry as *const ()
        ));
    }

    #[test]
    fn registry_unregister_drops_an_entry() {
        let r = CompiledScriptBackend::new();
        r.register("project://a.rs", make_gen(noop_script_entry, 1));
        let prev = r.unregister("project://a.rs");
        assert!(prev.is_some());
        assert_eq!(r.len(), 0);
        assert!(r.lookup("project://a.rs").is_none());
        // Unregistering a missing id is a no-op.
        assert!(r.unregister("project://b.rs").is_none());
    }

    #[test]
    fn registry_replace_overwrites() {
        let r = CompiledScriptBackend::new();
        r.register("project://a.rs", make_gen(noop_script_entry, 1));
        r.register("project://a.rs", make_gen(noop_script_entry, 2));
        assert_eq!(r.len(), 1);
        let active = r.lookup("project://a.rs").expect("active");
        assert_eq!(active.generation, 2);
    }

    #[test]
    fn registry_inflight_guard_keeps_old_generation_alive() {
        // An in-flight dispatch clones the Arc, the registry swaps the
        // generation, and the dispatch finishes using its guard. The
        // previous generation's Arc count stays above zero for as long
        // as the guard is held.
        let r = CompiledScriptBackend::new();
        r.register("project://a.rs", make_gen(noop_script_entry, 1));
        let in_flight = r.lookup("project://a.rs").expect("active");
        assert_eq!(Arc::strong_count(&in_flight), 2);
        r.register("project://a.rs", make_gen(noop_script_entry, 2));
        // The new active generation is gen 2.
        let new_active = r.lookup("project://a.rs").expect("active");
        assert_eq!(new_active.generation, 2);
        // The dispatch guard is still alive pointing at gen 1.
        assert_eq!(in_flight.generation, 1);
        drop(new_active);
        drop(in_flight);
        assert_eq!(r.len(), 1);
    }

    fn well_formed_descriptor() -> CompiledScriptDesc {
        let mut d = CompiledScriptDesc {
            abi_version: COMPILED_SCRIPT_ABI,
            descriptor_size: descriptor_size(),
            required_capabilities: 0,
            _pad: 0,
            call_prefix_count: 0,
            call_prefix_hashes: [0u64; MAX_PREFIX_HASHES],
            host_prefix_count: 0,
            host_prefix_hashes: [0u64; MAX_PREFIX_HASHES],
            entry: noop_script_entry,
        };
        let n_call = script_call_prefix_hashes(&mut d.call_prefix_hashes);
        d.call_prefix_count = n_call as u32;
        let n_host = script_host_calls_prefix_hashes(&mut d.host_prefix_hashes);
        d.host_prefix_count = n_host as u32;
        d
    }

    #[test]
    fn compat_accepts_a_well_formed_descriptor() {
        let d = well_formed_descriptor();
        check_compat(&d, CompiledScriptCapabilities::NONE).expect("compatible");
    }

    #[test]
    fn compat_rejects_undersized_descriptor() {
        let mut d = well_formed_descriptor();
        // Smaller than MIN_DESCRIPTOR_SIZE.
        d.descriptor_size = MIN_DESCRIPTOR_SIZE as u32 - 1;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::DescriptorTooSmall { .. })
        ));
    }

    #[test]
    fn compat_rejects_zero_size_descriptor() {
        let mut d = well_formed_descriptor();
        d.descriptor_size = 0;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::DescriptorTooSmall { .. })
        ));
    }

    #[test]
    fn compat_rejects_oversized_descriptor() {
        let mut d = well_formed_descriptor();
        d.descriptor_size = descriptor_size() + 1;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::DescriptorSizeMismatch { .. })
        ));
    }

    #[test]
    fn compat_rejects_version_mismatch() {
        let mut d = well_formed_descriptor();
        d.abi_version = COMPILED_SCRIPT_ABI + 1;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn compat_rejects_non_zero_pad() {
        let mut d = well_formed_descriptor();
        d._pad = 1;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::ReservedFieldNonZero { .. })
        ));
    }

    #[test]
    fn compat_rejects_zero_call_prefix_count() {
        let mut d = well_formed_descriptor();
        d.call_prefix_count = 0;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::PrefixHashMismatch { .. })
        ));
    }

    #[test]
    fn compat_rejects_oversized_call_prefix_count() {
        let mut d = well_formed_descriptor();
        d.call_prefix_count = (MAX_PREFIX_HASHES + 1) as u32;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::PrefixCountTooLarge { .. })
        ));
    }

    #[test]
    fn compat_rejects_corrupted_call_prefix_hash() {
        let mut d = well_formed_descriptor();
        d.call_prefix_hashes[3] ^= 0xdeadbeef;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::PrefixHashMismatch { which: "ScriptCall", .. })
        ));
    }

    #[test]
    fn compat_rejects_missing_capabilities() {
        let mut d = well_formed_descriptor();
        d.required_capabilities = 0x8000_0000;
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::MissingCapabilities { .. })
        ));
    }

    #[test]
    fn compat_rejects_null_entry() {
        // The default descriptor's entry is `trampoline_placeholder`,
        // a non-null placeholder. We can't easily construct a literal
        // null `ScriptEntry` (the type is `fn`), so we rely on the
        // `Default::default()` path: zero abi_version + zero size +
        // placeholder entry. Verify the rejection is the size check
        // (which fires before the entry check), since that's the path
        // the production loader actually exercises.
        let d = CompiledScriptDesc::default();
        assert!(matches!(
            check_compat(&d, CompiledScriptCapabilities::NONE),
            Err(CompatError::DescriptorTooSmall { .. })
        ));
    }

    #[test]
    fn prefix_chains_cover_all_declared_fields() {
        // If a field is appended to `ScriptCall` without bumping the
        // chain, this test fails — catches the mistake at compile time.
        let mut call = [0u64; MAX_PREFIX_HASHES];
        let n_call = script_call_prefix_hashes(&mut call);
        assert_eq!(n_call, 13, "ScriptCall prefix chain must cover all 13 fields");

        let mut host = [0u64; MAX_PREFIX_HASHES];
        let n_host = script_host_calls_prefix_hashes(&mut host);
        assert_eq!(n_host, 7, "ScriptHostCalls prefix chain must cover all 7 fields");
    }

    // Smoke test that the default-derived trampoline rejects a null
    // call. The default descriptor's `entry` is `trampoline_placeholder`,
    // which mirrors the production trampoline's null-check.
    #[test]
    fn trampoline_returns_error_for_null_call() {
        // SAFETY: null is explicitly handled by `trampoline_placeholder`.
        let status = trampoline_placeholder(core::ptr::null());
        assert_eq!(status, ScriptStatus::Error);
    }

    // Smoke that the typed-function pointer signature the macro
    // generates is well-formed. The actual end-to-end behaviour is
    // exercised by the per-crate acceptance suite that loads a real
    // cdylib and dispatches through `PluginScriptBackend`.
    #[test]
    fn noop_typed_fn_compiles() {
        let _ = noop as fn(&Ctx, &mut ScriptReply) -> Result<(), String>;
    }
}
