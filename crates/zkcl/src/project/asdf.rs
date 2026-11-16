use std::collections::BTreeMap;
use std::error::Error;
use std::io;
use std::path::{Component, Path, PathBuf};

use zkcl_runtime::{Root, Runtime};

/// ASDF system definitions, source files and resources needed by a delivered program.
#[derive(Debug)]
pub struct AsdfDelivery {
    pub systems: Vec<(String, PathBuf)>,
    pub aliases: Vec<(String, String)>,
    pub sources: Vec<AsdfSourceTree>,
    pub compiled: Vec<AsdfCompiledComponent>,
}

/// Compile-op outputs identified by ASDF's logical component path.
#[derive(Debug)]
pub struct AsdfCompiledComponent {
    pub path: Vec<String>,
    pub outputs: Vec<PathBuf>,
}

fn copy_file(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::copy(source, destination)?;
    let modified = std::fs::metadata(source)?.modified()?;
    std::fs::File::options()
        .write(true)
        .open(destination)?
        .set_times(std::fs::FileTimes::new().set_modified(modified))
}

#[derive(Debug)]
pub struct AsdfSourceTree {
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
}

/// Stage only the graph's declared source/resource inputs. Installed ASDF
/// systems discover their source directory here; compiled output is supplied
/// separately by the launcher and need not be writable beside the executable.
pub fn stage_delivery(delivery: &AsdfDelivery, destination: &Path) -> Result<(), Box<dyn Error>> {
    let mut definitions = BTreeMap::new();
    for (index, source) in delivery.sources.iter().enumerate() {
        let root = source.root.canonicalize()?;
        let target = destination.join("asdf-sources").join(index.to_string());
        for relative in &source.files {
            if relative.as_os_str().is_empty()
                || relative
                    .components()
                    .any(|part| !matches!(part, Component::Normal(_)))
            {
                return Err(format!("invalid ASDF delivery input {}", relative.display()).into());
            }
            let input = root.join(relative);
            let canonical = input.canonicalize()?;
            if !canonical.starts_with(&root) || !canonical.is_file() {
                return Err(format!(
                    "ASDF delivery input escapes its source root or is not a file: {}",
                    input.display()
                )
                .into());
            }
            let output = target.join(relative);
            std::fs::create_dir_all(output.parent().expect("input has a filename"))?;
            copy_file(&canonical, &output)?;
            for (name, definition) in &delivery.systems {
                if definition.canonicalize()? == canonical {
                    let staged = format!("asdf-sources/{index}/{}", path_text(relative)?);
                    let primary = name.split('/').next().unwrap_or(name);
                    for selected in [name.as_str(), primary] {
                        if let Some(previous) =
                            definitions.insert(selected.to_owned(), staged.clone())
                            && previous != staged
                        {
                            return Err(format!("ASDF delivery system {selected:?} has conflicting source definitions").into());
                        }
                    }
                }
            }
        }
    }
    for (name, definition) in &delivery.systems {
        if !definitions.contains_key(name) {
            return Err(format!("ASDF definition {name:?} at {} is absent from declared delivery inputs; include its .asd source", definition.display()).into());
        }
    }
    let systems = definitions
        .iter()
        .map(|(name, relative)| {
            format!(
                "(cons {} (merge-pathnames {} root))",
                lisp_string(name),
                lisp_string(relative)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let aliases = delivery
        .aliases
        .iter()
        .map(|(name, version)| format!("(cons {} {})", lisp_string(name), lisp_string(version)))
        .collect::<Vec<_>>()
        .join(" ");
    let mut compiled = Vec::new();
    for (index, component) in delivery.compiled.iter().enumerate() {
        let directory = destination.join("asdf-compiled").join(index.to_string());
        std::fs::create_dir_all(&directory)?;
        let mut outputs = Vec::new();
        let mut names = std::collections::BTreeSet::new();
        for file in &component.outputs {
            let name = file
                .file_name()
                .ok_or("ASDF compile output has no filename")?;
            if !names.insert(name.to_owned()) {
                return Err("ASDF compile outputs have duplicate filenames".into());
            }
            copy_file(file, &directory.join(name))?;
            let relative = PathBuf::from("asdf-compiled")
                .join(index.to_string())
                .join(name);
            outputs.push(format!(
                "(merge-pathnames {} output-root)",
                lisp_string(path_text(&relative)?)
            ));
        }
        let path = component
            .path
            .iter()
            .map(|name| lisp_string(name))
            .collect::<Vec<_>>()
            .join(" ");
        compiled.push(format!("(cons (list {path}) (list {}))", outputs.join(" ")));
    }
    let compiled = compiled.join(" ");
    let script = format!(
        "(require :asdf)\n(let ((*package* *package*)) (with-input-from-string (input {}) (uiop:eval-input input)))\n(in-package #:zkcl-project-asdf)\n(defun initialize-delivery (source-root output-directory)\n (let ((root (uiop:ensure-directory-pathname source-root))\n       (output-root (uiop:ensure-directory-pathname output-directory)))\n  (configure (list {systems}) (list {aliases}) output-directory)\n  (setf *compiled-components* (list {compiled}))))\n",
        lisp_string(POLICY),
    );
    std::fs::create_dir_all(destination)?;
    std::fs::write(destination.join("asdf-bootstrap.lisp"), script)?;
    Ok(())
}

pub fn initialize_delivery(
    runtime: &Runtime,
    assets: &Path,
    output: &Path,
) -> Result<(), Box<dyn Error>> {
    // Installation may rewrite source mtimes. Verified compiled outputs are
    // restored to writable storage no older than any delivered input, letting
    // ASDF use its ordinary freshness rules without replaying compilation.
    fn latest(directory: &Path, stamp: &mut std::time::SystemTime) -> io::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::other("symbolic link in ASDF delivery"));
            }
            if metadata.is_dir() {
                latest(&path, stamp)?;
            } else if metadata.is_file() {
                *stamp = (*stamp).max(metadata.modified()?);
            }
        }
        Ok(())
    }
    fn restore(source: &Path, destination: &Path, stamp: std::time::SystemTime) -> io::Result<()> {
        std::fs::create_dir_all(destination)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            let from = entry.path();
            let to = destination.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                restore(&from, &to, stamp)?;
            } else {
                std::fs::copy(&from, &to)?;
                std::fs::File::options()
                    .write(true)
                    .open(to)?
                    .set_times(std::fs::FileTimes::new().set_modified(stamp))?;
            }
        }
        Ok(())
    }
    let compiled = assets.join("asdf-compiled");
    if compiled.is_dir() {
        let mut stamp = std::time::UNIX_EPOCH;
        latest(assets, &mut stamp)?;
        restore(&compiled, &output.join("asdf-compiled"), stamp)?;
    }
    runtime.load_file(&assets.join("asdf-bootstrap.lisp"))?;
    let initialize = runtime
        .lookup_symbol("ZKCL-PROJECT-ASDF", "INITIALIZE-DELIVERY")?
        .ok_or("missing ASDF delivery initializer")?;
    let assets = runtime.make_string(path_text(&assets.canonicalize()?)?)?;
    let output = runtime.make_string(&format!("{}/", path_text(&output.canonicalize()?)?))?;
    runtime.call(&initialize, &[&assets, &output])?;
    Ok(())
}

pub struct AsdfSystem<'a> {
    pub name: &'a str,
    pub definition: &'a Path,
}

pub struct AsdfAlias<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

// Project runtimes are fresh instances. The policy lives in that instance, so
// ASDF requests made later by native components use the same selected sources.
// Executable Lisp retains the runtime's host permissions.
pub fn configure(
    runtime: &Runtime,
    systems: &[AsdfSystem<'_>],
    aliases: &[AsdfAlias<'_>],
    output_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let mut definitions = BTreeMap::new();
    for system in systems {
        let definition = system.definition.canonicalize().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot access ASDF definition for {:?} at {}: {error}",
                    system.name,
                    system.definition.display()
                ),
            )
        })?;
        if !definition.is_file() {
            return Err(io::Error::other(format!(
                "ASDF definition for {:?} is not a file: {}",
                system.name,
                definition.display()
            ))
            .into());
        }
        // ASDF asks for a secondary system's primary system while defining and
        // planning it, even when the consumer selected only the secondary name.
        let primary = system.name.split('/').next().unwrap_or(system.name);
        for name in [system.name, primary] {
            if let Some(previous) = definitions.insert(name, definition.clone())
                && previous != definition
            {
                return Err(io::Error::other(format!(
                    "ASDF system {name:?} has conflicting definitions: {} and {}",
                    previous.display(),
                    definition.display()
                ))
                .into());
            }
        }
    }
    std::fs::create_dir_all(output_dir)?;
    let output_dir = output_dir.canonicalize()?;
    let entries = definitions
        .iter()
        .map(|(name, definition)| {
            Ok(format!(
                "(cons {} (pathname {}))",
                lisp_string(name),
                lisp_path(definition)?
            ))
        })
        .collect::<Result<Vec<_>, io::Error>>()?
        .join(" ");
    let aliases = aliases
        .iter()
        .map(|alias| {
            format!(
                "(cons {} {})",
                lisp_string(alias.name),
                lisp_string(alias.version)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    runtime.require_module("asdf")?;
    runtime.eval(&format!(
        "(let ((*package* *package*))
             (with-input-from-string (input {}) (uiop:eval-input input)))",
        lisp_string(POLICY)
    ))?;
    runtime.eval(&format!(
        "(zkcl-project-asdf::configure (list {entries}) (list {aliases}) {})",
        lisp_string(&format!("{}/", path_text(&output_dir)?))
    ))?;
    Ok(())
}

/// Announce completion of the native library unit. ASDF's
/// standard operations use that same node and never reload its source or FASL.
pub fn mark_native_ready(runtime: &Runtime, names: &[&str]) -> Result<(), Box<dyn Error>> {
    for name in names {
        runtime.eval(&format!(
            "(zkcl-project-asdf::mark-native-ready {})",
            lisp_string(name)
        ))?;
    }
    Ok(())
}

pub fn load(runtime: &Runtime, name: &str) -> Result<(), Box<dyn Error>> {
    operate(runtime, name, AsdfOperation::Load)
}

/// Load a standalone system through ASDF's load-op and remember its source for
/// future operations in this runtime. The explicit definition's directory is
/// searched before `directory` and the user's ASDF configuration.
/// Return the native namestring of the definition used by ASDF, if any.
pub fn load_from_directory(
    runtime: &Runtime,
    system: &str,
    directory: &Path,
    definition: Option<&Path>,
) -> Result<Option<String>, Box<dyn Error>> {
    let absolute = |path: &Path| -> io::Result<PathBuf> {
        if path.is_absolute() {
            Ok(path.to_owned())
        } else {
            Ok(runtime.working_directory()?.join(path))
        }
    };
    let definition = definition
        .map(|path| -> Result<PathBuf, io::Error> {
            let canonical = absolute(path).and_then(|path| path.canonicalize());
            let canonical = canonical.map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("ASDF system {system:?}: cannot access {}: {error}", path.display()),
                )
            })?;
            if !canonical.is_file() {
                return Err(io::Error::other(format!(
                    "ASDF system {system:?}: definition {} is not a file",
                    canonical.display()
                )));
            }
            Ok(canonical)
        })
        .transpose()?;
    let directory = absolute(directory)?;
    runtime.require_module("asdf")?;
    let loader = "LOAD-STANDALONE-ASDF-SYSTEM";
    if !runtime
        .lookup_symbol("ZK", loader)?
        .is_some_and(|symbol| runtime.resolve_function(&symbol).is_ok())
    {
        let bridge = runtime.make_string(STANDALONE_POLICY)?;
        let evaluate = lisp_symbol(runtime, "UIOP", "STANDARD-EVAL-THUNK")?;
        runtime.call(&evaluate, &[&bridge])?;
    }
    let loader = lisp_symbol(runtime, "ZK", loader)?;
    let name = runtime.make_string(system)?;
    let directory = runtime.make_string(path_text(&directory)?)?;
    let definition = match definition {
        Some(path) => runtime.make_string(path_text(&path)?)?,
        None => runtime.nil(),
    };
    let result = runtime
        .call(&loader, &[&name, &directory, &definition])
        .map_err(|source| AsdfError {
            system: system.to_owned(),
            operation: AsdfOperation::Load,
            source,
        })?;
    read_optional_string(runtime, result.first().ok_or("missing ASDF source result")?)
}

/// Read the source of an already registered system without discovering or
/// evaluating .asd files.
pub fn registered_system_source(
    runtime: &Runtime,
    system: &str,
) -> Result<Option<String>, Box<dyn Error>> {
    runtime.require_module("asdf")?;
    let name = runtime.make_string(system)?;
    let system = call_primary(runtime, "ASDF", "REGISTERED-SYSTEM", &[&name])?;
    if system.is_nil() {
        return Ok(None);
    }
    let path = call_primary(runtime, "ASDF", "SYSTEM-SOURCE-FILE", &[&system])?;
    if path.is_nil() {
        return Ok(None);
    }
    let path = call_primary(runtime, "UIOP", "NATIVE-NAMESTRING", &[&path])?;
    read_optional_string(runtime, &path)
}

fn read_optional_string(
    runtime: &Runtime,
    value: &Root<'_>,
) -> Result<Option<String>, Box<dyn Error>> {
    if value.is_nil() {
        return Ok(None);
    }
    runtime
        .string(value)
        .map(Some)
        .ok_or_else(|| "ASDF query returned a value other than a string or NIL".into())
}

fn lisp_symbol<'rt>(
    runtime: &'rt Runtime,
    package: &str,
    name: &str,
) -> Result<Root<'rt>, Box<dyn Error>> {
    runtime
        .lookup_symbol(package, name)?
        .ok_or_else(|| format!("missing {package}:{name}").into())
}

fn call_primary<'rt>(
    runtime: &'rt Runtime,
    package: &str,
    name: &str,
    args: &[&Root<'_>],
) -> Result<Root<'rt>, Box<dyn Error>> {
    let function = lisp_symbol(runtime, package, name)?;
    runtime
        .call(&function, args)?
        .into_iter()
        .next()
        .ok_or_else(|| format!("{package}:{name} returned no values").into())
}

/// Standard ASDF operations, executed by the running ASDF implementation.
#[derive(Clone, Copy, Debug)]
pub enum AsdfOperation {
    Load,
    Compile,
    Test,
}

pub fn operate(
    runtime: &Runtime,
    system: &str,
    operation: AsdfOperation,
) -> Result<(), Box<dyn Error>> {
    runtime.require_module("asdf")?;
    let function = lisp_symbol(
        runtime,
        "ASDF",
        match operation {
            AsdfOperation::Load => "LOAD-SYSTEM",
            AsdfOperation::Compile => "COMPILE-SYSTEM",
            AsdfOperation::Test => "TEST-SYSTEM",
        },
    )?;
    let name = runtime.make_string(system)?;
    let result = match runtime.lookup_symbol("ZK", "CALL-STANDALONE-ASDF-OPERATION")? {
        Some(standalone) => runtime.call(&standalone, &[&function, &name]),
        None => runtime.call(&function, &[&name]),
    };
    result.map_err(|source| AsdfError {
        system: system.to_owned(),
        operation,
        source,
    })?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("ASDF {operation:?} for system {system:?}: {source}")]
pub struct AsdfError {
    pub system: String,
    pub operation: AsdfOperation,
    #[source]
    pub source: zkcl_runtime::EvalError,
}

/// Metadata obtained from the actual ASDF component graph after explicit execution.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AsdfComponentInfo {
    pub name: String,
    pub pathname: Option<String>,
    pub children: Vec<AsdfComponentInfo>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct AsdfSystemInfo {
    pub name: String,
    pub version: Option<String>,
    pub definition: Option<String>,
    pub loaded: bool,
    pub component: AsdfComponentInfo,
}

fn values<'rt>(
    runtime: &'rt Runtime,
    value: &zkcl_runtime::Root<'rt>,
) -> Result<Vec<zkcl_runtime::Root<'rt>>, Box<dyn Error>> {
    let mut tail = value.clone();
    let mut items = Vec::new();
    while !tail.is_nil() {
        let (head, next) = runtime
            .cons_fields(&tail)
            .ok_or("ASDF query returned an improper list")?;
        items.push(head);
        tail = next;
    }
    Ok(items)
}

/// Enumerate registered definitions without discovering additional .asd files.
pub fn registered_systems(runtime: &Runtime) -> Result<Vec<String>, Box<dyn Error>> {
    runtime.require_module("asdf")?;
    let result = call_primary(runtime, "ASDF", "REGISTERED-SYSTEMS", &[])?;
    let mut names = values(runtime, &result)?
        .iter()
        .map(|value| {
            runtime
                .string(value)
                .ok_or_else(|| "ASDF system name is not a string".into())
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    names.sort();
    Ok(names)
}

/// Ask ASDF to find a system and describe its real component graph. This may
/// execute the selected .asd file and user-defined component accessors.
/// Static source tools should continue to use project metadata instead.
pub fn system_info(
    runtime: &Runtime,
    name: &str,
) -> Result<Option<AsdfSystemInfo>, Box<dyn Error>> {
    runtime.require_module("asdf")?;
    let describe = runtime.eval(
        r#"(lambda (name)
      (let ((system (asdf:find-system name nil)))
        (when system
          (labels ((describe-component (component)
                     (list (asdf:component-name component)
                           (let ((path (asdf:component-pathname component)))
                             (when path (namestring path)))
                           (when (typep component 'asdf:module)
                             (mapcar #'describe-component (asdf:component-children component))))))
            (list (asdf:component-name system) (asdf:component-version system)
                  (let ((path (asdf:system-source-file system)))
                    (when path (namestring path)))
                  (asdf:component-loaded-p system) (describe-component system))))))"#,
    )?;
    let argument = runtime.make_string(name)?;
    let result = runtime.call(&describe[0], &[&argument])?;
    if result[0].is_nil() {
        return Ok(None);
    }
    let fields = values(runtime, &result[0])?;
    fn component(
        runtime: &Runtime,
        value: &zkcl_runtime::Root<'_>,
    ) -> Result<AsdfComponentInfo, Box<dyn Error>> {
        let fields = values(runtime, value)?;
        let [name, pathname, children] = fields.as_slice() else {
            return Err("invalid ASDF component metadata".into());
        };
        Ok(AsdfComponentInfo {
            name: runtime.string(name).ok_or("invalid ASDF component name")?,
            pathname: runtime.string(pathname),
            children: values(runtime, children)?
                .iter()
                .map(|child| component(runtime, child))
                .collect::<Result<_, _>>()?,
        })
    }
    let [name, version, definition, loaded, tree] = fields.as_slice() else {
        return Err("invalid ASDF system metadata".into());
    };
    Ok(Some(AsdfSystemInfo {
        name: runtime.string(name).ok_or("invalid ASDF system name")?,
        version: runtime.string(version),
        definition: runtime.string(definition),
        loaded: !loaded.is_nil(),
        component: component(runtime, tree)?,
    }))
}

fn lisp_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn path_text(path: &Path) -> Result<&str, io::Error> {
    path.to_str().ok_or_else(|| {
        io::Error::other(format!(
            "ASDF project paths must be UTF-8: {}",
            path.display()
        ))
    })
}

fn lisp_path(path: &Path) -> Result<String, io::Error> {
    Ok(lisp_string(path_text(path)?))
}

/// Record completed compile-op outputs so delivered ASDF systems do not rebuild
/// against a different compilation environment in a fresh runtime.
pub fn compiled_components(
    runtime: &Runtime,
    name: &str,
) -> Result<Vec<AsdfCompiledComponent>, Box<dyn Error>> {
    let query = runtime.eval(
        r#"(lambda (name)
      (labels ((walk (component)
                 (let ((outputs (asdf:output-files 'asdf:compile-op component)))
                   (append (when outputs
                             (list (list (asdf:component-find-path component)
                                         (mapcar #'namestring outputs))))
                           (when (typep component 'asdf:module)
                             (mapcan #'walk (asdf:component-children component)))))))
        (walk (asdf:find-system name))))"#,
    )?;
    let argument = runtime.make_string(name)?;
    let result = runtime.call(&query[0], &[&argument])?;
    values(runtime, &result[0])?
        .iter()
        .map(|record| {
            let fields = values(runtime, record)?;
            let [path, outputs] = fields.as_slice() else {
                return Err("invalid ASDF compile output record".into());
            };
            Ok(AsdfCompiledComponent {
                path: values(runtime, path)?
                    .iter()
                    .map(|name| {
                        runtime
                            .string(name)
                            .ok_or_else(|| "invalid ASDF component path".into())
                    })
                    .collect::<Result<_, Box<dyn Error>>>()?,
                outputs: values(runtime, outputs)?
                    .iter()
                    .map(|path| {
                        runtime
                            .string(path)
                            .ok_or_else(|| "invalid ASDF output path".into())
                            .and_then(|path| Ok(PathBuf::from(path).canonicalize()?))
                    })
                    .collect::<Result<_, Box<dyn Error>>>()?,
            })
        })
        .collect()
}

const STANDALONE_POLICY: &str = r#"
(defpackage #:zkcl-standalone-asdf (:use #:cl))
(in-package #:zkcl-standalone-asdf)

(defvar *selected-definitions* nil)

(defun selected-source (name)
  (cdr (assoc (asdf:primary-system-name name) *selected-definitions* :test #'equal)))

(defun find-selected-source (name)
  (let ((source (selected-source name)))
    (when source
      (or (probe-file source) (error 'file-error :pathname source)))))

(defun select-source (name source)
  (dolist (primary (list (asdf:primary-system-name name) (pathname-name source)))
    (setf *selected-definitions*
          (acons primary source (remove primary *selected-definitions* :key #'car :test #'equal))))
  (pushnew 'find-selected-source asdf:*system-definition-search-functions*))

(defun prefer-directory (directory registry)
  (cons directory (remove directory registry :test #'equal)))

(defun prefer-source-directory (source registry)
  (if source
      (prefer-directory (uiop:pathname-directory-pathname source) registry)
      registry))

(defun zk::call-standalone-asdf-operation (operation name)
  (let* ((source (selected-source name))
         (asdf:*central-registry*
           (prefer-source-directory source asdf:*central-registry*)))
    (asdf/session:with-asdf-session ()
      (when (and source (not (equal (asdf:primary-system-name name) (pathname-name source))))
        (asdf:find-system (pathname-name source) nil))
      (funcall operation name))))

(defun resolve-explicit-definition (name source)
  (let ((file-system (pathname-name source)))
    ;; Discard stale registrations before looking up the explicit source.
    ;; ASDF may otherwise carry component metadata from another file into
    ;; a registration for this source when the requested system is absent.
    (dolist (candidate (list name (asdf:primary-system-name name) file-system))
      (let* ((previous (asdf:registered-system candidate))
             (previous-source (asdf:system-source-file previous)))
        (when (and previous
                   (not (and previous-source (uiop:pathname-equal source previous-source))))
          (asdf:clear-system candidate))))
    (select-source name source)
    ;; ASDF schedules DEFINE-OP for the file-named system, including when
    ;; loading an alias. Resolve that system first to share its definition stamp.
    (asdf:find-system file-system nil)
    (let ((actual (asdf:system-source-file (asdf:find-system name nil))))
      (unless (and actual (uiop:pathname-equal source actual))
        (error "ASDF definition ~A does not define system ~S." source name)))))

(defun zk::load-standalone-asdf-system (name directory definition)
  (let ((directory (uiop:parse-native-namestring directory :ensure-directory t))
        (source (when definition (uiop:parse-native-namestring definition))))
    (setf asdf:*central-registry* (prefer-directory directory asdf:*central-registry*))
    (let ((actual (let ((*selected-definitions* *selected-definitions*))
                    (if source
                        (let ((asdf:*central-registry*
                                (prefer-source-directory source asdf:*central-registry*)))
                          (asdf/session:with-asdf-session ()
                            (resolve-explicit-definition name source)
                            (asdf:load-system name)))
                        (zk::call-standalone-asdf-operation #'asdf:load-system name))
                    (asdf:system-source-file (asdf:registered-system name)))))
      ;; Publish the pin only after a successful load. A later attempt can still
      ;; resolve the earlier selection if this file or its components failed.
      (when actual
        (select-source name actual)
        (uiop:native-namestring actual)))))
"#;

const POLICY: &str = r#"
(defpackage #:zkcl-project-asdf (:use #:cl))
(in-package #:zkcl-project-asdf)

(defvar *definitions* nil)
(defvar *active* nil)
(defvar *aliases* nil)
(defvar *native-ready* nil)
(defvar *compiled-components* nil)

(defmethod asdf:output-files :around ((operation asdf:compile-op) (component asdf:component))
  (let ((selected (and *active* (assoc (asdf:component-find-path component)
                                      *compiled-components* :test #'equal))))
    (if selected (values (cdr selected) t) (call-next-method))))

(defclass native-system (asdf:system) ())

(defun native-system-p (name)
  (assoc name *aliases* :test #'equal))

(defun check-native-ready (system)
  (unless (member (asdf:component-name system) *native-ready* :test #'equal)
    (error "Native ASDF alias ~S was requested before its locked native library completed."
           (asdf:component-name system))))

(defmethod asdf:component-depends-on :before
    ((operation asdf:operation) (system native-system))
  (unless (member (class-name (class-of operation))
                  '(asdf:compile-op asdf:load-op asdf:prepare-op asdf:define-op))
    (error "ASDF operation ~S has no published native alias contract for ~S."
           (type-of operation) (asdf:component-name system))))
(defmethod asdf:perform ((operation asdf:compile-op) (system native-system))
  (check-native-ready system))
(defmethod asdf:perform ((operation asdf:load-op) (system native-system))
  (check-native-ready system))

(defun mark-native-ready (name)
  (unless (native-system-p name)
    (error "~S is not a selected native ASDF alias." name))
  (pushnew name *native-ready* :test #'equal)
  (values))
(defparameter *toolchain-systems* '("asdf" "uiop" "asdf-package-system"))

(defun selected-definition (name)
  (cdr (assoc name *definitions* :test #'equal)))

(defun toolchain-system-p (name)
  (member name *toolchain-systems* :test #'equal))

(defun selected-system-p (name)
  (or (selected-definition name) (native-system-p name) (toolchain-system-p name)))

(defun unselected-system-error (name)
  (error "ASDF system ~S is not selected by the locked project. Declare its system and .asd source in the project dependencies."
         name))

(defmethod asdf:operate :before ((operation symbol) designator &key &allow-other-keys)
  (declare (ignore operation))
  ;; Standard system operations resolve names with an optional lookup.
  (when (and *active* (typep designator '(or string symbol)))
    (let ((name (asdf:coerce-name designator)))
      (unless (selected-system-p name)
        (unselected-system-error name)))))

(defun check-system-source (name system)
  (when system
    (let ((expected (selected-definition name))
          (actual (asdf:system-source-file system)))
      (cond
        ((native-system-p name)
         (unless (and (typep system 'native-system) (not actual))
           (error "Locked native ASDF alias ~S conflicts with a registered system from ~A."
                  name actual)))
        (expected
         (unless (and actual (equal expected (probe-file actual)))
           (error "Locked project ASDF system ~S must come from ~A, but is registered from ~A."
                  name expected actual)))
        ((toolchain-system-p name)
         (when actual
           (error "Locked project ASDF toolchain system ~S must be built in, but is registered from ~A."
                  name actual))))))
  system)

(defun sysdef-selected (name)
  (or (selected-definition name)
      (when (toolchain-system-p name)
        (check-system-source name (asdf:registered-system name)))))

(defun definition-in-progress (name)
  ;; DEFSYSTEM resolves a component's parent through FIND-SYSTEM while it is
  ;; constructing every definition in an .asd, including unselected siblings.
  ;; Permit that optional metadata lookup only in the selected definition file.
  (let* ((system (and asdf/session:*asdf-session*
                      (asdf/find-system::find-system-if-being-defined name)))
         (source (and system (asdf:system-source-file system))))
    (when (and source *load-truename*
               (equal source *load-truename*)
               (find source *definitions* :key #'cdr :test #'equal))
      system)))

(defmethod asdf:find-system :around (designator &optional (error-p t))
  (if (not *active*)
      (call-next-method)
      (let ((name (asdf:coerce-name designator)))
        (cond
          ((selected-system-p name)
           (check-system-source name (asdf:registered-system name))
           (check-system-source name (call-next-method)))
          ((and (not error-p) (definition-in-progress name)))
          (error-p (unselected-system-error name))
          (t nil)))))

(defun configure (definitions aliases output-directory)
  (when *active*
    (error "ASDF project resolution is already configured in this runtime."))
  (dolist (entry definitions)
    (when (toolchain-system-p (car entry))
      (error "ASDF system ~S is supplied by the built-in toolchain and cannot be replaced by ~A."
             (car entry) (cdr entry))))
  (dolist (alias aliases)
    (when (or (assoc (car alias) definitions :test #'equal)
              (toolchain-system-p (car alias)))
      (error "Native ASDF alias ~S conflicts with another selected system provider."
             (car alias)))
    (when (asdf:registered-system (car alias))
      (error "Native ASDF alias ~S is already registered." (car alias))))
  (let ((*definitions* definitions))
    (dolist (entry definitions)
      (check-system-source (car entry) (asdf:registered-system (car entry))))
    (dolist (name *toolchain-systems*)
      (check-system-source name (asdf:registered-system name))))
  (asdf:initialize-source-registry
    '(:source-registry :ignore-inherited-configuration))
  (asdf:initialize-output-translations
    `(:output-translations (t ,output-directory) :ignore-inherited-configuration))
  ;; The bridge belongs to the native build, never to an enclosing LOAD file.
  (let ((*load-pathname* nil) (*load-truename* nil)
        (*compile-file-pathname* nil) (*compile-file-truename* nil))
    (dolist (alias aliases)
      (eval `(asdf:defsystem ,(car alias) :class native-system :version ,(cdr alias)))))
  (setf *aliases* aliases
        *definitions* definitions
        asdf:*central-registry* nil
        asdf:*system-definition-search-functions*
          '(asdf/package-inferred-system::sysdef-package-inferred-system-search
            sysdef-selected)
        *active* t)
  (values))
"#;

#[cfg(test)]
mod delivery_tests {
    use super::*;

    #[test]
    fn relocated_asdf_sources_keep_resources_and_custom_load_operations() {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.agents/tmp/zkcl/repl_project_targets");
        std::fs::create_dir_all(&scratch).unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let original = fixture.path().join("original");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::write(
            original.join("resource.asd"),
            r#"(asdf:defsystem "resource"
          :components ((:file "library"))
          :perform (asdf:load-op :after (operation system)
            (declare (ignore operation system))
            (setf (get 'cl-user::delivery-resource 'cl-user::custom-operation) 99)))"#,
        )
        .unwrap();
        std::fs::write(
            original.join("library.lisp"),
            r#"(defpackage #:delivery-resource (:use #:cl))
(in-package #:delivery-resource)
(defparameter *resource*
  (with-open-file (input (asdf:system-relative-pathname "resource" "data.txt"))
    (read-line input)))"#,
        )
        .unwrap();
        std::fs::write(original.join("data.txt"), "relocated resource").unwrap();
        let delivery = AsdfDelivery {
            systems: vec![("resource".into(), original.join("resource.asd"))],
            aliases: Vec::new(),
            compiled: Vec::new(),
            sources: vec![AsdfSourceTree {
                root: original.clone(),
                files: ["resource.asd", "library.lisp", "data.txt"]
                    .map(Into::into)
                    .into(),
            }],
        };
        let assets = fixture.path().join("installed");
        stage_delivery(&delivery, &assets).unwrap();
        std::fs::remove_dir_all(original).unwrap();
        let output = fixture.path().join("writable-output");
        std::fs::create_dir_all(&output).unwrap();
        let runtime = Runtime::start().unwrap();
        initialize_delivery(&runtime, &assets, &output).unwrap();
        load(&runtime, "resource").unwrap();
        let values = runtime.eval("(values delivery-resource::*resource* (get 'cl-user::delivery-resource 'cl-user::custom-operation))").unwrap();
        assert_eq!(runtime.string(&values[0]).unwrap(), "relocated resource");
        assert_eq!(values[1].as_fixnum(), Some(99));
        assert!(std::fs::read_dir(output).unwrap().next().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn staged_resource_symlinks_cannot_escape_the_selected_source_root() {
        let scratch = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.agents/tmp/zkcl/repl_project_targets");
        std::fs::create_dir_all(&scratch).unwrap();
        let fixture = tempfile::tempdir_in(scratch).unwrap();
        let original = fixture.path().join("source");
        std::fs::create_dir_all(&original).unwrap();
        let secret = fixture.path().join("outside");
        std::fs::write(&secret, "outside").unwrap();
        std::os::unix::fs::symlink(secret, original.join("resource")).unwrap();
        let delivery = AsdfDelivery {
            systems: Vec::new(),
            aliases: Vec::new(),
            compiled: Vec::new(),
            sources: vec![AsdfSourceTree {
                root: original,
                files: vec!["resource".into()],
            }],
        };
        assert!(
            stage_delivery(&delivery, &fixture.path().join("assets"))
                .unwrap_err()
                .to_string()
                .contains("escapes its source root")
        );
    }
}
