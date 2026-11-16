#![cfg(unix)]

use std::error::Error;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use zkcl::{ConditionClass, EvalErrorKind, Root, Runtime};

mod common;

use common::files::TempDir;
use common::lisp::lisp_string;

fn namestring(runtime: &Runtime, pathname: &Root<'_>) -> String {
    let function = runtime.eval("#'namestring").unwrap().pop().unwrap();
    let values = runtime.call(&function, &[pathname]).unwrap();
    runtime.string(&values[0]).unwrap()
}

fn directory_namestrings(runtime: &Runtime, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut remaining = runtime.eval(source).unwrap().pop().unwrap();
    while let Some((first, rest)) = runtime.cons_fields(&remaining) {
        names.push(namestring(runtime, &first));
        remaining = rest;
    }
    assert!(remaining.is_nil());
    names
}

#[test]
fn relative_file_operations_resolve_against_default_pathname_defaults() {
    let temp_dir = TempDir::new("filesystem");
    let base = temp_dir.path().canonicalize().unwrap();
    let nested = base.join("nested");
    fs::create_dir(&nested).unwrap();
    for (dir, file) in [
        (&base, "alpha.lisp"),
        (&base, "alpha.txt"),
        (&base, ".hidden.lisp"),
        (&base, "literal*.lisp"),
        (&nested, "leaf.lisp"),
    ] {
        fs::write(dir.join(file), b"test").unwrap();
    }
    symlink("alpha.lisp", base.join("link.lisp")).unwrap();
    symlink("missing", base.join("dangling.lisp")).unwrap();
    symlink("nested", base.join("link-to-nested")).unwrap();
    symlink("..", nested.join("back-to-root")).unwrap();

    let runtime = Runtime::start().unwrap();
    let base_designator = lisp_string(&format!("{}/", base.display()));
    let relative = |form: &str| {
        format!("(let ((*default-pathname-defaults* (pathname {base_designator}))) {form})")
    };

    let alpha = base.join("alpha.lisp").to_str().unwrap().to_owned();
    fs::OpenOptions::new()
        .write(true)
        .open(&alpha)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(42))
        .unwrap();
    let date = runtime
        .eval(&relative(r#"(file-write-date "link.lisp")"#))
        .unwrap();
    assert_eq!(date[0].as_fixnum(), Some(2_208_988_842));
    let date_errors = runtime
        .eval(&relative(
            r#"(values (handler-case (file-write-date "missing.lisp") (file-error () t))
                       (handler-case (file-write-date "*.lisp") (file-error () t)))"#,
        ))
        .unwrap();
    assert_eq!(date_errors.len(), 2);
    assert!(date_errors.iter().all(|value| !value.is_nil()));
    let found = runtime
        .eval(&relative(r#"(probe-file "alpha.lisp")"#))
        .unwrap();
    assert_eq!(namestring(&runtime, &found[0]), alpha);
    let absent = runtime
        .eval(&relative(
            r#"(values (probe-file "missing.lisp")
                                  (probe-file "alpha.lisp/missing.lisp"))"#,
        ))
        .unwrap();
    assert_eq!(absent.len(), 2);
    assert!(absent.iter().all(Root::is_nil));

    let error = runtime
        .eval(&relative(r#"(truename "missing.lisp")"#))
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::FileError)
    );
    let os_error = error
        .source()
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .unwrap();
    assert_eq!(os_error.kind(), std::io::ErrorKind::NotFound);
    let missing = runtime
        .eval(&relative(
            r#"(handler-case (truename "missing.lisp")
                 (file-error (condition) (file-error-pathname condition)))"#,
        ))
        .unwrap();
    assert_eq!(
        namestring(&runtime, &missing[0]),
        base.join("missing.lisp").to_str().unwrap()
    );
    let wild = runtime
        .eval(&relative(
            r#"(handler-case (probe-file "*.lisp")
                 (file-error (condition) (namestring (file-error-pathname condition))))"#,
        ))
        .unwrap();
    assert_eq!(
        runtime.string(&wild[0]).as_deref(),
        base.join("*.lisp").to_str()
    );

    let mut expected = vec![
        base.join(".hidden.lisp").to_str().unwrap().to_owned(),
        alpha.clone(),
        base.join("literal*.lisp").to_str().unwrap().to_owned(),
    ];
    let glob = relative(r#"(directory (make-pathname :name :wild :type "lisp"))"#);
    assert_eq!(directory_namestrings(&runtime, &glob), expected);
    expected.push(nested.join("leaf.lisp").to_str().unwrap().to_owned());
    let glob = relative(r#"(directory "**/*.lisp" :unknown-key nil :allow-other-keys t)"#);
    assert_eq!(directory_namestrings(&runtime, &glob), expected);
    assert_eq!(
        directory_namestrings(&runtime, &relative(r#"(directory "nested/../**/*.lisp")"#)),
        expected
    );
    assert_eq!(
        directory_namestrings(
            &runtime,
            &relative(
                r#"(directory (make-pathname
                      :directory (append (pathname-directory *default-pathname-defaults*)
                                         '("nested" "back-to-root" :back))
                      :name "leaf" :type "lisp"))"#,
            ),
        ),
        [nested.join("leaf.lisp").to_str().unwrap()]
    );
    assert!(
        directory_namestrings(&runtime, &relative(r#"(directory "missing/*.lisp")"#)).is_empty()
    );
    assert!(
        directory_namestrings(&runtime, &relative(r#"(directory "alpha.lisp/**/")"#)).is_empty()
    );
    assert!(directory_namestrings(&runtime, &relative(r#"(directory "*/leaf.lisp/")"#)).is_empty());
    assert_eq!(
        directory_namestrings(&runtime, &relative(r#"(directory "*/")"#)),
        [format!("{}/", nested.display())]
    );
    assert_eq!(
        directory_namestrings(
            &runtime,
            &relative(r#"(directory (make-pathname :name "alpha" :type "lisp" :version :wild))"#),
        ),
        std::slice::from_ref(&alpha)
    );
    let bad_keyword = runtime
        .eval(&relative(r#"(directory "alpha.lisp" :unknown-key t)"#))
        .err()
        .unwrap();
    assert_eq!(
        bad_keyword.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ProgramError)
    );

    let exact = runtime
        .eval(&relative(r#"(truename "link.lisp")"#))
        .unwrap();
    assert_eq!(namestring(&runtime, &exact[0]), alpha);
    runtime
        .eval(&relative(r#"(delete-file "link.lisp")"#))
        .unwrap();
    assert!(base.join("alpha.lisp").is_file());
    assert!(fs::symlink_metadata(base.join("link.lisp")).is_err());
    let removed = runtime
        .eval(&relative(r#"(delete-file "alpha.lisp")"#))
        .unwrap();
    assert!(!removed[0].is_nil());
    assert!(!Path::new(&alpha).exists());
    let refused = runtime
        .eval(&relative(r#"(delete-file "*.lisp")"#))
        .err()
        .unwrap();
    assert_eq!(
        refused.kind(),
        EvalErrorKind::LispCondition(ConditionClass::FileError)
    );
    let directory_error = runtime
        .eval(&relative(r#"(delete-file "nested/")"#))
        .err()
        .unwrap();
    assert_eq!(
        directory_error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::FileError)
    );
    assert!(base.join("literal*.lisp").is_file());
    assert!(nested.join("leaf.lisp").is_file());
}

#[test]
fn rename_file_reports_the_truenames_of_a_new_directory_tree() {
    let temp_dir = TempDir::new("filesystem");
    let base = temp_dir.path().canonicalize().unwrap();
    let runtime = Runtime::start().unwrap();
    let base_designator = lisp_string(&format!("{}/", base.display()));
    let relative = |form: &str| {
        format!("(let ((*default-pathname-defaults* (pathname {base_designator}))) {form})")
    };
    fs::write(base.join("regular-file"), "contents").unwrap();

    let values = runtime
        .eval(&relative(
            r#"(let ((designator (copy-seq "existing.dir/child/*.lisp")))
                  (multiple-value-bind (result created) (ensure-directories-exist designator)
                    (multiple-value-bind (again recreated) (ensure-directories-exist designator)
                      (values (eq result designator) created (eq again designator) (not recreated)
                              (handler-case (ensure-directories-exist "regular-file/child")
                                (file-error () t))))))"#,
        ))
        .unwrap();
    assert!(values.iter().all(|value| !value.is_nil()));
    assert!(base.join("existing.dir/child").is_dir());
    let values = runtime
        .eval(&relative(
            r#"(let ((designator #P"existing.dir/child/new.dir/"))
                  (multiple-value-bind (result created) (ensure-directories-exist designator)
                    (values (eq result designator) created)))"#,
        ))
        .unwrap();
    assert!(values.iter().all(|value| !value.is_nil()));
    assert!(base.join("existing.dir/child/new.dir").is_dir());

    let values = runtime
        .eval(&relative(
            r#"(rename-file "existing.dir/" (merge-pathnames "renamed.dir/"))"#,
        ))
        .unwrap();
    assert_eq!(values.len(), 3);
    for (pathname, directory) in values
        .iter()
        .zip(["renamed.dir", "existing.dir", "renamed.dir"])
    {
        assert_eq!(
            namestring(&runtime, pathname),
            format!("{}/{directory}/", base.display())
        );
    }
    assert!(base.join("renamed.dir/child/new.dir").is_dir());
}

#[test]
fn closing_a_file_stream_preserves_its_pathname_designator() {
    let temp_dir = TempDir::new("filesystem");
    let path = temp_dir.path().canonicalize().unwrap().join("source");
    fs::write(&path, "nil").unwrap();
    let runtime = Runtime::start().unwrap();
    let path = lisp_string(path.to_str().unwrap());

    let values = runtime
        .eval(&format!(
            r#"(let* ((stream (open {path})) (path (pathname stream)))
                 (multiple-value-bind (before position) (parse-namestring stream nil nil :start 5)
                   (close stream)
                   (multiple-value-bind (after closed-position) (parse-namestring stream)
                     (let ((*default-pathname-defaults* #P"changes-default-type.lisp"))
                       (values (eq before path) (eq before after) position closed-position
                               (probe-file stream)
                               (truename stream)
                               (eq (ensure-directories-exist stream) stream))))))"#,
        ))
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    assert_eq!(values[2].as_fixnum(), Some(5));
    assert_eq!(values[3].as_fixnum(), Some(0));
    assert_eq!(
        namestring(&runtime, &values[4]),
        temp_dir
            .path()
            .canonicalize()
            .unwrap()
            .join("source")
            .display()
            .to_string()
    );
    assert_eq!(
        namestring(&runtime, &values[4]),
        namestring(&runtime, &values[5])
    );
    assert!(!values[6].is_nil());
}
