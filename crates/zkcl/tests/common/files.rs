use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct TempDir(PathBuf);

impl TempDir {
    #[track_caller]
    pub fn new(case: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        let pid = std::process::id();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("zkcl-{case}-{pid}-{timestamp}-{sequence}"));
        if let Err(error) = fs::create_dir(&path) {
            panic!("create test directory {}: {error}", path.display());
        }
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    #[track_caller]
    pub fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path().join(name);
        if let Err(error) = fs::write(&path, contents) {
            panic!("write test file {}: {error}", path.display());
        }
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.path());
    }
}
