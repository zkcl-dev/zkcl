use zkcl::Runtime;
use zkcl_bytecode::{CompiledFile, Instruction, LiteralNode};

mod common;

use common::lisp::eval_reprs;

const DECLARATION_SCOPE_SETUP: &str = r#"
    (defvar *expansions* nil)
    (defun scoped-target (x) x)
    (defun (setf scoped-target) (x object) (declare (ignore object)) x)
    (define-compiler-macro scoped-target (&whole form &rest args)
      (declare (ignore args)) (push :ordinary *expansions*) form)
    (define-compiler-macro (setf scoped-target) (&whole form &rest args)
      (declare (ignore args)) (push :setter *expansions*) form)
    (defun compile-expansions (definition)
      (let ((*expansions* nil)) (compile nil definition) (reverse *expansions*)))
    (defmacro visible-compiler-macros (&environment env)
      (list 'quote
            (mapcar (lambda (name) (not (null (compiler-macro-function name env))))
                    '(scoped-target (setf scoped-target)))))
"#;

#[test]
fn compiler_macro_visibility_follows_the_lexical_function_name() {
    let runtime = Runtime::start().unwrap();
    runtime.load_str(DECLARATION_SCOPE_SETUP).unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"(values
              (compile-expansions '(lambda (x)
                (locally (declare (notinline scoped-target))
                  (unwind-protect (scoped-target x)))))
              (compile-expansions '(lambda (x)
                (let () (declare (notinline (setf scoped-target)))
                  (unwind-protect
                      (values (scoped-target x) (funcall #'(setf scoped-target) x nil))))))
              (compile-expansions '(lambda (x)
                (locally (declare (notinline scoped-target))
                  (unwind-protect
                      (values (scoped-target x) (funcall #'(setf scoped-target) x nil))))))
              (compile-expansions '(lambda (x)
                (locally (declare (notinline (setf scoped-target)))
                  (unwind-protect (funcall #'(setf scoped-target) x nil)))
                (unwind-protect (funcall #'(setf scoped-target) x nil))))
              (compile-expansions '(lambda (x)
                (flet ((scoped-target (x) (scoped-target x)))
                  (declare (notinline scoped-target))
                  (unwind-protect (scoped-target x)))))
              (flet (((setf scoped-target) (x y) (declare (ignore y)) x))
                (unwind-protect (visible-compiler-macros)))
              (labels ((scoped-target (x) x))
                (unwind-protect (visible-compiler-macros)))
              (macrolet ((scoped-target (x) x))
                (unwind-protect (visible-compiler-macros))))"#,
        ),
        [
            "NIL",
            "(:ORDINARY)",
            "(:SETTER)",
            "(:SETTER)",
            "(:ORDINARY)",
            "(T NIL)",
            "(NIL T)",
            "(NIL T)"
        ]
    );
}

#[test]
fn compiled_notinline_declarations_apply_to_the_designated_function() {
    let directory = common::files::TempDir::new("function-declaration-scopes");
    let source = directory.write(
        "notinline.lisp",
        "(declaim (notinline (setf scoped-target)))
         (defun scoped-labels ()
           (labels ((advance (x) (+ x 1)) (caller (x) (advance x)))
             (declare (notinline advance))
             #'caller))
         (defun scoped-raw-labels ()
           (unwind-protect
               (labels ((advance (x) (+ x 2)) (caller (x) (advance x)))
                 (declare (notinline advance))
                 #'caller)))",
    );
    let fasl = Runtime::start()
        .unwrap()
        .compile_file(&source, None)
        .unwrap();
    let compiled = CompiledFile::decode(&std::fs::read(&fasl).unwrap()).unwrap();
    let declared_calls = compiled
        .nodes
        .iter()
        .filter_map(|node| match node {
            LiteralNode::Function(code) => Some(code),
            _ => None,
        })
        .flat_map(|code| &code.instructions)
        .filter(|instruction| {
            matches!(
                instruction,
                Instruction::Call {
                    notinline: true,
                    ..
                }
            )
        })
        .count();
    assert_eq!(declared_calls, 2);

    let runtime = Runtime::start().unwrap();
    runtime.load_str(DECLARATION_SCOPE_SETUP).unwrap();
    runtime.load_file(&fasl).unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"(values (funcall (scoped-labels) 6) (funcall (scoped-raw-labels) 8)
                (compile-expansions '(lambda (x)
                  (values (scoped-target x) (funcall #'(setf scoped-target) x nil)))))"#,
        ),
        ["7", "10", "(:ORDINARY)"]
    );
    runtime
        .eval("(proclaim '(inline (setf scoped-target)))")
        .unwrap();
    runtime
        .eval("(proclaim '(notinline scoped-target))")
        .unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            r#"(compile-expansions '(lambda (x)
                 (values (scoped-target x) (funcall #'(setf scoped-target) x nil))))"#,
        ),
        ["(:SETTER)"]
    );
}

#[test]
fn compiler_macros_expand_only_calls_to_the_visible_global_function() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(progn
               (defun cm-add (x) (+ x 1))
               (define-compiler-macro cm-add (&whole form x)
                 (if (integerp x) (+ x 100) form))
               (defun cm-shadow-test ()
                 (flet ((cm-add (x) (+ x 2)))
                   (cm-add 5)))
               (defun cm-notinline-test (x)
                 (declare (notinline cm-add))
                 (cm-add x))
               (values
                 (functionp (compiler-macro-function 'cm-add))
                 (eval '(cm-add 5))
                 (eval '(let ((y 5)) (cm-add y)))
                 (cm-shadow-test)
                 (cm-notinline-test 5)
                 (macrolet ((lookup-local-compiler-macro (&environment env)
                              `',(compiler-macro-function 'cm-add env)))
                   (flet ((cm-add (x) x))
                     (declare (ignore #'cm-add))
                     (lookup-local-compiler-macro)))
                 (setf (compiler-macro-function 'cm-add) nil)
                 (compiler-macro-function 'cm-add)))"
        ),
        ["T", "105", "6", "7", "6", "NIL", "NIL", "NIL"]
    );
}
