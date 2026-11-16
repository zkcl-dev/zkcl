use std::env;
use std::error::Error;
use std::path::PathBuf;

use zkcl_registry::ContentStore;

pub fn cache_root() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = env::var_os("ZKCL_CACHE_DIR").map(PathBuf::from) {
        if !path.is_absolute() {
            return Err("ZKCL_CACHE_DIR must be an absolute path".into());
        }
        return Ok(path);
    }

    #[cfg(windows)]
    let base = absolute_env("LOCALAPPDATA")
        .or_else(|| absolute_env("USERPROFILE").map(|home| home.join("AppData/Local")));
    #[cfg(not(windows))]
    let base = absolute_env("XDG_CACHE_HOME")
        .or_else(|| absolute_env("HOME").map(|home| home.join(".cache")));

    base.map(|path| path.join("zkcl"))
        .ok_or_else(|| "could not find the user cache directory, set ZKCL_CACHE_DIR".into())
}

pub fn source_store() -> Result<ContentStore, Box<dyn Error>> {
    Ok(ContentStore::new(cache_root()?.join("store")))
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}
