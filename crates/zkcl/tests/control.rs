use zkcl::Runtime;

#[test]
fn throw_uses_the_innermost_identical_tag_in_the_current_dynamic_environment() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defvar *throw-phase* :outer)").unwrap();

    for (source, expected) in [
        (
            "(let* ((tag (list :tag))
                    (different (copy-list tag)))
               (multiple-value-list
                 (catch tag
                   (catch different
                     (throw tag (values :matched :outer)))
                   :wrong)))",
            "(:MATCHED :OUTER)",
        ),
        (
            "(catch :tag
               (catch :exit
                 (unwind-protect
                     (catch :tag (throw :exit :wrong))
                   (throw :tag :older))))",
            ":OLDER",
        ),
        (
            "(let ((events nil))
               (list
                (handler-case
                    (let ((*throw-phase* :cleanup))
                      (unwind-protect
                          (progv '(*throw-phase*) '(:signaling)
                            (handler-bind
                                ((control-error
                                   (lambda (condition)
                                     (declare (ignore condition))
                                     (push *throw-phase* events))))
                              (throw (progn (push :tag events) :missing)
                                     (progn (push :result events) (values)))))
                        (push *throw-phase* events)))
                  (control-error () *throw-phase*))
                (reverse events)))",
            "(:OUTER (:TAG :RESULT :SIGNALING :CLEANUP))",
        ),
    ] {
        let values = rt.eval(source).unwrap();
        assert_eq!(rt.repr(&values[0]).unwrap(), expected, "{source}");
    }
}

#[test]
fn nonlocal_exits_deliver_all_values_to_their_target() {
    let rt = Runtime::start().unwrap();
    let numbers = |source| {
        rt.eval(source)
            .unwrap()
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>()
    };

    assert_eq!(
        numbers(
            "(block b
           (let ((exit (lambda () (return-from b (values 2 3)))))
             (block b (funcall exit) 99)
             98))",
        ),
        [Some(2), Some(3)]
    );
    assert_eq!(
        numbers("(block b (block b (return-from b 1)) (return-from b 2))",),
        [Some(2)]
    );
    assert!(rt.eval("(block nil (return (values)))").unwrap().is_empty());

    rt.load_str(
        "(defun send (tag) (throw tag (values 7 9)))
         (defun early (n) (if n (return-from early (values 6 8))) 99)",
    )
    .unwrap();
    assert_eq!(numbers("(early t)"), [Some(6), Some(8)]);
    assert_eq!(numbers("(early nil)"), [Some(99)]);
    assert_eq!(
        numbers("(flet ((get-it () (return-from get-it 14))) (get-it))"),
        [Some(14)]
    );
    assert_eq!(
        numbers("(let ((key (list 1))) (catch key (send key)))"),
        [Some(7), Some(9)]
    );
    assert_eq!(numbers("(catch :k (catch :k (send :k)) 19)"), [Some(19)]);

    assert_eq!(
        numbers("(unwind-protect (values 1 2) (values 3 4))"),
        [Some(1), Some(2)]
    );
    assert!(
        rt.eval("(unwind-protect (values) (values 3 4))")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        numbers(
            "(let ((order 0))
           (values (catch :out
                     (unwind-protect
                         (unwind-protect (throw :out 8)
                           (setq order (+ (* order 10) 1)))
                       (setq order (+ (* order 10) 2))))
                   order))",
        ),
        [Some(8), Some(12)]
    );
    assert_eq!(
        numbers("(catch :out (unwind-protect (throw :out 1) (throw :out (values 3 4))))",),
        [Some(3), Some(4)]
    );
    assert_eq!(
        numbers(
            "(catch :out (unwind-protect (throw :out (values 49 69))
                       (catch :cleanup (throw :cleanup 7))))",
        ),
        [Some(49), Some(69)]
    );
    assert_eq!(
        numbers("(block out (unwind-protect (return-from out 1) (return-from out 42)))",),
        [Some(42)]
    );

    let missing = rt
        .eval("(unwind-protect (throw :missing (setq attempt 2)) (setq cleaned 3))")
        .err()
        .unwrap();
    assert!(missing.to_string().contains("no active CATCH"));
    assert_eq!(numbers("(values attempt cleaned)"), [Some(2), Some(3)]);
    assert!(
        rt.eval("(unwind-protect (car 1) (setq cleaned 5))")
            .is_err()
    );
    assert_eq!(numbers("cleaned"), [Some(5)]);

    let escaped = rt
        .eval("(block done (lambda () (return-from done 1)))")
        .unwrap();
    let error = rt.call(&escaped[0], &[]).err().unwrap();
    assert!(error.to_string().contains("BLOCK has already exited"));
    assert!(rt.eval("(return-from no-block 1)").is_err());

    assert_eq!(
        numbers(
            "(let ((n 0))
           (tagbody again (setq n (+ n 1)) (if (= n 3) (go 41) (go again))
              (setq n 999)
            41)
           n)",
        ),
        [Some(3)]
    );
    assert_eq!(
        numbers(
            "(let ((n 0))
           (tagbody
             (flet ((exit () (unwind-protect (go done) (setq n 42))))
               (tagbody (exit) done (setq n 999)))
             (setq n 888)
            done)
           n)",
        ),
        [Some(42)]
    );
    assert!(rt.eval("(tagbody (go done) (car 1) done)").unwrap()[0].is_nil());
    let jump = rt
        .eval(
            "(let (saved)
        (tagbody (setq saved (lambda () (go done))) done)
        saved)",
        )
        .unwrap();
    let error = rt.call(&jump[0], &[]).err().unwrap();
    assert!(error.to_string().contains("TAGBODY has already exited"));
}

#[test]
fn lexical_exits_unwind_targets_discovered_in_later_branches() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defun nested-return (path)
           (let ((*exit-value* 17))
             (declare (special *exit-value*))
             (block outer
               (block middle
                 (if (eq path :outer)
                     (return-from outer (values *exit-value* 18))
                     (block inner
                       (funcall (if (eq path :middle)
                                    (return-from middle 23)
                                    (lambda () (return-from inner 42))))))))))
         (defun nested-go (path)
           (let ((result 0) (*exit-value* 17))
             (declare (special *exit-value*))
             (tagbody
               (block middle
                 (if (eq path :outer)
                     (go outer-done)
                     (tagbody
                       (funcall (if (eq path :middle)
                                    (return-from middle nil)
                                    (lambda () (go inner-done))))
                       (setq result -99)
                       inner-done
                       (setq result (+ *exit-value* 2)))))
               (setq result (+ result 3))
               outer-done)
             result))",
    )
    .unwrap();

    for (source, expected) in [
        ("(nested-return :outer)", &[17, 18][..]),
        ("(nested-return :middle)", &[23]),
        ("(nested-return :inner)", &[42]),
        ("(nested-go :outer)", &[0]),
        ("(nested-go :middle)", &[3]),
        ("(nested-go :inner)", &[22]),
    ] {
        let values = rt.eval(source).unwrap();
        let actual: Vec<_> = values
            .iter()
            .map(|value| value.as_fixnum().unwrap())
            .collect();
        assert_eq!(actual, expected, "{source}");
    }
}

#[test]
fn local_declarations_do_not_escape_their_lexical_scope() {
    let rt = Runtime::start().unwrap();

    assert_eq!(
        rt.eval(
            "(if t
               (progn
                 (let ((x 1))
                   (declare (type fixnum x))
                   x)
                 (let ((x nil) (y 2))
                   (setq x \"fresh\"))))",
        )
        .unwrap()
        .iter()
        .map(|value| rt.repr(value).unwrap())
        .collect::<Vec<_>>(),
        ["\"fresh\""]
    );
}

#[test]
fn external_go_labels_survive_closure_forwarding() {
    let rt = Runtime::start().unwrap();
    for jump in [
        "(funcall (funcall (lambda () (lambda () (go second)))))",
        "(funcall (lambda () (flet ((jump () (go second))) (declare (notinline jump)) (jump))))",
    ] {
        let source = format!(
            "(block done (tagbody {jump}
                               first (return-from done 1)
                               second (return-from done 2)))"
        );
        assert_eq!(rt.eval(&source).unwrap()[0].as_fixnum(), Some(2), "{jump}");
    }
}

#[test]
fn captured_exits_select_the_original_recursive_activation() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defvar *exit-trace*)
         (defun recursive-return (depth escape)
           (block target
             (let ((exit (or escape
                             (lambda () (return-from target (values depth (list depth)))))))
               (unwind-protect
                   (if (zerop depth)
                       (mapc (lambda (argument) (declare (ignore argument)) (funcall exit))
                             '(a b))
                       (recursive-return (1- depth) exit))
                 (push depth *exit-trace*)))
             (values :wrong-target depth)))
         (defun recursive-go (depth escape)
           (tagbody
             (flet ((jump () (go second)))
               (let ((exit (or escape #'jump)))
                 (unwind-protect
                     (if (zerop depth)
                         (funcall exit)
                         (recursive-go (1- depth) exit))
                   (push depth *exit-trace*))))
             (return-from recursive-go :wrong-activation)
             first
             (return-from recursive-go :wrong-tag)
             second
             (return-from recursive-go depth)))",
    )
    .unwrap();

    for (call, expected) in [
        ("(recursive-return 3 nil)", "((3 (3)) (3 2 1 0))"),
        ("(recursive-go 3 nil)", "((3) (3 2 1 0))"),
    ] {
        let source = format!(
            "(let ((*exit-trace* nil))
               (list (multiple-value-list {call}) *exit-trace*))"
        );
        let values = rt.eval(&source).unwrap();
        assert_eq!(rt.repr(&values[0]).unwrap(), expected, "{call}");
    }
}

#[test]
fn cleanup_transfers_replace_a_pending_exit_only_when_they_escape() {
    let rt = Runtime::start().unwrap();
    for (source, expected) in [
        (
            "(multiple-value-list
               (block outer
                 (let ((replacement (lambda () (return-from outer (values :cleanup '(3 5))))))
                   (unwind-protect
                       (funcall (lambda () (return-from outer :protected)))
                     (funcall replacement)))))",
            "(:CLEANUP (3 5))",
        ),
        (
            "(block result
               (tagbody
                 (let ((replacement (lambda () (go second))))
                   (unwind-protect
                       (funcall (lambda () (go first)))
                     (funcall replacement)))
                 first
                 (return-from result :wrong-tag)
                 second
                 (return-from result :cleanup)))",
            ":CLEANUP",
        ),
        (
            "(let ((count 0))
               (tagbody
                 (unwind-protect
                     (funcall (lambda () (go outer)))
                   (incf count (block local (funcall (lambda () (return-from local 1)))))
                   (tagbody
                     (funcall (lambda () (go local)))
                     (setq count -100)
                     local
                     (incf count)))
                 (setq count -200)
                 outer
                 (incf count))
               count)",
            "3",
        ),
    ] {
        let values = rt.eval(source).unwrap();
        assert_eq!(rt.repr(&values[0]).unwrap(), expected, "{source}");
    }
}

#[test]
fn captured_exits_signal_control_error_after_their_extent_ends() {
    let rt = Runtime::start().unwrap();
    rt.load_str(
        "(defun reenter-block (old)
           (block target
             (let ((escape (lambda () (return-from target :wrong-activation))))
               (if old
                   (handler-case (funcall old) (control-error () :expired))
                   escape))))
         (defun reenter-tagbody (old)
           (let (escape)
             (tagbody
               (setq escape (lambda () (go target)))
               (when old
                 (return-from reenter-tagbody
                   (handler-case (funcall old) (control-error () :expired))))
               (go done)
               target
               (return-from reenter-tagbody :wrong-activation)
               done)
             escape))",
    )
    .unwrap();

    // An expired exit has undefined consequences in Common Lisp. zkcl signals CONTROL-ERROR.
    for source in [
        "(reenter-block (reenter-block nil))",
        "(reenter-tagbody (reenter-tagbody nil))",
        "(block destination
           (block abandoned
             (let ((escape (lambda () (return-from abandoned :wrong-target))))
               (unwind-protect (return-from destination :wrong-value)
                 (handler-case (funcall escape)
                   (control-error () (return-from destination :expired)))))))",
        "(block destination
           (tagbody
             (let ((escape (lambda () (go abandoned))))
               (unwind-protect (return-from destination :wrong-value)
                 (handler-case (funcall escape)
                   (control-error () (return-from destination :expired)))))
             abandoned
             (return-from destination :wrong-target)))",
    ] {
        let values = rt.eval(source).unwrap();
        assert_eq!(rt.repr(&values[0]).unwrap(), ":EXPIRED", "{source}");
    }
}

#[test]
fn tagbody_resolves_only_lexically_visible_source_tags() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defmacro value-statement () '17)").unwrap();
    let body = "(block done
                 (tagbody
                   (value-statement)
                   (go #1=#:same)
                   #2=#:same (return-from done :wrong-symbol)
                   #1# (tagbody (go #x10000000000000000)
                                18446744073709551616
                                (funcall (lambda () (go 17))))
                   (return-from done :wrong-scope)
                   17 (return-from done :ok)))";
    for source in [body.to_owned(), format!("(funcall (lambda () {body}))")] {
        assert_eq!(rt.repr(&rt.eval(&source).unwrap()[0]).unwrap(), ":OK");
    }
    for source in ["(tagbody 1/2)", "(lambda () (tagbody #\\A))"] {
        let error = rt
            .eval(source)
            .err()
            .expect("expected an invalid TAGBODY tag");
        assert!(
            error
                .to_string()
                .contains("TAGBODY tag or a compound statement"),
            "{error}"
        );
    }
}
