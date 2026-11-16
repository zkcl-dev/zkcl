use zkcl::Runtime;

mod common;

use common::lisp::eval_reprs;

#[test]
fn defconstant_binds_a_global_constant() {
    let runtime = Runtime::start().unwrap();
    assert_eq!(
        eval_reprs(&runtime, "(defconstant +limit+ 20)"),
        ["+LIMIT+"]
    );
    assert_eq!(eval_reprs(&runtime, "+limit+"), ["20"]);
    assert_eq!(eval_reprs(&runtime, "(boundp '+limit+)"), ["T"]);
    assert_eq!(eval_reprs(&runtime, "(+ +limit+ 1)"), ["21"]);
    assert_eq!(
        eval_reprs(&runtime, "(defconstant +documented+ 3 \"a constant\")"),
        ["+DOCUMENTED+"]
    );
    assert_eq!(eval_reprs(&runtime, "+documented+"), ["3"]);
    assert_eq!(
        eval_reprs(&runtime, "(documentation '+documented+ 'variable)"),
        ["\"a constant\""]
    );
}

#[test]
fn reinterning_a_name_does_not_transfer_its_constant_binding() {
    let rt = Runtime::start().unwrap();
    rt.load_str("(defconstant retained 42) (defparameter *original* 'retained)")
        .unwrap();
    rt.collect();
    assert_eq!(eval_reprs(&rt, "(defconstant retained 42)"), ["RETAINED"]);
    assert!(rt.eval("(defconstant retained 43)").is_err());
    rt.eval("(unintern *original*)").unwrap();
    assert_eq!(
        eval_reprs(&rt, "(list (constantp *original*) (constantp 'retained))"),
        ["(T NIL)"]
    );
    assert_eq!(eval_reprs(&rt, "(setq retained 9)"), ["9"]);
}
