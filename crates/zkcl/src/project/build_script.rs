use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use zkcl_registry::Digest;

use super::workspace::Project;

/// Host configuration for build scripts. By default scripts run in a fresh,
/// in-process runtime. A CLI can explicitly select its zkcl executable instead.
#[derive(Clone, Default)]
pub struct BuildOptions {
    pub program: Option<PathBuf>,
    pub runtime: zkcl_runtime::RuntimeBuilder,
    /// Register capabilities in each fresh build instance before loading code.
    pub initialize_runtime:
        Option<std::rc::Rc<dyn Fn(&zkcl_runtime::Runtime) -> Result<(), Box<dyn Error>>>>,
    pub environment: std::collections::BTreeMap<String, Option<String>>,
}

impl std::fmt::Debug for BuildOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuildOptions")
            .field("program", &self.program)
            .field("runtime", &self.runtime)
            .field("environment", &self.environment)
            .field("custom_initializer", &self.initialize_runtime.is_some())
            .finish()
    }
}

pub fn output_dir(session: &Path) -> PathBuf {
    session.join("generated")
}

pub fn run(
    project: &Project,
    output: &Path,
    target: &str,
    options: &BuildOptions,
) -> Result<(), Box<dyn Error>> {
    run_with_dependencies(project, output, target, None, &[], options)
}

/// Cache generated files only. Lisp compilation/loading still runs for each caller.
/// A supplied bootstrap is cacheable when its logical inputs are represented in
/// `dependency_fingerprint`, which can omit per-invocation staging paths.
pub fn run_with_dependencies(
    project: &Project,
    output: &Path,
    target: &str,
    bootstrap: Option<&Path>,
    dependency_fingerprint: &[u8],
    options: &BuildOptions,
) -> Result<(), Box<dyn Error>> {
    let Some(build) = &project.manifest.build else {
        return Ok(());
    };
    let script = project.source_file(&build.path)?;
    let cacheable = build.inputs.is_some()
        && !build.outputs.is_empty()
        && ((bootstrap.is_none() && build.dependencies.is_empty())
            || !dependency_fingerprint.is_empty())
        // A host initializer can depend on state absent from the declared inputs.
        && (options.program.is_some() || options.initialize_runtime.is_none());
    let cache = super::cache::cache_root()?.join("build-tasks");
    fs::create_dir_all(&cache)?;
    let cache_entry = if cacheable {
        // Only cache tasks with declared inputs. Hash file streams so an
        // executable or input does not have to fit in memory twice.
        let mut identity = Vec::new();
        let mut add = |bytes: &[u8]| {
            identity.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            identity.extend_from_slice(bytes);
        };
        add(b"zkcl-build-task/2");
        add(project.root.as_os_str().as_encoded_bytes());
        add(target.as_bytes());
        add(format!("{:?}", options.runtime).as_bytes());
        add(format!("{:?}", options.environment).as_bytes());
        add(&[u8::from(bootstrap.is_some())]);
        add(dependency_fingerprint);
        let executable = options.program.clone().map_or_else(env::current_exe, Ok)?;
        let mut files = vec![project.manifest_path.clone(), executable, script.clone()];
        for input in build
            .inputs
            .as_ref()
            .expect("cacheable task declares inputs")
        {
            files.push(project.source_file(input)?);
        }
        let snapshots = files
            .into_iter()
            .map(|file| {
                let digest = Digest::of_reader(File::open(&file)?)?;
                add(file.as_os_str().as_encoded_bytes());
                add(digest.as_bytes());
                Ok((file, digest))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let mut environment: Vec<_> = env::vars_os()
            .filter(|(name, _)| name != "ZKCL_OUT_DIR" && name != "ZKCL_BUILD_TARGET")
            .collect();
        environment.sort();
        for (name, value) in environment {
            add(name.as_encoded_bytes());
            add(value.as_encoded_bytes());
        }
        let key = Digest::of_bytes(&identity).to_string();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(cache.join(format!("{key}.lock")))?;
        lock.lock()?;
        let entry = cache.join(&key);
        if valid_entry(&entry, &key, &build.outputs) {
            if snapshots_current(&snapshots) {
                return copy_outputs(&entry.join("files"), output, &build.outputs);
            }
            // Inputs changed while obtaining or inspecting the cache entry.
            // Execute once without publishing under this old key.
            None
        } else {
            Some((entry, key, snapshots, lock))
        }
    } else {
        // A declaration still requires its input files when the task has no
        // cacheable outputs.
        for input in build.inputs.iter().flatten() {
            project.source_file(input)?;
        }
        None
    };
    let stage = tempfile::Builder::new()
        .prefix("task-")
        .tempdir_in(&cache)?;
    let files = stage.path().join("files");
    fs::create_dir(&files)?;
    let mut directory = files.canonicalize()?.into_os_string();
    directory.push(MAIN_SEPARATOR_STR);
    let run_path = if let Some(bootstrap) = bootstrap {
        let wrapper = stage.path().join("run.lisp");
        let literal = |path: &Path| -> Result<String, Box<dyn Error>> {
            let text = path.to_str().ok_or("build path is not UTF-8")?;
            Ok(format!(
                "\"{}\"",
                text.replace('\\', "\\\\").replace('"', "\\\"")
            ))
        };
        fs::write(
            &wrapper,
            format!(
                "(load {})\n(load {})\n",
                literal(bootstrap)?,
                literal(&script)?
            ),
        )?;
        wrapper
    } else {
        script.clone()
    };
    if let Some(program) = &options.program {
        let mut command = Command::new(program);
        for (name, value) in &options.environment {
            match value {
                Some(value) => {
                    command.env(name, value);
                }
                None => {
                    command.env_remove(name);
                }
            }
        }
        command
            .arg("run")
            .arg(run_path)
            .current_dir(&project.root)
            .env("ZKCL_OUT_DIR", directory)
            .env("ZKCL_BUILD_TARGET", target)
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        let mut child = command.spawn()?;
        let mut stdout = child.stdout.take().expect("build stdout is piped");
        let copied = io::copy(&mut stdout, &mut io::stderr());
        drop(stdout);
        let status = child.wait()?;
        copied?;
        if !status.success() {
            return Err(format!("build script {} failed with {status}", script.display()).into());
        }
    } else {
        let mut runtime = options.runtime.clone().start()?;
        for (name, value) in &options.environment {
            runtime.set_environment_variable(name, value.as_deref())?;
        }
        runtime.set_working_directory(&project.root)?;
        runtime.set_environment_variable(
            "ZKCL_OUT_DIR",
            Some(directory.to_str().ok_or("build output path is not UTF-8")?),
        )?;
        runtime.set_environment_variable("ZKCL_BUILD_TARGET", Some(target))?;
        runtime.set_standard_input(io::Cursor::new(Vec::<u8>::new()));
        runtime.set_standard_output(io::stderr());
        if let Some(initialize) = &options.initialize_runtime {
            initialize(&runtime)?;
        }
        runtime.load_file(&run_path)?;
        if runtime.requested_exit_code() != 0 {
            return Err(format!(
                "build script {} requested exit status {}",
                script.display(),
                runtime.requested_exit_code()
            )
            .into());
        }
    }
    let digests = output_digests(&files, &build.outputs)?;
    copy_outputs(&files, output, &build.outputs)?;
    if let Some((entry, key, snapshots, _lock)) = cache_entry
        && snapshots_current(&snapshots)
    {
        fs::write(
            stage.path().join("outputs.json"),
            serde_json::to_vec(&CacheReceipt {
                key,
                outputs: digests,
            })?,
        )?;
        if entry.exists() {
            fs::remove_dir_all(&entry)?;
        }
        fs::rename(stage.path(), &entry)?;
    }
    Ok(())
}

fn snapshots_current(snapshots: &[(PathBuf, Digest)]) -> bool {
    snapshots.iter().all(|(file, digest)| {
        File::open(file)
            .and_then(Digest::of_reader)
            .is_ok_and(|current| current == *digest)
    })
}

fn output_digests(
    root: &Path,
    outputs: &std::collections::BTreeSet<PathBuf>,
) -> Result<Vec<String>, Box<dyn Error>> {
    if !fs::symlink_metadata(root)?.file_type().is_dir() {
        return Err("build output root must be a regular directory".into());
    }
    outputs
        .iter()
        .map(|path| {
            let full = root.join(path);
            // Cached output must be a regular file inside the staging directory. Reject
            // symlinks even when their current target is inside it, keeping copies independent.
            let mut ancestor = full.as_path();
            while ancestor != root {
                if fs::symlink_metadata(ancestor)?.file_type().is_symlink() {
                    return Err(format!(
                        "build output {} contains a symbolic link",
                        path.display()
                    )
                    .into());
                }
                ancestor = ancestor.parent().ok_or("invalid generated output path")?;
            }
            if !full.is_file() {
                return Err(format!("build output {} is not a file", path.display()).into());
            }
            Ok(Digest::of_bytes(&fs::read(full)?).to_string())
        })
        .collect()
}

#[derive(Deserialize, Serialize)]
struct CacheReceipt {
    key: String,
    outputs: Vec<String>,
}

fn valid_entry(entry: &Path, key: &str, outputs: &std::collections::BTreeSet<PathBuf>) -> bool {
    let Ok(metadata) = fs::read(entry.join("outputs.json")) else {
        return false;
    };
    let Ok(receipt) = serde_json::from_slice::<CacheReceipt>(&metadata) else {
        return false;
    };
    receipt.key == key
        && output_digests(&entry.join("files"), outputs)
            .is_ok_and(|actual| actual == receipt.outputs)
}

fn copy_outputs(
    source: &Path,
    destination: &Path,
    outputs: &std::collections::BTreeSet<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(destination)?;
    for path in outputs {
        let to = destination.join(path);
        fs::create_dir_all(to.parent().expect("output has parent"))?;
        fs::copy(source.join(path), to)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn scratch() -> tempfile::TempDir {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.agents/tmp/zkcl/build-tasks");
        fs::create_dir_all(&root).unwrap();
        tempfile::tempdir_in(root).unwrap()
    }

    #[test]
    fn cached_outputs_are_verified_and_copied_independently() {
        let directory = scratch();
        let entry = directory.path().join("entry");
        let files = entry.join("files");
        fs::create_dir_all(files.join("generated")).unwrap();
        let outputs = BTreeSet::from([PathBuf::from("generated/value.lisp")]);
        fs::write(
            files.join("generated/value.lisp"),
            b"(defconstant value 42)",
        )
        .unwrap();
        let digests = output_digests(&files, &outputs).unwrap();
        fs::write(
            entry.join("outputs.json"),
            serde_json::to_vec(&CacheReceipt {
                key: "original-inputs".into(),
                outputs: digests,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(valid_entry(&entry, "original-inputs", &outputs));
        assert!(!valid_entry(&entry, "different-inputs", &outputs));
        let copied = directory.path().join("invocation");
        copy_outputs(&files, &copied, &outputs).unwrap();
        fs::write(copied.join("generated/value.lisp"), b"modified").unwrap();
        assert!(valid_entry(&entry, "original-inputs", &outputs));
        fs::write(files.join("generated/value.lisp"), b"corrupted").unwrap();
        assert!(!valid_entry(&entry, "original-inputs", &outputs));
        fs::remove_file(files.join("generated/value.lisp")).unwrap();
        assert!(!valid_entry(&entry, "original-inputs", &outputs));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_generated_output_symlinks() {
        let directory = scratch();
        fs::write(directory.path().join("real"), b"data").unwrap();
        std::os::unix::fs::symlink("real", directory.path().join("linked")).unwrap();
        assert!(
            output_digests(directory.path(), &BTreeSet::from([PathBuf::from("linked")])).is_err()
        );
    }
}
