use std::borrow::Cow;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use cranelift_codegen::incremental_cache::CacheKvStore;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
pub(crate) struct FunctionCache<'a> {
    directory: &'a Path,
}

impl<'a> FunctionCache<'a> {
    pub(crate) fn open(directory: &'a Path) -> Option<Self> {
        fs::create_dir_all(directory).ok()?;
        Some(Self { directory })
    }

    fn path(&self, key: &[u8]) -> PathBuf {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = String::with_capacity(2 * key.len());
        for &byte in key {
            name.push(char::from(HEX[usize::from(byte >> 4)]));
            name.push(char::from(HEX[usize::from(byte & 15)]));
        }
        self.directory.join(name)
    }
}

fn checksum(key: &[u8], payload: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(key)
        .chain_update(payload)
        .finalize()
        .into()
}

impl CacheKvStore for FunctionCache<'_> {
    fn get(&self, key: &[u8]) -> Option<Cow<'_, [u8]>> {
        let mut record = fs::read(self.path(key)).ok()?;
        let (digest, payload) = record.split_at_checked(32)?;
        if digest != checksum(key, payload) {
            return None;
        }
        record.drain(..32);
        Some(Cow::Owned(record))
    }

    fn insert(&mut self, key: &[u8], payload: Vec<u8>) {
        let Ok(mut staged) = tempfile::NamedTempFile::new_in(self.directory) else {
            return;
        };
        if staged.write_all(&checksum(key, &payload)).is_ok() && staged.write_all(&payload).is_ok()
        {
            // Concurrent producers may compile the same stencil. Each rename
            // publishes a complete, verified record for the same key.
            let _ = staged.persist(self.path(key));
        }
    }
}
