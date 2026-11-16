use std::fs;

use zkcl::Runtime;
use zkcl_bytecode::{CaptureKind, CompiledFile, LiteralNode};

mod common;

use common::files::TempDir;

#[test]
fn parameter_defaults_capture_outer_bindings_in_nested_local_functions() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(let ((total 7))
           (block finish
             (labels ((advance (x) (incf total x))
                      (outer (&optional (n (advance 2)))
                        (flet ((inner (&key (v (advance n)))
                                 (return-from finish (values n v total))))
                          (declare (notinline inner))
                          (inner))))
               (declare (notinline advance outer))
               (outer))))",
        )
        .unwrap();
    assert_eq!(
        values
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(9), Some(18), Some(18)]
    );
}

#[test]
fn raw_forms_preserve_special_declarations_across_closures() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(let ((x 10))
               (declare (special x))
               (let ((x 4))
                 (let ((writer
                         (funcall (lambda ()
                           (locally (declare (special x))
                             (lambda () (unwind-protect (setq x 19)))))))
                       (reader
                         (lambda (&optional (prior (lambda () x)))
                           (declare (special x))
                           (values (funcall prior) (unwind-protect x)))))
                   (multiple-value-bind (lexical dynamic) (funcall reader)
                     (flet ((lexical-reader () x))
                       (declare (special x))
                       (let* ((initial x) (written (funcall writer)))
                         (values lexical dynamic (lexical-reader) initial written
                                 (unwind-protect x)
                                 (funcall (lambda (x) (unwind-protect x)) 50))))))))",
        )
        .unwrap();
    assert_eq!(
        values
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [
            Some(4),
            Some(10),
            Some(4),
            Some(10),
            Some(19),
            Some(19),
            Some(50)
        ]
    );
}

#[test]
fn raw_macro_writes_follow_their_lexical_binding_through_nested_functions() {
    let rt = Runtime::start().unwrap();
    let values = rt
        .eval(
            "(let ((x 4))
               (macrolet ((write-x () '(setq x 19)))
                 (let ((reader (lambda () x))
                       (writer-maker
                         (lambda ()
                           (flet ((make-writer (&aux (writer
                                                      (lambda ()
                                                        (unwind-protect (write-x)))))
                                    writer))
                             #'make-writer)))
                       (shadowed-maker
                         (lambda (&optional (x 50 x-p))
                           (lambda ()
                             (unwind-protect (if x-p (+ x 100) x) (write-x))))))
                   (let* ((shadowed (funcall shadowed-maker))
                          (before (funcall shadowed))
                          (after (funcall shadowed))
                          (supplied (funcall (funcall shadowed-maker 7)))
                          (outer-before (funcall reader))
                          (writer (funcall (funcall writer-maker)))
                          (written (funcall writer)))
                     (values before after supplied outer-before written (funcall reader))))))",
        )
        .unwrap();
    assert_eq!(
        values
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(50), Some(19), Some(107), Some(4), Some(19), Some(19)]
    );
}

#[test]
fn raw_forms_capture_only_the_bindings_they_reference() {
    let rt = Runtime::start().unwrap();
    let directory = TempDir::new("raw-closure-captures");
    let input = directory.write(
        "closures.lisp",
        "(defun raw-local (direct hidden spare)
           (flet ((child () (values (unwind-protect hidden) direct)))
             (values #'child spare)))",
    );
    let path = rt.compile_file(&input, None).unwrap();
    let compiled = CompiledFile::decode(&fs::read(&path).unwrap()).unwrap();
    let captures: Vec<_> = compiled
        .nodes
        .iter()
        .filter_map(|node| match node {
            LiteralNode::Function(code) if !code.captures.is_empty() => Some(code.captures.len()),
            _ => None,
        })
        .collect();
    assert_eq!(captures, [2]);

    rt.load_file(&path).unwrap();
    let result = rt.eval("(raw-local 1 2 3)").unwrap();
    assert_eq!(result[1].as_fixnum(), Some(3));
    let reading = rt.call(&result[0], &[]).unwrap();
    assert_eq!(reading[0].as_fixnum(), Some(2));
    assert_eq!(reading[1].as_fixnum(), Some(1));
}

#[test]
fn raw_macro_calls_retain_local_functions_and_parameter_defaults() {
    let runtime = Runtime::start().unwrap();
    let directory = TempDir::new("raw-local-function-defaults");
    let source = "(defun make-raw-local-reader (x)
                    (flet ((read-current (&optional (value x)) value))
                      (macrolet ((hidden-call () '(read-current)))
                        (lambda () (unwind-protect (hidden-call))))))";
    let input = directory.write("local-defaults.lisp", source);
    let compiled = runtime.compile_file(&input, None).unwrap();
    for from_fasl in [false, true] {
        if from_fasl {
            runtime.load_file(&compiled).unwrap();
        } else {
            runtime.load_str(source).unwrap();
        }
        let closure = runtime.eval("(make-raw-local-reader (list 19))").unwrap();
        runtime.collect();
        let result = runtime.call(&closure[0], &[]).unwrap();
        assert_eq!(runtime.repr(&result[0]).unwrap(), "(19)");
    }
}

#[test]
fn closures_share_bindings_only_within_the_same_activation() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defun counter (start)
           (values (lambda () (setq start (+ start 1))) (lambda () start)))
         (defun nested (start)
           (lambda () (lambda () (setq start (+ start 2)))))
         (defun lexical-source () 99)
         (let ((local 7)) (defun exported-closure () local))",
    )
    .unwrap();
    let counter = rt.eval("(counter 10)").unwrap();
    let independent = rt.eval("(counter 40)").unwrap();
    let call0 = |f| rt.call(f, &[]).unwrap()[0].as_fixnum();
    assert_eq!(call0(&counter[0]), Some(11));
    rt.collect();
    assert_eq!(call0(&counter[1]), Some(11));
    assert_eq!(call0(&independent[1]), Some(40));
    assert_eq!(call0(&counter[0]), Some(12));

    let factory = rt.eval("(nested 5)").unwrap().pop().unwrap();
    let one = rt.call(&factory, &[]).unwrap().pop().unwrap();
    let two = rt.call(&factory, &[]).unwrap().pop().unwrap();
    drop(factory);
    rt.collect();
    assert_eq!(call0(&one), Some(7));
    assert_eq!(call0(&two), Some(9));

    let late = rt.eval("(let ((x 1)) (setq x 8) #'(lambda () x))").unwrap();
    assert_eq!(call0(&late[0]), Some(8));
    let from_values = rt
        .eval("(multiple-value-bind (a b) (values 3 4) (lambda () (+ a b)))")
        .unwrap();
    assert_eq!(call0(&from_values[0]), Some(7));
    assert_eq!(
        rt.eval("(exported-closure)").unwrap()[0].as_fixnum(),
        Some(7)
    );

    let shadowed = rt
        .eval(
            "(let ((lexical-source 6))
                 (flet ((lexical-source () (+ 1 (lexical-source))))
                   (values lexical-source (funcall #'lexical-source))))",
        )
        .unwrap();
    assert_eq!(
        shadowed
            .iter()
            .map(|root| root.as_fixnum())
            .collect::<Vec<_>>(),
        [Some(6), Some(100)]
    );

    let even = rt
        .eval(
            "(labels ((is-even (n) (if (= n 0) t (is-odd (- n 1))))
                        (is-odd (n) (if (= n 0) nil (is-even (- n 1)))))
                 #'is-even)",
        )
        .unwrap()
        .pop()
        .unwrap();
    rt.collect();
    let argument = rt.fixnum(750).unwrap();
    let truth = rt.eval("t").unwrap();
    assert!(rt.call(&even, &[&argument]).unwrap()[0].same_identity(&truth[0]));
}

#[test]
fn compiled_closures_use_unboxed_captures() {
    use zkcl_bytecode::Instruction;
    let runtime = Runtime::start().unwrap();
    let directory = TempDir::new("direct-closure-captures");
    let input = directory.write(
        "direct.lisp",
        "(defun make-readers (a b) (lambda () (values a b)))
         (defun capture-exit (callee)
           (block done (funcall callee (lambda () (return-from done 73))) 0))",
    );
    let file = runtime.compile_file_to_bytecode(&input).unwrap();
    let functions: Vec<_> = file
        .nodes
        .iter()
        .filter_map(|node| match node {
            LiteralNode::Function(code) => Some(code),
            _ => None,
        })
        .collect();
    assert!(functions.iter().any(|code| {
        code.arity.required == 2
            && code
                .instructions
                .iter()
                .any(|ins| matches!(ins, Instruction::Closure { cells, .. } if cells == &(0..2)))
    }));
    let exit = functions
        .iter()
        .find(|code| code.captures == [CaptureKind::ExitToken])
        .unwrap();
    assert!(
        exit.instructions
            .iter()
            .any(|ins| matches!(ins, Instruction::Capture(_, 0)))
    );
    assert!(functions.iter().all(|code| {
        !code
            .instructions
            .iter()
            .any(|ins| matches!(ins, Instruction::BoxLocal { .. }))
    }));
    let compiled = runtime.compile_file(&input, None).unwrap();
    runtime.load_file(&compiled).unwrap();
    let closure = runtime.eval("(make-readers (list 19) (list 23))").unwrap();
    runtime.collect();
    let values = runtime.call(&closure[0], &[]).unwrap();
    assert_eq!(runtime.repr(&values[0]).unwrap(), "(19)");
    assert_eq!(runtime.repr(&values[1]).unwrap(), "(23)");
    assert_eq!(
        runtime.eval("(capture-exit #'funcall)").unwrap()[0].as_fixnum(),
        Some(73)
    );
}

#[test]
fn forwarded_go_targets_share_one_activation_across_a_compiler_boundary() {
    let rt = Runtime::start().unwrap();
    let dir = TempDir::new("forwarded-go");
    let input = dir.write(
        "jump.lisp",
        "(defun forwarded-go (pick)
       (block result
         (tagbody
           (unwind-protect
               (funcall (funcall (lambda () (lambda (flag)
                                              (if flag (go first) (go second))))) pick))
          first (return-from result :first)
          second (return-from result :second))))",
    );
    let module = rt.compile_file_to_bytecode(&input).unwrap();
    let environments: Vec<_> = module
        .nodes
        .iter()
        .filter_map(|node| match node {
            LiteralNode::Function(code) if !code.captures.is_empty() => {
                Some(code.captures.as_slice())
            }
            _ => None,
        })
        .collect();
    assert_eq!(environments, [&[CaptureKind::ExitToken]; 2]);
    let file = rt.compile_file(&input, None).unwrap();
    rt.load_file(&file).unwrap();
    assert_eq!(
        rt.repr(
            &rt.eval("(list (forwarded-go nil) (forwarded-go t))")
                .unwrap()[0]
        )
        .unwrap(),
        "(:SECOND :FIRST)"
    );
}
