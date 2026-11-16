use std::error::Error;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use zkcl::{Root, Runtime};
use zkcl_syntax::documentation::{DocumentationExample, DocumentationTests};
use zkcl_syntax::syntax::SourceReadtable;
use zkcl_syntax::{SourceSnapshot, SourceWorkspace};

const LOAD_SYSTEM: &str = r#"
#'(lambda (name)
    (let ((system (asdf:find-system name)))
      (asdf:load-system system)
      (values
        (let ((directory (asdf:system-source-directory system)))
          (and directory (namestring directory)))
        (mapcar (lambda (component)
                  (let ((hook (asdf/component:around-compile-hook component)))
                    (cons (namestring (asdf:component-pathname component))
                          (or (eq hook 'zk:call-with-raw-strings)
                              (and (stringp hook)
                                   (string-equal (string-trim '(#\Space #\Tab) hook)
                                                 "zk:call-with-raw-strings"))))))
                (asdf/component:sub-components system :type 'asdf:cl-source-file))
        (mapcar #'symbol-name *features*))))"#;

const EVALUATE_EXAMPLE: &str = r#"
#'(lambda (source package raw-strings-p)
    (let ((*package* (or (find-package package) (error "Package ~S does not exist" package)))
          (*readtable* (if raw-strings-p (zk:make-raw-strings-readtable) (copy-readtable nil))))
      (zk::%eval-doc-example source)))"#;

pub(super) fn test_system(
    system: &str,
    heap_size: Option<usize>,
) -> Result<ExitCode, Box<dyn Error>> {
    let runtime = super::asdf_runtime(heap_size)?;
    let load_system = runtime.eval(LOAD_SYSTEM)?;
    let evaluate_example = prepare_examples(&runtime)?;

    let name = runtime.make_string(system)?;
    let loaded = runtime.call(&load_system[0], &[&name])?;
    let [directory, components, features] = loaded.as_slice() else {
        return Err("expected ASDF's system directory, components and features".into());
    };
    let features = list_items(&runtime, features)
        .filter_map(|feature| runtime.string(&feature))
        .collect();
    let mut workspace = SourceWorkspace::new();
    workspace.set_features(Some(features));
    let mut runner = DocTestRunner {
        runtime: &runtime,
        function: &evaluate_example[0],
        passed: 0,
        failed: 0,
        diagnostics: 0,
    };
    for component in list_items(&runtime, components) {
        let (path, raw) = runtime
            .cons_fields(&component)
            .expect("component and readtable pair");
        let path = PathBuf::from(runtime.string(&path).expect("component namestring"));
        let source = read_source(&path)?;
        workspace.set_document("example-source", source);
        workspace.set_initial_package("example-source", Some("COMMON-LISP-USER".into()));
        let readtable = if raw.is_nil() {
            SourceReadtable::Standard
        } else {
            SourceReadtable::RawStrings
        };
        workspace.set_initial_readtable("example-source", readtable);
        let snapshot = workspace
            .snapshot("example-source")
            .expect("source just set");
        runner.check(&path, &snapshot, snapshot.documentation_tests())?;
    }
    if let Some(directory) = runtime.string(directory) {
        let readme = PathBuf::from(directory).join("README.md");
        match read_source(&readme) {
            Ok(source) => {
                workspace.set_document("example-source", source);
                let snapshot = workspace
                    .snapshot("example-source")
                    .expect("source just set");
                let tests =
                    DocumentationTests::markdown(snapshot.text(), SourceReadtable::Standard);
                runner.check(&readme, &snapshot, &tests)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let DocTestRunner {
        passed,
        failed,
        diagnostics,
        ..
    } = runner;
    println!(
        "Doc tests for {system}: {passed} passed, {failed} failed, {diagnostics} errors in examples"
    );
    Ok(if failed == 0 && diagnostics == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

struct DocTestRunner<'rt> {
    runtime: &'rt Runtime,
    function: &'rt Root<'rt>,
    passed: usize,
    failed: usize,
    diagnostics: usize,
}

impl DocTestRunner<'_> {
    fn check(
        &mut self,
        path: &Path,
        snapshot: &SourceSnapshot,
        tests: &DocumentationTests,
    ) -> Result<(), Box<dyn Error>> {
        for diagnostic in &tests.diagnostics {
            self.diagnostics += 1;
            report(
                path,
                snapshot,
                diagnostic.range.start().into(),
                diagnostic.message,
            );
        }
        for example in &tests.examples {
            match self.evaluate(example)? {
                None => self.passed += 1,
                Some((offset, message)) => {
                    self.failed += 1;
                    report(path, snapshot, offset, &message);
                }
            }
        }
        Ok(())
    }

    fn evaluate(
        &self,
        example: &DocumentationExample,
    ) -> Result<Option<(usize, String)>, Box<dyn Error>> {
        let fence = usize::from(example.range.start());
        let Some(package) = &example.package else {
            return Ok(Some((
                fence,
                "cannot determine the package for this example".into(),
            )));
        };
        let package = self.runtime.make_string(package)?;
        let source = self.runtime.make_string(example.source())?;
        let raw = if example.readtable == SourceReadtable::RawStrings {
            self.runtime.t()
        } else {
            self.runtime.nil()
        };
        let values = match self.runtime.call(self.function, &[&source, &package, &raw]) {
            Ok(values) => values,
            Err(error) => return Ok(Some((fence, error.message().into()))),
        };
        let [] = values.as_slice() else {
            let [message, offset] = values.as_slice() else {
                return Err("expected a documentation example error and offset".into());
            };
            let offset = offset.as_fixnum().expect("error offset") as usize;
            let offset = example
                .source_range(offset..offset)
                .map_or(fence, |range| range.start().into());
            return Ok(Some((
                offset,
                self.runtime.string(message).expect("error message"),
            )));
        };
        Ok(None)
    }
}

fn list_items<'rt>(runtime: &'rt Runtime, list: &Root<'rt>) -> impl Iterator<Item = Root<'rt>> {
    let mut next = runtime.cons_fields(list);
    std::iter::from_fn(move || {
        let (item, tail) = next.take()?;
        next = runtime.cons_fields(&tail);
        Some(item)
    })
}

fn read_source(path: &Path) -> io::Result<String> {
    fs::read_to_string(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

fn report(path: &Path, snapshot: &SourceSnapshot, offset: usize, message: &str) {
    let position = snapshot.position(offset).expect("source location");
    eprintln!(
        "{}:{}:{}: error[doc/example]: {message}",
        path.display(),
        position.line + 1,
        position.column + 1
    );
}

fn prepare_examples(runtime: &Runtime) -> Result<Vec<Root<'_>>, Box<dyn Error>> {
    runtime.register_function("zk::%eval-doc-example", |runtime, arguments| {
        let source = arguments
            .first()
            .and_then(|arg| runtime.string(arg))
            .ok_or("expected the documentation example source string")?;
        match runtime.load_str(&source) {
            Ok(()) => Ok(Vec::new()),
            Err(error) => Ok(vec![
                runtime.make_string(error.message())?,
                runtime
                    .fixnum(error.offset() as isize)
                    .ok_or("documentation error offset is too large")?,
            ]),
        }
    })?;
    runtime.eval(EVALUATE_EXAMPLE).map_err(Into::into)
}

pub(super) fn test_project(
    runtime: &Runtime,
    name: &str,
    root: &Path,
    files: &[PathBuf],
) -> Result<ExitCode, Box<dyn Error>> {
    let evaluate = prepare_examples(runtime)?;
    let features = runtime.eval("(mapcar #'symbol-name *features*)")?;
    let mut workspace = SourceWorkspace::new();
    workspace.set_features(Some(
        list_items(runtime, &features[0])
            .filter_map(|feature| runtime.string(&feature))
            .collect(),
    ));
    let mut runner = DocTestRunner {
        runtime,
        function: &evaluate[0],
        passed: 0,
        failed: 0,
        diagnostics: 0,
    };
    for path in files {
        workspace.set_document("example-source", read_source(path)?);
        workspace.set_initial_package("example-source", Some("COMMON-LISP-USER".into()));
        workspace.set_initial_readtable("example-source", SourceReadtable::Standard);
        let snapshot = workspace
            .snapshot("example-source")
            .expect("source just set");
        runner.check(path, &snapshot, snapshot.documentation_tests())?;
    }
    let readme = root.join("README.md");
    match read_source(&readme) {
        Ok(source) => {
            workspace.set_document("example-source", source);
            let snapshot = workspace
                .snapshot("example-source")
                .expect("source just set");
            runner.check(
                &readme,
                &snapshot,
                &DocumentationTests::markdown(snapshot.text(), SourceReadtable::Standard),
            )?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    println!(
        "Doc tests for {name}: {} passed, {} failed, {} errors in examples",
        runner.passed, runner.failed, runner.diagnostics
    );
    Ok(if runner.failed == 0 && runner.diagnostics == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
