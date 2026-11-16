use zkcl::{EvalErrorKind, Runtime};

#[test]
fn macro_literals_keep_their_identity_during_compiler_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-now", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    runtime
        .load_str(
            "(let* ((object (list 1 2)) (text \"heap literal\")
                    (target (lambda (x) (+ x (car object)))))
               (setq saved object saved-text text saved-target target)
               (defmacro quote-captured () (list 'quote object))
               (defmacro return-captured-string () text)
               (defmacro return-captured-function () target))
             (defmacro fresh-quoted-list () (list 'quote (list 55 56)))
             (defmacro collect-before-materialize () (collect-now) nil)
             (defmacro sees-captured (expression) (eq (car (cdr expression)) saved))
             (defmacro pass-captured () (list 'sees-captured (list 'quote saved)))
             (defun nest (count object)
               (if (= count 0) object (nest (- count 1) (list object))))
             (setq deep (nest 300 0))
             (defmacro quote-deep () (list 'quote deep))
             (defmacro return-deep-code () deep)",
        )
        .unwrap();

    let values = runtime
        .eval(
            "(values (eq saved (quote-captured))
                     (eq saved-text (return-captured-string))
                     (car (fresh-quoted-list))
                     (collect-before-materialize)
                     (pass-captured)
                     (eq deep (quote-deep))
                     (eq saved-target (return-captured-function))
                     (funcall (return-captured-function) 8))",
        )
        .unwrap();
    let result: Vec<_> = values.iter().map(|value| !value.is_nil()).collect();
    assert_eq!(result, [true, true, true, false, true, true, true, true]);
    assert_eq!(values[2].as_fixnum(), Some(55));
    assert_eq!(values[7].as_fixnum(), Some(9));

    let error = runtime.eval("(return-deep-code)").err().unwrap();
    assert_eq!(error.kind(), EvalErrorKind::MacroExpansion);
    assert!(error.to_string().contains("form nesting limit"));
}

#[test]
fn generated_defmacro_treats_a_docstring_as_declarative_body_prefix() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            "(setq saved (list 1 2))
             (defmacro define-documented ()
               '(defmacro documented () \"A docstring\"
                  (declare (special saved))
                  (car saved)))",
        )
        .unwrap();
    runtime.eval("(define-documented)").unwrap();
    assert_eq!(
        runtime.eval("(documented)").unwrap()[0].as_fixnum(),
        Some(1)
    );
}
