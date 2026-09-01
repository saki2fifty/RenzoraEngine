//! Versioned, length-delimited `BuildFingerprint`.
//!
//! See `phase2-cached-compiler-design.md` §2 for the architecture. The cache
//! key is `Blake3(serialized_fingerprint)` (full 32 bytes / 64 hex chars /
//! 256 bits — no truncation). On every cache hit the loader re-reads the
//! stored `gen-<N>/fingerprint.bin`, re-serializes the candidate fingerprint
//! from the current inputs, and compares byte-for-byte (§2.3).

use std::collections::BTreeSet;

use renzora_identity::CanonicalId;

/// Current schema version. Bumped when the field table itself changes;
/// existing caches must be rebuilt after a bump.
pub const FINGERPRINT_SCHEMA_VERSION: u32 = 2;

/// 32-byte Blake3 output, displayed as 64 hex characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BuildKey(pub [u8; 32]);

impl BuildKey {
    /// All-zero key — reserved for the "no fingerprint yet" state.
    pub const ZERO: Self = Self([0; 32]);

    /// Hex-encode the key.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Parse a hex-encoded key.
    pub fn from_hex(s: &str) -> Result<Self, String> {
        if s.len() != 64 {
            return Err(format!("build key hex must be 64 chars, got {}", s.len()));
        }
        let mut out = [0u8; 32];
        for i in 0..32 {
            let byte_str = &s[i * 2..i * 2 + 2];
            out[i] = u8::from_str_radix(byte_str, 16)
                .map_err(|e| format!("invalid hex at byte {i}: {e}"))?;
        }
        Ok(Self(out))
    }
}

/// 32-byte content hash (used for the SDK and the lockfile).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    /// All-zero content hash.
    pub const ZERO: Self = Self([0; 32]);

    /// SHA-256 of `bytes`, returned as a `ContentHash`.
    pub fn sha256(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let out = hasher.finalize();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&out);
        Self(arr)
    }

    /// Blake3 of `bytes`, returned as a `ContentHash`.
    pub fn blake3(bytes: &[u8]) -> Self {
        let out = blake3::hash(bytes);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(out.as_bytes());
        Self(arr)
    }

    /// Hex-encode.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(64);
        for b in &self.0 {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

/// The ABI stamp — version + sorted `INTERFACE_PREFIX_HASHES`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AbiStamp {
    /// ABI version (bumped when the surface changes).
    pub version: u32,
    /// Each u32 LE is the hash of one C-ABI symbol prefix.
    pub interface_prefix_hashes: Vec<u32>,
}

/// Helper for tests / external callers — a stable 32-byte representation of
/// the source content. Equal sources yield equal hashes.
pub fn source_content_hash(bytes: &[u8]) -> ContentHash {
    ContentHash::blake3(bytes)
}

/// The versioned build fingerprint. See `phase2-cached-compiler-design.md`
/// §2.1.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BuildFingerprint {
    /// Always [`FINGERPRINT_SCHEMA_VERSION`]. Bumped when the field table
    /// itself changes.
    pub schema_version: u32,
    /// Canonical identity (already covers scheme + path).
    pub canonical_identity: CanonicalId,
    /// Exact source bytes (length-delimited prefix in serialization).
    pub source_content: Vec<u8>,
    /// Target triple string.
    pub target_triple: String,
    /// Full `rustc -Vv` output captured at SDK build time.
    pub toolchain_stamp: String,
    /// 32-byte content hash of the Tier 1 SDK package.
    pub sdk_content_hash: ContentHash,
    /// ABI stamp (version + sorted interface-prefix hashes).
    pub abi: AbiStamp,
    /// Wrapper template schema version.
    pub wrapper_schema: u32,
    /// BLAKE3 / SHA-256 of the actual generated wrapper content
    /// (workspace + per-script manifests + src/lib.rs). When the schema
    /// bumps, both fields change together.
    pub wrapper_content_hash: ContentHash,
    /// Manifest schema version (the on-disk layout of `active.bin` /
    /// `fingerprint.bin` / `status.bin`).
    pub manifest_schema: u32,
    /// Hash of the manifest schema descriptor.
    pub manifest_content_hash: ContentHash,
    /// Resolved lockfile content hash.
    pub lock_resolution: ContentHash,
    /// Enabled capabilities / features — `runtime`, `static_plugins`, …
    pub capabilities: BTreeSet<String>,
    /// Build profile.
    pub profile: ProfileTag,
    /// Cargo `--config` profile overrides.
    pub rustflags: Vec<String>,
    /// Panic strategy.
    pub panic: PanicTag,
    /// Crate type.
    pub crate_type: CrateTypeTag,
    /// Version of `compiler_cache` itself.
    pub compiler_service_schema: u32,
}

/// Stable string tag for build profile (avoids importing `BuildProfile` here).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProfileTag {
    /// `dist`.
    Dist,
    /// `dist-lean`.
    DistLean,
}

impl ProfileTag {
    /// String form.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProfileTag::Dist => "dist",
            ProfileTag::DistLean => "dist-lean",
        }
    }
}

/// Single-byte tag for panic strategy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanicTag {
    /// `panic = "abort"`.
    Abort,
    /// `panic = "unwind"`.
    Unwind,
}

impl PanicTag {
    /// Single-byte tag.
    pub fn byte(&self) -> u8 {
        match self {
            PanicTag::Abort => 0,
            PanicTag::Unwind => 1,
        }
    }
}

/// Single-byte tag for crate type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CrateTypeTag {
    /// `cdylib`.
    Cdylib,
    /// `staticlib`.
    Staticlib,
    /// `dylib`.
    Dylib,
}

impl CrateTypeTag {
    /// Single-byte tag.
    pub fn byte(&self) -> u8 {
        match self {
            CrateTypeTag::Cdylib => 0,
            CrateTypeTag::Staticlib => 1,
            CrateTypeTag::Dylib => 2,
        }
    }
}

impl BuildFingerprint {
    /// Construct a fingerprint with the minimum required fields, plus the
    /// current schema version. Other fields keep their default values
    /// (`String::new()`, `[0; 32]`, empty vectors, …).
    pub fn new(identity: CanonicalId, source: Vec<u8>) -> Self {
        Self {
            schema_version: FINGERPRINT_SCHEMA_VERSION,
            canonical_identity: identity,
            source_content: source,
            target_triple: String::new(),
            toolchain_stamp: String::new(),
            sdk_content_hash: ContentHash::ZERO,
            abi: AbiStamp {
                version: 1,
                interface_prefix_hashes: Vec::new(),
            },
            wrapper_schema: 1,
            wrapper_content_hash: ContentHash::ZERO,
            manifest_schema: 1,
            manifest_content_hash: ContentHash::ZERO,
            lock_resolution: ContentHash::ZERO,
            capabilities: BTreeSet::new(),
            profile: ProfileTag::Dist,
            rustflags: Vec::new(),
            panic: PanicTag::Abort,
            crate_type: CrateTypeTag::Cdylib,
            compiler_service_schema: 1,
        }
    }

    /// Serialize to the length-delimited binary form defined in
    /// `phase2-cached-compiler-design.md` §2.2.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        // 4 bytes BE: schema_version
        out.extend_from_slice(&self.schema_version.to_be_bytes());
        // 4 bytes BE: total payload length (filled in at the end)
        let payload_len_pos = out.len();
        out.extend_from_slice(&0u32.to_be_bytes());
        let payload_start = out.len();

        // canonical_identity: 4 bytes BE root + 4 bytes BE path length + N bytes path
        let root_byte = match self.canonical_identity.root() {
            renzora_identity::RootKind::Engine => 0u32,
            renzora_identity::RootKind::Project => 1u32,
        };
        out.extend_from_slice(&root_byte.to_be_bytes());
        let path_bytes = self.canonical_identity.path().as_bytes();
        out.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(path_bytes);

        // source_content
        out.extend_from_slice(&(self.source_content.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.source_content);

        // target_triple
        let bytes = self.target_triple.as_bytes();
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);

        // toolchain_stamp
        let bytes = self.toolchain_stamp.as_bytes();
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);

        // sdk_content_hash: 32 bytes
        out.extend_from_slice(&self.sdk_content_hash.0);

        // abi.version + abi.interface_prefix_hashes
        out.extend_from_slice(&self.abi.version.to_be_bytes());
        out.extend_from_slice(&(self.abi.interface_prefix_hashes.len() as u32).to_be_bytes());
        for h in &self.abi.interface_prefix_hashes {
            out.extend_from_slice(&h.to_le_bytes());
        }

        // wrapper_schema
        out.extend_from_slice(&self.wrapper_schema.to_be_bytes());
        // wrapper_content_hash
        out.extend_from_slice(&self.wrapper_content_hash.0);
        // manifest_schema
        out.extend_from_slice(&self.manifest_schema.to_be_bytes());
        // manifest_content_hash
        out.extend_from_slice(&self.manifest_content_hash.0);

        // lock_resolution: 32 bytes
        out.extend_from_slice(&self.lock_resolution.0);

        // capabilities: 4 bytes BE count, then 4 bytes length + N bytes per entry
        out.extend_from_slice(&(self.capabilities.len() as u32).to_be_bytes());
        for cap in &self.capabilities {
            let bytes = cap.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
        }

        // profile
        let bytes = self.profile.as_str().as_bytes();
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);

        // rustflags: 4 bytes BE count, then 4 bytes length + N bytes per flag
        out.extend_from_slice(&(self.rustflags.len() as u32).to_be_bytes());
        for flag in &self.rustflags {
            let bytes = flag.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
        }

        // panic (1 byte), crate_type (1 byte)
        out.push(self.panic.byte());
        out.push(self.crate_type.byte());

        // compiler_service_schema
        out.extend_from_slice(&self.compiler_service_schema.to_be_bytes());

        // Fill in total payload length.
        let payload_len = (out.len() - payload_start) as u32;
        out[payload_len_pos..payload_len_pos + 4].copy_from_slice(&payload_len.to_be_bytes());

        out
    }

    /// Parse a length-delimited serialized fingerprint.
    pub fn deserialize(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < 8 {
            return Err("fingerprint too short".to_string());
        }
        let schema_version = u32::from_be_bytes(bytes[0..4].try_into().map_err(|_| "bad schema_version")?);
        let total_len = u32::from_be_bytes(bytes[4..8].try_into().map_err(|_| "bad total_len")?) as usize;
        if total_len + 8 != bytes.len() {
            return Err(format!(
                "fingerprint total_len {total_len} + header 8 != {} bytes available",
                bytes.len()
            ));
        }
        let payload = &bytes[8..];
        let mut r = Reader::new(payload);

        let root_byte = r.read_u32_be()?;
        let root = match root_byte {
            0 => renzora_identity::RootKind::Engine,
            1 => renzora_identity::RootKind::Project,
            _ => return Err(format!("unknown root byte {root_byte}")),
        };
        let path = r.read_bytes()?;
        let canonical_identity = CanonicalId::from_rooted(root, std::str::from_utf8(path).map_err(|e| format!("invalid utf8 in canonical identity path: {e}"))?)
            .map_err(|e| format!("invalid canonical identity in fingerprint: {e}"))?;

        let source_content = r.read_bytes()?.to_vec();
        let target_triple = String::from_utf8(r.read_bytes()?.to_vec()).map_err(|e| format!("invalid utf8 in target_triple: {e}"))?;
        let toolchain_stamp = String::from_utf8(r.read_bytes()?.to_vec()).map_err(|e| format!("invalid utf8 in toolchain_stamp: {e}"))?;
        let sdk_content_hash = ContentHash(r.read_array32()?);

        let abi_version = r.read_u32_be()?;
        let n_hashes = r.read_u32_be()? as usize;
        let mut interface_prefix_hashes = Vec::with_capacity(n_hashes);
        for _ in 0..n_hashes {
            interface_prefix_hashes.push(r.read_u32_le()?);
        }

        let wrapper_schema = r.read_u32_be()?;
        let wrapper_content_hash = ContentHash(r.read_array32()?);
        let manifest_schema = r.read_u32_be()?;
        let manifest_content_hash = ContentHash(r.read_array32()?);
        let lock_resolution = ContentHash(r.read_array32()?);

        let n_caps = r.read_u32_be()? as usize;
        let mut capabilities = BTreeSet::new();
        for _ in 0..n_caps {
            capabilities.insert(String::from_utf8(r.read_bytes()?.to_vec()).map_err(|e| format!("invalid utf8 in capability: {e}"))?);
        }

        let profile = String::from_utf8(r.read_bytes()?.to_vec()).map_err(|e| format!("invalid utf8 in profile: {e}"))?;
        let profile = match profile.as_str() {
            "dist" => ProfileTag::Dist,
            "dist-lean" => ProfileTag::DistLean,
            other => return Err(format!("unknown profile tag {other}")),
        };

        let n_flags = r.read_u32_be()? as usize;
        let mut rustflags = Vec::with_capacity(n_flags);
        for _ in 0..n_flags {
            rustflags.push(String::from_utf8(r.read_bytes()?.to_vec()).map_err(|e| format!("invalid utf8 in rustflag: {e}"))?);
        }

        let panic = match r.read_u8()? {
            0 => PanicTag::Abort,
            1 => PanicTag::Unwind,
            b => return Err(format!("unknown panic byte {b}")),
        };
        let crate_type = match r.read_u8()? {
            0 => CrateTypeTag::Cdylib,
            1 => CrateTypeTag::Staticlib,
            2 => CrateTypeTag::Dylib,
            b => return Err(format!("unknown crate_type byte {b}")),
        };
        let compiler_service_schema = r.read_u32_be()?;

        Ok(Self {
            schema_version,
            canonical_identity,
            source_content,
            target_triple,
            toolchain_stamp,
            sdk_content_hash,
            abi: AbiStamp {
                version: abi_version,
                interface_prefix_hashes,
            },
            wrapper_schema,
            wrapper_content_hash,
            manifest_schema,
            manifest_content_hash,
            lock_resolution,
            capabilities,
            profile,
            rustflags,
            panic,
            crate_type,
            compiler_service_schema,
        })
    }

    /// `Blake3(serialized_fingerprint)` — full 32 bytes / 64 hex chars / 256
    /// bits, no truncation.
    pub fn build_key(&self) -> BuildKey {
        let bytes = self.serialize();
        let out = blake3::hash(&bytes);
        let mut arr = [0u8; 32];
        arr.copy_from_slice(out.as_bytes());
        BuildKey(arr)
    }

    /// Verify that two fingerprints serialize to identical bytes (per §2.3).
    pub fn bytes_equal(a: &BuildFingerprint, b: &BuildFingerprint) -> bool {
        a.serialize() == b.serialize()
    }
}

/// Tiny cursor over a byte slice for `BuildFingerprint::deserialize`.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
    fn need(&self, n: usize) -> Result<(), String> {
        if self.remaining() < n {
            Err(format!(
                "fingerprint payload truncated at {}: need {n}, have {}",
                self.pos,
                self.remaining()
            ))
        } else {
            Ok(())
        }
    }
    fn read_u8(&mut self) -> Result<u8, String> {
        self.need(1)?;
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }
    fn read_u32_be(&mut self) -> Result<u32, String> {
        self.need(4)?;
        let b: [u8; 4] = self.buf[self.pos..self.pos + 4].try_into().map_err(|_| "bad u32")?;
        self.pos += 4;
        Ok(u32::from_be_bytes(b))
    }
    fn read_u32_le(&mut self) -> Result<u32, String> {
        self.need(4)?;
        let b: [u8; 4] = self.buf[self.pos..self.pos + 4].try_into().map_err(|_| "bad u32")?;
        self.pos += 4;
        Ok(u32::from_le_bytes(b))
    }
    fn read_array32(&mut self) -> Result<[u8; 32], String> {
        self.need(32)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(&self.buf[self.pos..self.pos + 32]);
        self.pos += 32;
        Ok(out)
    }
    fn read_bytes(&mut self) -> Result<&'a [u8], String> {
        let len = self.read_u32_be()? as usize;
        self.need(len)?;
        let slice = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(slice)
    }
}
