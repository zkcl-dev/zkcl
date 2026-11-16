use zkcl::{Root, Runtime};

mod common;

use common::lisp::eval_reprs;

fn eval_values<'rt>(runtime: &'rt Runtime, source: &str) -> Vec<Root<'rt>> {
    runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error:?}"))
}

fn eval_numbers(runtime: &Runtime, source: &str) -> Vec<Option<isize>> {
    eval_values(runtime, source)
        .iter()
        .map(|value| value.as_fixnum())
        .collect()
}

#[test]
fn plist_lookup_prefers_the_first_matching_indicator_by_identity() {
    let runtime = Runtime::start().unwrap();

    assert_eq!(
        eval_numbers(
            &runtime,
            "(let* ((key (list 1))
                    (plist (list key 17 :first nil :first 28)))
               (values (getf plist key 0)
                       (getf plist (list 1) 2)
                       (getf plist :first 3)
                       (getf plist :missing 4)))",
        ),
        [Some(17), Some(2), None, Some(4)]
    );
}

#[test]
fn get_properties_returns_the_original_plist_tail() {
    let runtime = Runtime::start().unwrap();
    let properties = eval_values(
        &runtime,
        "(let ((plist (list :unmatched 1 nil nil :later 3)))
           (multiple-value-bind (indicator value tail)
               (get-properties plist (list :later nil))
             (values indicator value tail (eq tail (cddr plist)))))",
    );
    assert_eq!(properties.len(), 4);
    assert!(properties[0].is_nil());
    assert!(properties[1].is_nil());
    assert!(!properties[2].is_nil());
    assert!(!properties[3].is_nil());
    let absent = eval_values(
        &runtime,
        "(get-properties (list :present 1) (list :missing))",
    );
    assert_eq!(absent.len(), 3);
    assert!(absent.iter().all(zkcl::Root::is_nil));
}

#[test]
fn setf_getf_updates_the_captured_place_after_evaluating_the_new_value() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-plist-value", |runtime, _| {
            runtime.collect();
            Ok(vec![runtime.fixnum(42).unwrap()])
        })
        .unwrap();

    assert_eq!(
        eval_numbers(
            &runtime,
            "(let* ((order 0) (owner (list nil)) (original owner))
               (values
                 (setf (getf (car (progn (setq order 1) owner))
                             (progn (setq order (+ (* order 10) 2)) :fresh)
                             (progn (setq order (+ (* order 10) 3)) 99))
                       (progn
                         (setq order (+ (* order 10) 4))
                         (setq owner (list (list :other 31)))
                         (rplaca original (list :late 22))
                         (collect-plist-value)))
                 order
                 (getf (car original) :fresh)
                 (getf (car original) :late)
                 (getf (car owner) :other)))",
        ),
        [Some(42), Some(1234), Some(42), Some(22), Some(31)]
    );
}

#[test]
fn getf_expansions_compose_in_generalized_places() {
    let runtime = Runtime::start().unwrap();

    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((props nil))
               (setf (getf (getf props :outer) :inner) 7)
               (values (incf (getf (getf props :outer) :inner) 2)
                       (getf (getf props :outer) :inner)
                       (incf (getf props :counter 10))))",
        ),
        [Some(9), Some(9), Some(11)]
    );
    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((props (list :a 1 :b 2)))
               (psetf (getf props :a) (getf props :b)
                      (getf props :b) (getf props :a))
               (values (getf props :a) (getf props :b)))",
        ),
        [Some(2), Some(1)]
    );
}

#[test]
fn remf_relinks_the_place_after_removing_the_first_match() {
    let runtime = Runtime::start().unwrap();
    let removed = eval_values(
        &runtime,
        "(let* ((owner (list (list :first 11 :first 12 :last 13)))
                (order 0))
           (symbol-macrolet ((props (car owner)))
             (values
               (remf (car (progn (setq order 1) owner))
                     (progn (setq order (+ (* order 10) 2)) :first))
               (getf props :first)
               order
               (remf props :last)
               (remf props :missing)
               (getf props :last)
               (remf props :first)
               props)))",
    );
    assert_eq!(removed.len(), 8);
    assert!(!removed[0].is_nil());
    assert_eq!(removed[1].as_fixnum(), Some(12));
    assert_eq!(removed[2].as_fixnum(), Some(12));
    assert!(!removed[3].is_nil());
    assert!(removed[4..6].iter().all(zkcl::Root::is_nil));
    assert!(!removed[6].is_nil());
    assert!(removed[7].is_nil());
}

#[test]
fn plist_operations_validate_the_entire_list_before_mutating() {
    let runtime = Runtime::start().unwrap();

    assert_eq!(
        eval_numbers(
            &runtime,
            "(let ((plist (list :present 1 :odd))
                   (dotted (list* :present 3 :dotted))
                   (circle (list :present 5)))
               (rplacd (cdr circle) circle)
               (values
                 (handler-case (setf (getf plist :present) 2)
                   (type-error () (cadr plist)))
                 (handler-case (remf dotted :present)
                   (type-error () (cadr dotted)))
                 (handler-case (getf circle :present)
                   (type-error () (cadr circle)))
                 (handler-case (get-properties nil circle)
                   (type-error () 7))))",
        ),
        [Some(1), Some(3), Some(5), Some(7)]
    );
}

#[test]
fn get_reads_the_current_symbol_property_list() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(
            &runtime,
            "(let ((s (gensym)))
               (values
                 (symbol-plist s)
                 (get s :a :missing)
                 (setf (get s :a) 10)
                 (get s :a)
                 (setf (get s :b) 20)
                 (remprop s :a)
                 (remprop s :a)
                 (get s :a :gone)
                 (setf (symbol-plist s) '(:x 1 :y 2))
                 (get s :y)))"
        ),
        [
            "NIL",
            ":MISSING",
            "10",
            "10",
            "20",
            "T",
            "NIL",
            ":GONE",
            "(:X 1 :Y 2)",
            "2"
        ]
    );
}
