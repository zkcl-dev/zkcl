use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn pathname_components_survive_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-pathname", |runtime, _| {
            runtime.collect();
            Ok(vec![runtime.nil()])
        })
        .unwrap();

    let values = runtime
        .eval(
            r#"(let ((p (make-pathname :directory '(:absolute "tmp" "项目") :name "Report" :type "lisp")))
                 (collect-pathname)
                 (values (pathnamep p) (typep p 'pathname) (eq p (pathname p))
                         (pathname-name p) (pathname-type p :case :common)
                         (pathname-directory p) (namestring p)))"#,
        )
        .unwrap();
    assert!(values[..3].iter().all(|value| !value.is_nil()));
    assert_eq!(runtime.string(&values[3]).as_deref(), Some("Report"));
    assert_eq!(runtime.string(&values[4]).as_deref(), Some("LISP"));
    let (marker, tail) = runtime.cons_fields(&values[5]).unwrap();
    let (first, tail) = runtime.cons_fields(&tail).unwrap();
    let (second, tail) = runtime.cons_fields(&tail).unwrap();
    assert_eq!(runtime.symbol_name(&marker).as_deref(), Some("ABSOLUTE"));
    assert_eq!(runtime.string(&first).as_deref(), Some("tmp"));
    assert_eq!(runtime.string(&second).as_deref(), Some("项目"));
    assert!(tail.is_nil());
    assert_eq!(
        runtime.string(&values[6]).as_deref(),
        Some("/tmp/项目/Report.lisp")
    );
}

#[test]
fn parse_namestring_uses_character_indices() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(multiple-value-bind (path pos)
                 (parse-namestring "前ab/测.def.trailing后" nil *default-pathname-defaults* :start 1 :end 9)
               (values (namestring path) pos))"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("ab/测.def"));
    assert_eq!(values[1].as_fixnum(), Some(9));
}

#[test]
fn pathname_defaults_resolve_relative_compile_targets() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(let ((*default-pathname-defaults* #P"/work/src/base.lisp"))
                 (values
                   (namestring (merge-pathnames "foo/bar"))
                   (namestring (compile-file-pathname "app.lisp" :verbose t))
                   (namestring (compile-file-pathname "app.lisp" :output-file "obj/out"))
                   (namestring (compile-file-pathname "app.lisp" :output-file "result.custom"))
                   (namestring (make-pathname :name "new" :type nil :defaults "a.lisp"))
                   (pathname-version (merge-pathnames (make-pathname :name "new") (make-pathname :version 7) nil))
                   (equal #P"/tmp/字.v1.lisp" (pathname "/tmp/字.v1.lisp"))
                   (equalp #P"/tmp/X.lisp" #P"/tmp/x.lisp")
                   (namestring #P"/tmp/字.v1.lisp")
                   (wild-pathname-p #P"/tmp/**/*.lisp" :directory)
                   (wild-pathname-p #P"/tmp/\\*.lisp")))"#,
        )
        .unwrap();
    for (value, expected) in values.iter().zip([
        "/work/src/foo/bar.lisp",
        "/work/src/app.zkclfasl",
        "/work/src/obj/out.zkclfasl",
        "/work/src/result.custom",
        "new",
    ]) {
        assert_eq!(runtime.string(value).as_deref(), Some(expected));
    }
    assert!(values[5].is_nil());
    assert!(!values[6].is_nil());
    assert!(values[7].is_nil());
    assert_eq!(
        runtime.string(&values[8]).as_deref(),
        Some("/tmp/字.v1.lisp")
    );
    assert!(!values[9].is_nil());
    assert!(values[10].is_nil());
}

#[test]
fn merge_pathnames_resolves_relative_directory_components() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(let ((relative (make-pathname :directory '(:relative "down")
                                                :defaults #P"/root/parent/base.lisp")))
                  (values (namestring relative)
                          (namestring (merge-pathnames relative #P"/other/"))))"#,
        )
        .unwrap();
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some("down/base.lisp")
    );
    assert_eq!(
        runtime.string(&values[1]).as_deref(),
        Some("/other/down/base.lisp")
    );

    let values = runtime
        .eval(
            r#"(namestring (merge-pathnames "later/item.lisp"
                                            (make-pathname :directory '(:absolute "base" :wild :back "temp" :back))))"#,
        )
        .unwrap();
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some("/base/later/item.lisp")
    );
}

#[test]
fn pathname_conversion_reports_invalid_inputs() {
    let runtime = Runtime::start().unwrap();
    let error = runtime.eval("(pathname 12)").err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::TypeError)
    );
    let error = runtime.eval("#3P\"file\"").err().unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Read);
    let values = runtime.eval("#-zkcl #3P(\"ignored\") 42").unwrap();
    assert_eq!(values[0].as_fixnum(), Some(42));
}

#[test]
fn pathname_namestring_survives_uninterning_its_directory_marker() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(let ((path #P"/work/\\./x.lisp"))
                 (unintern :absolute :keyword)
                 (namestring path))"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("/work/x.lisp"));
}
