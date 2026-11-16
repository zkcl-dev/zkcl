use zkcl::Runtime;

fn check(runtime: &Runtime, source: &str) {
    let values = runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"));
    assert_eq!(values.len(), 1, "{source}");
    assert!(!values[0].is_nil(), "{source}");
}

#[test]
fn raw_strings_are_scoped_to_the_readtable() {
    let runtime = Runtime::start().unwrap();
    assert!(!runtime.raw_strings_enabled());
    assert!(runtime.eval("#>text").is_err());
    runtime.eval("(zk:in-readtable :raw-strings)").unwrap();
    assert!(runtime.raw_strings_enabled());

    let values = runtime
        .eval(concat!(
            "(values\n#>(defun greet (name)\r\n",
            "  #>  (format nil \"Hi, ~A\" name))\r\n",
            "\t#>\r\n#>(greet #\\A)  \n",
            "#| Standard #| nested |# comment |#\n",
            "#>broken ( \" \\ 雪😀\n",
            "#+(or) #37>skipped )\n #>skipped \"\n",
            "#x2a)"
        ))
        .unwrap();
    let expected = "(defun greet (name)\n  (format nil \"Hi, ~A\" name))\n\n(greet #\\A)  ";
    assert_eq!(values.len(), 3);
    assert_eq!(runtime.string(&values[0]).as_deref(), Some(expected));
    assert_eq!(
        runtime.string(&values[1]).as_deref(),
        Some("broken ( \" \\ 雪😀")
    );
    assert_eq!(values[2].as_fixnum(), Some(42));

    check(
        &runtime,
        r##"(with-input-from-string (stream (format nil "#>first~%#|comment|# #x2a"))
              (and (string= "first" (read stream)) (= 42 (read stream))))"##,
    );
    assert_eq!(
        runtime.string(&runtime.eval("#>").unwrap()[0]).as_deref(),
        Some("")
    );
    runtime.eval("(zk:in-readtable :standard)").unwrap();
    assert!(!runtime.raw_strings_enabled());
    assert!(runtime.eval("#>text").is_err());
}

#[test]
fn text_reader_uses_bindings_changed_by_read_time_eval() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval("(defvar *saved-readtable* *readtable*)")
        .unwrap();
    let values = runtime
        .eval(
            "'(#.(progn (setq *readtable* (copy-readtable nil))
                        (setf (readtable-case *readtable*) :preserve)
                        :changed)
                 AfterSwitch)",
        )
        .unwrap();
    runtime
        .eval("(SETQ *READTABLE* *SAVED-READTABLE*)")
        .unwrap();
    assert_eq!(
        runtime.repr(&values[0]).unwrap(),
        "(:CHANGED |AfterSwitch|)"
    );

    runtime
        .eval("(setq *readtable* (copy-readtable nil))")
        .unwrap();
    let values = runtime
        .eval("'(Before #.(setf (readtable-case *readtable*) :preserve) AfterMutation)")
        .unwrap();
    runtime
        .eval("(SETQ *READTABLE* *SAVED-READTABLE*)")
        .unwrap();
    assert_eq!(
        runtime.repr(&values[0]).unwrap(),
        "(BEFORE :PRESERVE |AfterMutation|)"
    );

    let values = runtime.eval("'(10 #.(setq *read-base* 16) 10)").unwrap();
    runtime.eval("(SETQ *READ-BASE* #10R10)").unwrap();
    assert_eq!(runtime.repr(&values[0]).unwrap(), "(10 16 16)");

    runtime
        .eval("(defpackage :read-binding-test (:use :cl))")
        .unwrap();
    let values = runtime
        .eval("'(before #.(progn (setq *package* (find-package :read-binding-test)) nil) after)")
        .unwrap();
    runtime.eval("(in-package :cl-user)").unwrap();
    assert_eq!(
        runtime.repr(&values[0]).unwrap(),
        "(BEFORE NIL READ-BINDING-TEST::AFTER)"
    );
}

#[test]
fn copy_readtable_preserves_the_destination_identity() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        "(and (readtablep *readtable*)
              (not (readtablep nil))
              (not (readtablep (list :upcase)))
              (typep *readtable* 'readtable)
              (eq 'readtable (type-of *readtable*))
              (eq :upcase (readtable-case *readtable*)))",
    );
    check(
        &runtime,
        "(let* ((old *readtable*)
                (*readtable* (copy-readtable))
                (fresh (copy-readtable nil nil)))
           (and (not (eq old *readtable*))
                (eq :downcase (setf (readtable-case *readtable*) :downcase))
                (eq :upcase (readtable-case old))
                (eq :downcase (readtable-case (copy-readtable)))
                (eq :upcase (readtable-case (copy-readtable nil)))
                (eq fresh (copy-readtable *readtable* fresh))
                (eq :downcase (readtable-case fresh))
                (eq fresh (copy-readtable fresh fresh))
                (eq fresh (copy-readtable nil fresh))
                (eq :upcase (readtable-case fresh))))",
    );
    check(&runtime, "(eq (read-from-string \"mIxEd\") 'mixed)");
}

#[test]
fn readtable_case_transforms_unescaped_symbol_characters() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        r##"(let ((*readtable* (copy-readtable nil)))
             (setf (readtable-case *readtable*) :downcase)
             (and (string= (symbol-name (read-from-string "ShEll")) "shell")
                  (string= (symbol-name (read-from-string "|SH|e\\Ll")) "SHeLl")
                  (eq (read-from-string "|CL|:|CAR|") 'cl:car)
                  (string= (symbol-name (read-from-string ":UpCaSe")) "upcase")
                  (string= (symbol-name (read-from-string "#:BiG")) "big")
                  (eq (symbol-package (read-from-string "#:BiG")) nil)
                  (= (read-from-string "#XFE") 254)))"##,
    );
    check(
        &runtime,
        r##"(let ((*readtable* (copy-readtable nil)))
             (funcall #'(setf readtable-case) :preserve *readtable*)
             (and (string= (symbol-name (read-from-string "SheLl")) "SheLl")
                  (string= (symbol-name (read-from-string "#:Sh\\ELL")) "ShELL")
                  (eq (read-from-string "CL:CAR") 'cl:car)
                  (not (eq (read-from-string "car") 'cl:car))
                  (not (eq (read-from-string ":downcase") :downcase))))"##,
    );
    check(
        &runtime,
        r##"(let ((*readtable* (copy-readtable nil)))
             (setf (readtable-case *readtable*) :invert)
             (and (string= (symbol-name (read-from-string "shell")) "SHELL")
                  (string= (symbol-name (read-from-string "SHELL")) "shell")
                  (string= (symbol-name (read-from-string "ShEll")) "ShEll")
                  (string= (symbol-name (read-from-string "|s|hell")) "sHELL")
                  (string= (symbol-name (read-from-string "#:SH\\eLL")) "shell")
                  (eq (read-from-string "cl:car") 'cl:car)
                  (eq (read-from-string "cl:|CAR|") 'cl:car)
                  (eq (read-from-string ":downcase") :downcase)))"##,
    );
    check(
        &runtime,
        r#"(defpackage "RD-FISH" (:use) (:export "fish" "FISH"))"#,
    );
    check(
        &runtime,
        r#"(let ((*readtable* (copy-readtable nil)))
             (setf (readtable-case *readtable*) :invert)
             (and (eq (read-from-string "rd-fish:fish") 'rd-fish:fish)
                  (eq (read-from-string "rd-fish:FISH") 'rd-fish:|fish|)
                  (string= (prin1-to-string 'rd-fish:|fish|) "rd-fish:FISH")
                  (eq (read-from-string (prin1-to-string 'rd-fish:|fish|))
                      'rd-fish:|fish|)))"#,
    );
}

#[test]
fn symbol_printing_roundtrips_each_readtable_case() {
    let runtime = Runtime::start().unwrap();
    for (case, printed) in [
        (":upcase", "(ZEBRA |zebra| |ZeBra|)"),
        (":downcase", "(|ZEBRA| ZEBRA |ZeBra|)"),
        (":preserve", "(ZEBRA zebra ZeBra)"),
        (":invert", "(zebra ZEBRA ZeBra)"),
    ] {
        check(
            &runtime,
            &format!(
                r#"(let ((*readtable* (copy-readtable nil)) (*print-case* :upcase)
                     (symbols '(zebra |zebra| |ZeBra|)))
                 (setf (readtable-case *readtable*) {case})
                 (and (string= (prin1-to-string symbols) "{printed}")
                      (equal symbols (read-from-string (prin1-to-string symbols)))))"#,
            ),
        );
    }
    check(
        &runtime,
        r#"(let* ((symbols '(zebra |zebra| |ZeBra|))
                  (*readtable* (copy-readtable nil))
                  (*print-readably* t))
             (setf (readtable-case *readtable*) :invert)
             (let ((printed (prin1-to-string symbols)))
               (let ((*readtable* (copy-readtable nil)))
                 (equal symbols (read-from-string printed)))))"#,
    );
}

#[test]
fn readtable_operations_report_bad_designators() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        "(let ((table (copy-readtable nil)))
           (and (handler-case (readtable-case nil)
                  (type-error (c) (eq (type-error-expected-type c) 'readtable)))
                (handler-case (copy-readtable table 1)
                  (type-error (c) (= (type-error-datum c) 1)))
                (handler-case (setf (readtable-case table) 'upcase)
                  (type-error (c) (eq (type-error-datum c) 'upcase)))
                (eq (readtable-case table) :upcase)))",
    );
}

#[test]
fn readtable_case_survives_collection() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        "(progn
           (defparameter *kept-readtable* (copy-readtable))
           (setf (readtable-case *kept-readtable*) :preserve)
           (readtablep *kept-readtable*))",
    );
    runtime.collect();
    check(
        &runtime,
        r#"(let ((*readtable* *kept-readtable*))
             (and (eq :preserve (readtable-case *readtable*))
                  (string= (symbol-name (read-from-string "Quail")) "Quail")))"#,
    );
}
