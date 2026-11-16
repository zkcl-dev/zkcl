mod common;

use zkcl::{EvalErrorKind, Runtime};

#[test]
fn optional_macro_defaults_see_outer_bindings_before_special_binding() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(setq shadowed 99 free-in-default 66)
             (let ((shadowed 7) (free-in-default 13))
               (defmacro defaults (&optional (early shadowed)
                                              (from-free free-in-default)
                                              (shadowed 17))
                 (declare (special shadowed free-in-default))
                 (list 'values early from-free
                               (symbol-value 'shadowed) free-in-default)))",
        )
        .unwrap();
    let values = runtime.eval("(defaults)").unwrap();
    let values: Vec<_> = values.iter().map(|value| value.as_fixnum()).collect();
    assert_eq!(values, [Some(7), Some(13), Some(17), Some(66)]);
}

#[test]
fn optional_macro_defaults_see_special_expander_bindings() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval(
            "(defmacro wrapper-default
                 (&whole whole
                  &optional (result (if (macro-function 'present (symbol-value 'env))
                                        (car (symbol-value 'whole))))
                  &environment env)
               (declare (special whole env))
               (list 'quote result))",
        )
        .unwrap();
    let value = runtime
        .eval("(macrolet ((present () nil)) (wrapper-default))")
        .unwrap();
    assert_eq!(
        runtime.symbol_name(&value[0]).as_deref(),
        Some("WRAPPER-DEFAULT")
    );
}

#[test]
fn misplaced_whole_reports_the_lambda_list_location() {
    let runtime = Runtime::start().unwrap();
    let malformed = "(defmacro misplaced-whole (&environment env &whole form) 0)";
    let error = runtime.eval(malformed).err().expect("&WHOLE must be first");
    assert_eq!(error.kind(), EvalErrorKind::Compile);
    assert_eq!(error.offset(), malformed.find("&whole").unwrap());
}

#[test]
fn custom_declarations_follow_lexical_environments_and_survive_file_compilation() {
    let dir = common::files::TempDir::new("declaration-environment");
    let source = dir.write(
        "declarations.lisp",
        r#"
        (zk:define-declaration annotation (spec env)
          (values :declare
                  (cons 'annotation (append (zk:declaration-information 'annotation env)
                                             (cdr spec)))))
        (zk:define-declaration policy-note (spec env)
          (declare (ignore spec))
          (values :declare (cons 'policy-note (second (assoc 'safety (zk:declaration-information 'optimize env))))))
        (defmacro seen-policy (&environment env) (zk:declaration-information 'policy-note env))
        (defmacro seen (&environment env)
          (list 'quote (zk:declaration-information 'annotation env)))
        (defmacro optimize-safety (&environment env)
          (second (assoc 'safety (zk:declaration-information 'optimize env))))
        (defun read-annotations ()
          (declare (annotation a) (annotation b))
          (list (seen)
                (locally (declare (annotation c)) (seen))
                (funcall (lambda () (seen)))
                (let ((value (seen))) (declare (annotation d)) (list value (seen)))
                (eval '(seen))
                (locally (declare (optimize (safety 0))) (optimize-safety))))
    "#,
    );
    let runtime = Runtime::start().unwrap();
    let fasl = runtime.compile_file(&source, None).unwrap();
    drop(runtime);
    let runtime = Runtime::start().unwrap();
    runtime.load_file(&fasl).unwrap();
    runtime
        .eval("(assert (equal (read-annotations) '((a b) (a b c) (a b) ((a b) (a b d)) nil 0)))")
        .unwrap();
    runtime.eval("(assert (equal (eval '(locally (declare (annotation e)) (list (seen) (eval '(seen))))) '((e) nil)))").unwrap();
    runtime
        .eval("(assert (member 'annotation (zk:declaration-information 'declaration)))")
        .unwrap();
    runtime
        .eval("(assert (null (zk:declaration-information 'annotation)))")
        .unwrap();
    runtime.eval("(assert (equal (locally (declare (optimize (safety 1)))
                           (list (locally (declare (optimize (safety 0)) (policy-note)) (seen-policy))
                                 (locally (declare (policy-note) (optimize (safety 0))) (seen-policy))))
                         '(0 1)))").unwrap();
    assert!(
        runtime
            .eval("(zk:declaration-information 'unknown-declaration)")
            .is_err()
    );
}
