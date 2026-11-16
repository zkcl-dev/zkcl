//! Process-wide FASL memoization for bundled module sources.
//!
//! Bundled modules are `include_str!` data. The cache retains a native artifact
//! per source and FFI capability for runtimes whose reader features match the compiler's.
//! Nothing is persisted; the cache dies with the process.
//!
//! A separate runtime prepares the packages needed by the compiler and retains
//! its compile-time effects. A candidate loading the FASL runs its load entries.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::mem::DropGuard;
use std::sync::{
    Arc, Mutex, MutexGuard, OnceLock,
    atomic::{AtomicU64, Ordering},
};

use rustc_hash::FxHashMap;
use zkcl_bytecode::CompiledFile;

use crate::heap::{ListSpine, Word};
use crate::loader::{self, LoadFile};
use crate::package::Package;
use crate::{EvalError, Runtime, RuntimeBuilder};

type CacheKey = (usize, bool);

struct CompiledModule {
    features: Vec<String>,
    bytes: Vec<u8>,
}

/// Compiled modules keyed by the source's address and enabled host capability.
///
/// Bundled sources are `&'static str` constants, so the address identifies one for the
/// whole process; two module names sharing a source share one entry.
static COMPILED: OnceLock<Mutex<FxHashMap<CacheKey, Arc<CompiledModule>>>> = OnceLock::new();

fn cache() -> MutexGuard<'static, FxHashMap<CacheKey, Arc<CompiledModule>>> {
    COMPILED
        .get_or_init(|| Mutex::new(FxHashMap::default()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cache_key(source: &'static str, foreign_calls: bool) -> CacheKey {
    (source.as_ptr() as usize, foreign_calls)
}

/// The cached FASL for `source`, if this process compiled it already.
pub(super) fn fasl(runtime: &Runtime, source: &'static str) -> Option<LoadFile> {
    let module = cache()
        .get(&cache_key(source, runtime.foreign_calls))
        .cloned()?;
    if !matches_features(runtime, &module.features) {
        return None;
    }
    // This process wrote these bytes for this binary, so decoding is expected to succeed;
    // a failure falls back to the source instead of failing REQUIRE.
    let contents = loader::FileContents::compiled(runtime, &module.bytes).ok()?;
    Some(LoadFile::embedded(contents))
}

fn matches_features(runtime: &Runtime, expected: &[String]) -> bool {
    let Some(features) = runtime
        .find_common_lisp("*FEATURES*")
        .and_then(|symbol| runtime.heap.symbol_cell(symbol, false))
    else {
        return false;
    };
    let packages = runtime.packages.borrow();
    let mut actual = ListSpine::new(features);
    for name in expected {
        let Some((keyword, _)) = packages.present(Package::KEYWORD, name) else {
            return false;
        };
        if !matches!(actual.next(&runtime.heap), Ok(Some((word, _))) if word == keyword)
            || runtime.heap.symbol_home(keyword) != Some(Package::KEYWORD)
        {
            return false;
        }
    }
    actual.tail() == Word::NIL
}

/// Compile `source` for the caller's FFI capability unless it is already cached.
pub(super) fn warm(source: &'static str, name: &str, foreign_calls: bool) -> Result<(), EvalError> {
    let key = cache_key(source, foreign_calls);
    if cache().contains_key(&key) {
        return Ok(());
    }
    let module = compile(source, name, foreign_calls)?;
    cache().insert(key, Arc::new(module));
    Ok(())
}

/// Compile `source` in a throwaway image and remember its input features.
fn compile(
    source: &'static str,
    name: &str,
    foreign_calls: bool,
) -> Result<CompiledModule, EvalError> {
    let builder = RuntimeBuilder {
        foreign_calls,
        ..RuntimeBuilder::new()
    };
    let image = builder.start().map_err(|error| {
        EvalError::new(
            0,
            format!("bundled module compiler image failed to start: {error:?}"),
        )
    })?;
    image.initialize_packages()?;
    let features_symbol = image
        .find_common_lisp("*FEATURES*")
        .expect("bootstrap feature variable");
    let features_value = image
        .heap
        .symbol_cell(features_symbol, false)
        .expect("bootstrap features are bound");
    let features = crate::vm::proper_list(&image, features_value, 0)?
        .into_iter()
        .map(|word| image.heap.symbol_name(word).expect("bootstrap feature name"))
        .collect();
    // Some later forms need load-time package effects while being read, such as
    // the export of ZK:*USE-ASDF-PORTS*. Use the bundled loader's dynamic reader
    // bindings for this pass so its final IN-PACKAGE stays local to the load.
    image.heap.set_symbol_cell(
        image.package_variable(),
        false,
        Package::COMMON_LISP.word(),
    );
    // The compilation starts with the original features. ASDF itself adds
    // feature keywords during its initialization.
    {
        let features = image.root_slot(features_value);
        let symbol = image.root_slot(features_symbol);
        let _execution = image.enter_host_execution()?;
        crate::vm::with_special_bindings(&image, [(symbol.get(), features.get())], || {
            crate::vm::load_host(&image, LoadFile::embedded_source(source))
        })?;
    }
    let bytes = compile_source_file(&image, source, name)?
        .encode()
        .map_err(|error| EvalError::from_error(0, error))?;
    Ok(CompiledModule { features, bytes })
}

fn compile_source_file(
    image: &Runtime,
    source: &str,
    name: &str,
) -> Result<CompiledFile, EvalError> {
    static NEXT_SOURCE: AtomicU64 = AtomicU64::new(0);
    let directory = std::env::temp_dir();
    let process = std::process::id();
    let name = file_component(name);
    let (path, file) = loop {
        let id = NEXT_SOURCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("zkcl-bundled-{process}-{id}-{name}.lisp"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(EvalError::file_io(0, &path, error)),
        }
    };
    let path = DropGuard::new(path, |path| {
        let _ = fs::remove_file(path);
    });
    // Each compiler keeps its input until it has finished reading it.
    let mut file = file;
    file.write_all(source.as_bytes())
        .map_err(|error| EvalError::file_io(0, &path, error))?;
    drop(file);
    crate::fasl::compile_host_bytecode(image, &path)
}

/// Keep a module name from escaping the temporary directory.
fn file_component(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `1`/`0` per observable definition, evaluated one form at a time: `eval` returns the
    /// values of the last form only, so a single `(list …)` would compare one list.
    const ZK_TEST_PROBES: [&str; 5] = [
        "(if (find-package \"ZK-TEST\") 1 0)",
        "(if (member \"ZK-TEST\" *modules* :test #'string=) 1 0)",
        "(if (fboundp 'zk-test:run) 1 0)",
        "(if (fboundp 'zk-test:check) 1 0)",
        "(if (macro-function 'zk-test:deftest) 1 0)",
    ];

    /// ASDF is where a dropped `:compile-toplevel` effect would actually bite.
    const ASDF_PROBES: [&str; 5] = [
        "(if (find-package \"ASDF\") 1 0)",
        "(if (member \"ASDF\" *modules* :test #'string=) 1 0)",
        "(if (fboundp 'asdf:load-system) 1 0)",
        "(if (fboundp 'asdf:asdf-version) 1 0)",
        "(if (macro-function 'asdf:defsystem) 1 0)",
    ];

    fn observable(runtime: &Runtime, probes: &[&str]) -> Vec<Option<isize>> {
        probes
            .iter()
            .map(|probe| {
                runtime
                    .eval(probe)
                    .expect("probe evaluates")
                    .pop()
                    .and_then(|value| value.as_fixnum())
            })
            .collect()
    }

    /// Compare source, cache, and source fallback in separate runtimes.
    fn compare(module: &str, probes: &[&str]) {
        let require = format!("(require \"{module}\")");
        let source = super::super::bundled_source(module).expect("bundled source");
        // The source path must run before warming, or the cache answers for it.
        let source_state = {
            let runtime = Runtime::start().expect("source runtime");
            runtime.load_str(&require).expect("source require");
            observable(&runtime, probes)
        };
        {
            let runtime = Runtime::start().expect("warming runtime");
            runtime
                .warm_bundled_module(module)
                .expect("the module warms");
        }
        let cache_state = {
            let runtime = Runtime::start().expect("cached runtime");
            runtime
                .initialize_packages()
                .expect("cached runtime packages");
            assert!(fasl(&runtime, source).is_some(), "pristine cache hit");
            runtime.load_str(&require).expect("cached require");
            observable(&runtime, probes)
        };
        assert_eq!(
            source_state,
            vec![Some(1); probes.len()],
            "loading {module} from source must define everything the probes look for"
        );
        assert_eq!(
            source_state, cache_state,
            "loading {module} from the cache must leave the source load's state"
        );
        let runtime = Runtime::start().expect("runtime with additional feature");
        runtime
            .load_str("(push :zkcl-bundled-cache-feature *features*)")
            .expect("custom feature");
        assert!(
            fasl(&runtime, source).is_none(),
            "changed features bypass cache"
        );
        runtime.load_str(&require).expect("source fallback");
        assert_eq!(source_state, observable(&runtime, probes));
    }

    #[test]
    fn a_warmed_module_loads_the_same_state_as_its_source() {
        compare("zk-test", &ZK_TEST_PROBES);
    }

    /// Compiling ASDF in a debug build takes minutes, so this one is a release probe.
    #[test]
    #[ignore = "compiles 700 KB of ASDF; run with --release --ignored --nocapture"]
    fn a_warmed_asdf_loads_the_same_state_as_its_source() {
        compare("asdf", &ASDF_PROBES);
    }
}
