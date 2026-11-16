use super::*;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use zkcl_aot::{
    AotBackendKind, AotOptLevel, AotTargetConfig, compile_fasl_bytes_to_object_with_function_cache,
};
use zkcl_registry::Digest;

#[derive(Debug)]
pub struct AotUnit {
    pub module_id: String,
    pub entries: Vec<zkcl_aot::AotRegistrationEntry>,
    pub fasl_length: usize,
    pub fasl: PathBuf,
    pub object: PathBuf,
    pub registration: PathBuf,
}

/// Emit one object, FASL and Rust registration module per compiled component.
///
/// The configuration's module ID identifies this component group and must be
/// unique after `zkcl_aot::sanitize_module_id` within the linked program. Each
/// file receives its own suffix and retains its original code and literal
/// indices. Registration sources must be included as separate Rust modules
/// because each defines `AOT_MODULE`.
///
/// Register each descriptor, then load the delivered FASLs in dependency order using
/// `Runtime::load_file` to retain file pathname bindings. `load_aot_module` loads
/// from memory and therefore binds both load pathnames to NIL.
///
/// Output paths are flat and reusable. `.zkcl-aot.lock` serializes concurrent
/// calls during preflight and per-file replacement. Each call replaces only
/// the files it emits. Callers that need to publish a consistent batch should
/// use a private output directory and publish it after emission succeeds.
pub fn emit_aot_units(
    units: &[FaslUnit],
    config: &AotTargetConfig,
    output_dir: &Path,
) -> Result<Vec<AotUnit>, DeliveryError> {
    fs::create_dir_all(output_dir)
        .map_err(|source| DeliveryError::io("create", output_dir, source))?;
    let output_dir = output_dir
        .canonicalize()
        .map_err(|source| DeliveryError::io("resolve", output_dir, source))?;
    let cache = crate::project::cache::cache_root()
        .map_err(|error| {
            DeliveryError::io(
                "locate AOT cache",
                output_dir.as_path(),
                io::Error::other(error.to_string()),
            )
        })?
        .join("aot");
    fs::create_dir_all(&cache).map_err(|source| DeliveryError::io("create", &cache, source))?;
    let executable = std::env::current_exe()
        .map_err(|source| DeliveryError::io("find executable", &cache, source))?;
    let toolchain = Digest::of_reader(
        fs::File::open(&executable)
            .map_err(|source| DeliveryError::io("open", &executable, source))?,
    )
    .map_err(|source| DeliveryError::io("hash", &executable, source))?;
    let staging = tempfile::Builder::new()
        .prefix(".aot-components-")
        .tempdir_in(&output_dir)
        .map_err(|error| DeliveryError::io("stage AOT components", &output_dir, error))?;
    let prepare = |index, unit: &FaslUnit, jobs| {
        let mut config = config.clone();
        config.module_id = format!("{}-component-{index}", config.module_id);
        config.jobs = Some(jobs);
        stage_component(index, unit, &config, staging.path(), &cache, toolchain)
    };
    let limit = config.worker_limit();
    let jobs = limit.get();
    let workers = jobs.min(units.len());
    let prepared = if workers <= 1 {
        units
            .iter()
            .enumerate()
            .map(|(index, unit)| prepare(index, unit, limit))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let mut results: Vec<_> = (0..units.len()).map(|_| None).collect();
        let queue = Mutex::new(units.iter().zip(&mut results).enumerate());
        let first_error = AtomicUsize::new(units.len());
        // Each share includes the caller and nested backend jobs. Together
        // they stay within the invocation's limit.
        let work = |worker| {
            let share = jobs / workers + usize::from(worker < jobs % workers);
            let limit = NonZeroUsize::new(share).unwrap();
            let tasks = std::iter::from_fn(|| queue.lock().unwrap().next());
            for (index, (unit, result)) in tasks {
                if index > first_error.load(Ordering::Relaxed) {
                    break;
                }
                let prepared = prepare(index, unit, limit);
                if prepared.is_err() {
                    // Earlier indices have all been claimed. Let them finish
                    // so that the reported error matches serial compilation.
                    first_error.fetch_min(index, Ordering::Relaxed);
                }
                *result = Some(prepared);
            }
        };
        thread::scope(|scope| -> io::Result<()> {
            for worker in 1..workers {
                thread::Builder::new().spawn_scoped(scope, move || work(worker))?;
            }
            work(0);
            Ok(())
        })
        .map_err(|error| DeliveryError::io("start AOT workers", &output_dir, error))?;
        results
            .into_iter()
            .map(|result| result.expect("each component has a result"))
            .collect::<Result<Vec<_>, _>>()?
    };

    // Backend work can finish before taking the destination lock.
    let lock_path = output_dir.join(".zkcl-aot.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|error| DeliveryError::io("open", &lock_path, error))?;
    lock.lock()
        .map_err(|error| DeliveryError::io("lock", &lock_path, error))?;

    // Check all paths first so a directory or other non-file destination cannot
    // fail partway through replacing the batch.
    let staged = fs::read_dir(staging.path())
        .map_err(|error| DeliveryError::io("list staged AOT components", staging.path(), error))?;
    for entry in staged {
        let entry = entry.map_err(|error| {
            DeliveryError::io("list staged AOT components", staging.path(), error)
        })?;
        let destination = output_dir.join(entry.file_name());
        let error = match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.is_file() => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Ok(_) => io::Error::new(io::ErrorKind::InvalidInput, "expected a regular file"),
            Err(error) => error,
        };
        return Err(DeliveryError::io(
            "check AOT destination",
            &destination,
            error,
        ));
    }

    let mut emitted = Vec::with_capacity(prepared.len());
    for (index, mut unit) in prepared.into_iter().enumerate() {
        for path in [&mut unit.object, &mut unit.fasl, &mut unit.registration] {
            *path = publish_component_file(path, &output_dir)?;
        }
        let ir = staging.path().join(format!("{index:04}.ll"));
        if ir.exists() {
            publish_component_file(&ir, &output_dir)?;
        }
        emitted.push(unit);
    }
    Ok(emitted)
}

fn stage_component(
    index: usize,
    unit: &FaslUnit,
    config: &AotTargetConfig,
    staging: &Path,
    cache: &Path,
    toolchain: Digest,
) -> Result<AotUnit, DeliveryError> {
    let bytes =
        fs::read(&unit.fasl).map_err(|source| DeliveryError::io("read", &unit.fasl, source))?;
    let compiled = cached_aot(cache, toolchain, bytes, config, &unit.source)?;
    let object = staging.join(format!("{index:04}.o"));
    let fasl = staging.join(format!("{index:04}.zkclfasl"));
    let registration = staging.join(format!("component_{index:04}.rs"));
    let fasl_length = compiled.fasl_bytes.len();
    let entries = compiled.entries.into_iter().map(Into::into).collect();
    write_file(&object, compiled.object_bytes)?;
    write_file(&fasl, compiled.fasl_bytes)?;
    write_file(&registration, compiled.registration_source)?;
    if let Some(ir) = compiled.llvm_ir {
        write_file(&staging.join(format!("{index:04}.ll")), ir)?;
    }
    Ok(AotUnit {
        module_id: config.module_id.clone(),
        entries,
        fasl_length,
        fasl,
        object,
        registration,
    })
}

fn publish_component_file(staged: &Path, directory: &Path) -> Result<PathBuf, DeliveryError> {
    let destination = directory.join(staged.file_name().expect("staged component filename"));
    if let Ok(before) = fs::read(&destination) {
        let after = fs::read(staged).map_err(|error| DeliveryError::io("read", staged, error))?;
        if before == after {
            return Ok(destination);
        }
    }
    fs::rename(staged, &destination)
        .map_err(|error| DeliveryError::io("publish AOT component", &destination, error))?;
    Ok(destination)
}

// Cache only offline object emission: compile_files above must replay Lisp effects.
#[derive(Serialize, Deserialize)]
struct CachedEntry {
    entry_index: u32,
    native_debug: bool,
    symbol_name: String,
    arity: zkcl_bytecode::Arity,
    root_slots: usize,
    leaf_symbol: Option<String>,
    leaf_guards: Vec<zkcl_bytecode::native_abi::LeafGuard>,
}

impl From<zkcl_aot::AotRegistrationEntry> for CachedEntry {
    fn from(entry: zkcl_aot::AotRegistrationEntry) -> Self {
        Self {
            entry_index: entry.entry_index,
            native_debug: entry.native_debug,
            symbol_name: entry.symbol_name,
            arity: entry.arity,
            root_slots: entry.root_slots,
            leaf_symbol: entry.leaf_symbol,
            leaf_guards: entry.leaf_guards,
        }
    }
}
impl From<CachedEntry> for zkcl_aot::AotRegistrationEntry {
    fn from(entry: CachedEntry) -> Self {
        Self {
            entry_index: entry.entry_index,
            native_debug: entry.native_debug,
            symbol_name: entry.symbol_name,
            arity: entry.arity,
            root_slots: entry.root_slots,
            leaf_symbol: entry.leaf_symbol,
            leaf_guards: entry.leaf_guards,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct CachedAot {
    key: String,
    object_bytes: Vec<u8>,
    fasl_bytes: Vec<u8>,
    registration_source: String,
    llvm_ir: Option<String>,
    entries: Vec<CachedEntry>,
}

fn aot_key(
    toolchain: Digest,
    bytes: &[u8],
    config: &AotTargetConfig,
) -> Result<String, zkcl_aot::AotError> {
    // Worker count only schedules function emission. Object order and the
    // symbol and registration indices are fixed by the compiled file.
    let backend = match config.backend {
        AotBackendKind::Direct => "direct",
        AotBackendKind::CraneliftObject => "cranelift",
        AotBackendKind::Llvm => "llvm",
    };
    let optimization = match config.opt_level {
        AotOptLevel::None => "none",
        AotOptLevel::Speed => "speed",
        AotOptLevel::Aggressive => "aggressive",
        AotOptLevel::Size => "size",
    };
    let identity = serde_json::to_vec(&(
        "zkcl-aot-object",
        toolchain.as_bytes(),
        Digest::of_bytes(bytes).as_bytes(),
        &config.module_id,
        config.target.to_string(),
        backend,
        optimization,
        config.codegen_target_identity()?,
    ))
    .expect("AOT key fields serialize as JSON values");
    Ok(Digest::of_bytes(&identity).to_string())
}

fn read_cached_aot(directory: &Path, key: &str, fasl: &[u8]) -> Option<CachedAot> {
    let bytes = fs::read(directory.join("artifact.json")).ok()?;
    let digest = fs::read_to_string(directory.join("digest")).ok()?;
    if Digest::of_bytes(&bytes).to_string() != digest {
        return None;
    }
    let artifact: CachedAot = serde_json::from_slice(&bytes).ok()?;
    (artifact.key == key && artifact.fasl_bytes == fasl).then_some(artifact)
}

fn cached_aot(
    cache: &Path,
    toolchain: Digest,
    bytes: Vec<u8>,
    config: &AotTargetConfig,
    source: &Path,
) -> Result<CachedAot, DeliveryError> {
    let aot_error = |error| DeliveryError::Aot {
        path: source.to_owned(),
        source: error,
    };
    let key = aot_key(toolchain, &bytes, config).map_err(aot_error)?;
    let directory = cache.join(&key);
    let lock_path = cache.join(format!("{key}.lock"));
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| DeliveryError::io("open", &lock_path, error))?;
    lock.lock()
        .map_err(|error| DeliveryError::io("lock", &lock_path, error))?;
    if let Some(compiled) = read_cached_aot(&directory, &key, &bytes) {
        return Ok(compiled);
    }
    let function_cache = cache
        .join("cranelift-functions")
        .join(toolchain.to_string());
    let compiled = compile_fasl_bytes_to_object_with_function_cache(bytes, config, &function_cache)
        .map_err(aot_error)?;
    let compiled = CachedAot {
        key,
        object_bytes: compiled.object_bytes,
        fasl_bytes: compiled.fasl_bytes,
        registration_source: compiled.registration_source,
        llvm_ir: compiled.llvm_ir,
        entries: compiled
            .registration_entries
            .into_iter()
            .map(Into::into)
            .collect(),
    };
    let encoded = serde_json::to_vec(&compiled).map_err(|error| {
        DeliveryError::io("encode AOT cache", &directory, io::Error::other(error))
    })?;
    let staging = tempfile::Builder::new()
        .prefix(".aot-")
        .tempdir_in(cache)
        .map_err(|error| DeliveryError::io("stage AOT cache", cache, error))?;
    let digest = Digest::of_bytes(&encoded).to_string();
    for (name, contents) in [
        ("artifact.json", encoded.as_slice()),
        ("digest", digest.as_bytes()),
    ] {
        let path = staging.path().join(name);
        let mut file =
            fs::File::create(&path).map_err(|error| DeliveryError::io("create", &path, error))?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|error| DeliveryError::io("write", &path, error))?;
    }
    if directory.exists() {
        fs::remove_dir_all(&directory)
            .map_err(|error| DeliveryError::io("discard corrupt AOT cache", &directory, error))?;
    }
    fs::rename(staging.path(), &directory)
        .map_err(|error| DeliveryError::io("commit AOT cache", &directory, error))?;
    Ok(compiled)
}

fn write_file(path: &Path, contents: impl AsRef<[u8]>) -> Result<(), DeliveryError> {
    fs::write(path, contents).map_err(|source| DeliveryError::io("write", path, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aot_cache_reuses_verified_objects_and_repairs_corruption() {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/aot-cache");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let source = dir.path().join("test.lisp");
        fs::write(&source, "(defun cache-answer () 42)").unwrap();
        let runtime = Runtime::start().unwrap();
        let fasl = runtime
            .compile_file(&source, Some(&dir.path().join("test.zkclfasl")))
            .unwrap();
        let bytes = fs::read(fasl).unwrap();
        let config =
            AotTargetConfig::host("cache-test").with_backend(zkcl_aot::AotBackendKind::Direct);
        let toolchain = Digest::of_bytes(b"test-toolchain");
        let key = aot_key(toolchain, &bytes, &config).unwrap();
        let first = cached_aot(dir.path(), toolchain, bytes.clone(), &config, &source).unwrap();
        let marker = dir.path().join(&key).join("hit-marker");
        fs::write(&marker, "keep on hit").unwrap();
        let second = cached_aot(dir.path(), toolchain, bytes.clone(), &config, &source).unwrap();
        assert!(marker.exists());
        assert_eq!(first.object_bytes, second.object_bytes);
        assert_eq!(first.registration_source, second.registration_source);
        let entries: Vec<zkcl_aot::AotRegistrationEntry> =
            first.entries.into_iter().map(Into::into).collect();
        let second_entries: Vec<zkcl_aot::AotRegistrationEntry> =
            second.entries.into_iter().map(Into::into).collect();
        assert_eq!(entries, second_entries);
        fs::write(dir.path().join(&key).join("artifact.json"), b"corrupt").unwrap();
        let repaired = cached_aot(dir.path(), toolchain, bytes.clone(), &config, &source).unwrap();
        assert!(!marker.exists());
        assert_eq!(repaired.object_bytes, second.object_bytes);
        let mut other = config.clone();
        other.module_id = "different-module".to_owned();
        let other_key = aot_key(toolchain, &bytes, &other).unwrap();
        assert_ne!(key, other_key);
        cached_aot(dir.path(), toolchain, bytes.clone(), &other, &source).unwrap();
        let artifact = dir.path().join(&key);
        let foreign = dir.path().join(&other_key);
        // A valid record for another module (with its unmodified checksum)
        // cannot masquerade as the requested module's native registrations.
        for name in ["artifact.json", "digest"] {
            fs::copy(foreign.join(name), artifact.join(name)).unwrap();
        }
        let restored = cached_aot(dir.path(), toolchain, bytes.clone(), &config, &source).unwrap();
        assert_eq!(restored.registration_source, repaired.registration_source);
        other = config.clone();
        other.opt_level = zkcl_aot::AotOptLevel::None;
        assert_ne!(key, aot_key(toolchain, &bytes, &other).unwrap());
        other = config.clone();
        other.backend = zkcl_aot::AotBackendKind::Llvm;
        assert_ne!(key, aot_key(toolchain, &bytes, &other).unwrap());
        other = config.clone();
        other.target = "wasm32-unknown-unknown".parse().unwrap();
        assert_ne!(key, aot_key(toolchain, &bytes, &other).unwrap());
        other = config.clone();
        other.jobs = std::num::NonZeroUsize::new(2);
        assert_eq!(key, aot_key(toolchain, &bytes, &other).unwrap());
        assert_ne!(
            key,
            aot_key(Digest::of_bytes(b"new-toolchain"), &bytes, &config).unwrap()
        );
        assert_ne!(key, aot_key(toolchain, b"different-fasl", &config).unwrap());
    }

    #[test]
    fn ordered_files_retain_compile_effects_and_independent_fasls() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.agents/tmp/zkcl/project-delivery")
            .join(format!("ordered-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.lisp");
        let second = dir.join("second.lisp");
        fs::write(
            &first,
            r#"(defpackage #:delivery-test (:use #:cl))
(in-package #:delivery-test)
(eval-when (:compile-toplevel) (setf (get 'answer 'compile-value) 42))
(defmacro answer () (get 'answer 'compile-value 0))
(defparameter *first-load* *load-truename*)"#,
        )
        .unwrap();
        fs::write(
            &second,
            r#"(in-package #:delivery-test)
(defparameter *answer* (answer))
(defparameter *second-load* *load-truename*)"#,
        )
        .unwrap();
        let runtime = Runtime::start().unwrap();
        let mut units = compile_files(&runtime, &[first], &dir.join("first-fasl")).unwrap();
        units.extend(compile_files(&runtime, &[second], &dir.join("second-fasl")).unwrap());
        assert_eq!(units.len(), 2);
        assert_ne!(units[0].fasl, units[1].fasl);
        let answer = runtime.eval("delivery-test::*answer*").unwrap();
        assert_eq!(runtime.repr(&answer[0]).unwrap(), "42");
        let loads = runtime
            .eval("(and delivery-test::*first-load* delivery-test::*second-load* (not (equal delivery-test::*first-load* delivery-test::*second-load*)))")
            .unwrap();
        assert_eq!(runtime.repr(&loads[0]).unwrap(), "T");

        let mut config =
            AotTargetConfig::host("delivery-test").with_backend(zkcl_aot::AotBackendKind::Direct);
        config.jobs = Some(NonZeroUsize::MIN);
        let serial = emit_aot_units(&units, &config, &dir.join("serial")).unwrap();
        config.jobs = NonZeroUsize::new(4);
        let native = emit_aot_units(&units, &config, &dir.join("native")).unwrap();
        assert_eq!(native.len(), 2);
        for (index, (unit, native)) in units.iter().zip(&native).enumerate() {
            assert_eq!(
                fs::read(&unit.fasl).unwrap(),
                fs::read(&native.fasl).unwrap()
            );
            let object = fs::read(&native.object).unwrap();
            assert!(!object.is_empty());
            assert_eq!(object, fs::read(&serial[index].object).unwrap());
            assert_eq!(native.entries, serial[index].entries);
            assert_eq!(
                fs::read(&native.registration).unwrap(),
                fs::read(&serial[index].registration).unwrap()
            );
            assert!(
                fs::read_to_string(&native.registration)
                    .unwrap()
                    .contains(&format!("delivery-test-component-{index}"))
            );
            fs::remove_file(&unit.source).unwrap();
        }
        let bad = dir.join("invalid.zkclfasl");
        fs::write(&bad, b"invalid FASL").unwrap();
        let invalid = [
            FaslUnit {
                source: units[0].source.clone(),
                fasl: units[0].fasl.clone(),
            },
            FaslUnit {
                source: dir.join("invalid.lisp"),
                fasl: bad,
            },
        ];
        let untouched = dir.join("failed-native");
        assert!(emit_aot_units(&invalid, &config, &untouched).is_err());
        assert_eq!(fs::read_dir(&untouched).unwrap().count(), 0);
        let before = fs::read(&native[0].object).unwrap();
        let different_module = AotTargetConfig {
            module_id: "incomplete-delivery".into(),
            ..config
        };
        assert!(emit_aot_units(&invalid, &different_module, &dir.join("native")).is_err());
        assert_eq!(fs::read(&native[0].object).unwrap(), before);

        // Force a conflict on the last registration after an earlier object
        // would have changed. Reject it before replacing that earlier object.
        let first_registration = fs::read(&native[0].registration).unwrap();
        let registration = &native[1].registration;
        fs::remove_file(registration).unwrap();
        fs::create_dir(registration).unwrap();
        let error = emit_aot_units(&units, &different_module, &dir.join("native")).unwrap_err();
        assert!(matches!(error, DeliveryError::Io { path, .. } if path == *registration));
        assert_eq!(fs::read(&native[0].object).unwrap(), before);
        assert_eq!(
            fs::read(&native[0].registration).unwrap(),
            first_registration
        );
        drop(answer);
        drop(loads);
        drop(runtime);

        // A fresh instance consumes the exact per-file products, without source
        // compilation or replaying the build-only property mutation.
        let runtime = Runtime::start().unwrap();
        for unit in &units {
            runtime.load_file(&unit.fasl).unwrap();
        }
        let answer = runtime.eval("delivery-test::*answer*").unwrap();
        assert_eq!(runtime.repr(&answer[0]).unwrap(), "42");
        fs::remove_dir_all(dir).unwrap();
    }
}
