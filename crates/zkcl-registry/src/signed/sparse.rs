use super::{Result, SignedModuleIndex, invalid, validate_identity};
use crate::{Digest, file_registry::validate_module};
use serde::{Deserialize, Serialize};
use tough::schema::Target;

/// Relative names are resolved against the configured TUF targets endpoint.
pub const SPARSE_V1_MARKER_TARGET: &str = "v1/registry.json";
pub const SPARSE_INDEX_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const SPARSE_SHARD_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const SPARSE_INDEX_MAX_RELEASES: usize = 16_384;
pub const SPARSE_SHARD_MAX_MODULES: usize = 16_384;
pub(super) const MARKER_MAX_BYTES: u64 = 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Marker {
    pub version: u64,
    pub identity: String,
}

/// The top-level TUF target that authenticates the protocol and registry identity.
pub fn sparse_marker_bytes(identity: &str) -> Result<Vec<u8>> {
    validate_identity(identity)?;
    let bytes = serde_json::to_vec(&Marker {
        version: 1,
        identity: identity.to_owned(),
    })?;
    if bytes.len() as u64 > MARKER_MAX_BYTES {
        return Err(invalid("signed registry marker byte limit exceeded"));
    }
    Ok(bytes)
}

/// Authenticated shard directory. Entries are ordered by canonical module name.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SparseShard {
    pub modules: Vec<SparseModule>,
}

/// Immutable module index reference carried in an authenticated shard.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SparseModule {
    pub module: String,
    pub length: u64,
    pub sha256: String,
}

/// First byte of SHA-256 of the canonical UTF-8 module name, in lowercase hex.
pub fn sparse_shard_prefix(module: &str) -> String {
    format!("{:02x}", Digest::of_bytes(module.as_bytes()).as_bytes()[0])
}

pub fn sparse_shard_target(prefix: &str) -> String {
    format!("v1/shards/{prefix}.json")
}

pub fn sparse_index_path(digest: Digest) -> String {
    format!("v1/index/{digest}.json")
}

pub fn sparse_source_path(digest: Digest) -> String {
    format!("sources/{digest}.tar")
}

pub(super) fn shard_target_prefix(name: &str) -> Option<&str> {
    let prefix = name.strip_prefix("v1/shards/")?.strip_suffix(".json")?;
    (prefix.len() == 2
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(prefix)
}

impl SignedModuleIndex {
    /// Encode a validated index with release records in version-key order.
    pub fn to_sparse_bytes(&self) -> Result<Vec<u8>> {
        validate_module(&self.module)?;
        self.validate()?;
        let mut index = self.clone();
        index
            .releases
            .sort_unstable_by(|left, right| left.version.cmp(&right.version));
        let bytes = serde_json::to_vec(&index)?;
        if bytes.len() as u64 > SPARSE_INDEX_MAX_BYTES {
            return Err(invalid("signed index byte limit exceeded"));
        }
        Ok(bytes)
    }
}

impl SparseShard {
    /// Encode validated entries in module name order.
    pub fn to_bytes(&self, prefix: &str) -> Result<Vec<u8>> {
        let mut shard = self.clone();
        shard
            .modules
            .sort_unstable_by(|left, right| left.module.cmp(&right.module));
        shard.find(prefix, None)?;
        let bytes = serde_json::to_vec(&shard)?;
        if bytes.len() as u64 > SPARSE_SHARD_MAX_BYTES {
            return Err(invalid("signed shard byte limit exceeded"));
        }
        Ok(bytes)
    }

    pub(super) fn find(
        &self,
        prefix: &str,
        module: Option<&str>,
    ) -> Result<Option<ObjectReference>> {
        if self.modules.is_empty() || self.modules.len() > SPARSE_SHARD_MAX_MODULES {
            return Err(invalid("invalid signed shard module count"));
        }
        let mut previous = "";
        let mut requested = None;
        for entry in &self.modules {
            validate_module(&entry.module)?;
            if entry.module.as_str() <= previous || sparse_shard_prefix(&entry.module) != prefix {
                return Err(invalid("invalid module order or placement in signed shard"));
            }
            let object =
                ObjectReference::new(entry.sha256.parse()?, entry.length, SPARSE_INDEX_MAX_BYTES)?;
            if module == Some(entry.module.as_str()) {
                requested = Some(object);
            }
            previous = &entry.module;
        }
        Ok(requested)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ObjectReference {
    pub digest: Digest,
    pub length: u64,
}

impl ObjectReference {
    pub fn new(digest: Digest, length: u64, max_bytes: u64) -> Result<Self> {
        if length == 0 || length > max_bytes {
            return Err(invalid("invalid signed object length"));
        }
        Ok(Self { digest, length })
    }

    pub fn from_target(target: &Target, max_bytes: u64) -> Result<Self> {
        Self::new(
            Digest::from_bytes(
                target
                    .hashes
                    .sha256
                    .as_ref()
                    .try_into()
                    .map_err(|_| invalid("invalid signed target SHA-256 length"))?,
            ),
            target.length,
            max_bytes,
        )
    }

    pub fn verify(self, bytes: &[u8]) -> Result<()> {
        if bytes.len() as u64 != self.length {
            return Err(invalid("signed object length mismatch"));
        }
        if Digest::of_bytes(bytes) != self.digest {
            return Err(invalid("signed object SHA-256 mismatch"));
        }
        Ok(())
    }
}
