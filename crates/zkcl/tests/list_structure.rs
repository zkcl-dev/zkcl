use zkcl::{ConditionClass, EvalErrorKind, Root, Runtime};

fn binary_tree(runtime: &Runtime, depth: u32, leaf: isize) -> Root<'_> {
    if depth == 0 {
        return runtime.fixnum(leaf).unwrap();
    }
    let car = binary_tree(runtime, depth - 1, leaf * 2);
    let cdr = binary_tree(runtime, depth - 1, leaf * 2 + 1);
    runtime.cons(&car, &cdr).unwrap()
}

#[test]
fn list_accessors_follow_the_cons_links_in_the_input() {
    let runtime = Runtime::start().unwrap();

    // Every path through a complete binary tree identifies a different leaf.
    // The name spells the C[AD]+R traversal in reverse order.
    for depth in 1..=4 {
        let tree = binary_tree(&runtime, depth, 0);
        for path in 0..(1 << depth) {
            let mut name = String::from("C");
            for step in 0..depth {
                name.push(if path & (1 << step) == 0 { 'A' } else { 'D' });
            }
            name.push('R');
            let function = runtime
                .eval(&format!("#'CL:{name}"))
                .unwrap()
                .pop()
                .unwrap();
            let result = runtime.call(&function, &[&tree]).unwrap();
            assert_eq!(result[0].as_fixnum(), Some(path), "{name}");
        }
    }

    let result = runtime
        .eval(
            "(let ((x '(0 1 2 3 4 5 6 7 8 9)))
               (values (first x) (second x) (third x) (fourth x) (fifth x)
                       (sixth x) (seventh x) (eighth x) (ninth x) (tenth x)
                       (nth 6 x)))",
        )
        .unwrap();
    assert_eq!(
        result.iter().map(Root::as_fixnum).collect::<Vec<_>>(),
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 6].map(Some)
    );

    for source in [
        "(eq t (endp nil))",
        "(null (endp '(1 . 2)))",
        "(listp '(1 . 2))",
        "(not (listp 41))",
        "(listp nil)",
        "(consp '(1 . 2))",
        "(null (consp nil))",
        "(null (caddar nil))",
        "(null (tenth '(1)))",
        "(null (nth 1073741824 '(1)))",
        "(null (nthcdr 1073741824 '(1)))",
        "(= 42 (rest '(1 . 42)))",
        "(= 42 (nthcdr 1 '(1 . 42)))",
        "(= 42 (list* 42))",
        "(let ((tail (list 3 4))) (eq tail (nthcdr 2 (list* 1 2 tail))))",
        "(let ((x '(1 2 . 42))) (eq (rest x) (last x)))",
        "(let ((x '(1 2 . 42))) (eq x (last x 4)))",
        "(= 42 (last '(1 2 . 42) 0))",
        "(null (last nil 2))",
        "(let ((x (list 1 2))) (eq x (last x (expt 2 100))))",
        "(and (null (butlast nil)) (null (nbutlast nil 0)))",
        "(equal (butlast '(1 2 . 3)) '(1))",
        "(let* ((element (list 1)) (original (list* element 2 3))
                (copy (butlast original 0)))
           (and (equal copy '((1) 2)) (eq element (car copy))
                (not (eq original copy)) (not (eq (cdr original) (cdr copy)))
                (eql (cddr original) 3)))",
        "(let* ((x (list* 1 2 3 4)) (removed (cddr x)))
           (and (eq x (nbutlast x)) (equal x '(1 2)) (equal removed '(3 . 4))))",
        "(let ((x (list* 1 2 3))) (and (eq x (nbutlast x 0)) (equal x '(1 2))))",
        "(let ((x (list* 1 2 3)))
           (and (null (butlast x 2)) (null (nbutlast x 2))
                (null (nbutlast x (1+ most-positive-fixnum)))
                (null (butlast x (expt 2 100))) (equal x '(1 2 . 3))))",
    ] {
        assert!(!runtime.eval(source).unwrap()[0].is_nil(), "{source}");
    }
}

#[test]
fn invalid_list_arguments_signal_the_corresponding_lisp_condition() {
    let runtime = Runtime::start().unwrap();
    for (source, class) in [
        ("(list*)", ConditionClass::ProgramError),
        ("(caddar nil 1)", ConditionClass::ProgramError),
        ("(last nil 1 2)", ConditionClass::ProgramError),
        ("(nthcdr 2 '(1 . 42))", ConditionClass::TypeError),
        ("(nth 1 '(1 . 42))", ConditionClass::TypeError),
        ("(last 42)", ConditionClass::TypeError),
        ("(endp 42)", ConditionClass::TypeError),
        ("(last nil 'bad-count)", ConditionClass::TypeError),
        ("(butlast)", ConditionClass::ProgramError),
        ("(nbutlast nil 0 0)", ConditionClass::ProgramError),
        ("(butlast 42 0)", ConditionClass::TypeError),
        ("(nbutlast nil -1)", ConditionClass::TypeError),
        ("(butlast nil (- (expt 2 100)))", ConditionClass::TypeError),
        ("(nbutlast nil 1.0)", ConditionClass::TypeError),
        (
            "(let ((x (list 1 2))) (rplacd (cdr x) x) (nbutlast x 0))",
            ConditionClass::TypeError,
        ),
    ] {
        let error = runtime.eval(source).err().expect("expected a condition");
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(class),
            "{source}"
        );
    }

    let slots = runtime
        .eval(
            "(handler-case (nth -1 nil)
               (type-error (c)
                 (let ((expected (type-error-expected-type c)))
                   (values (type-error-datum c) (car expected) (cadr expected)))))",
        )
        .unwrap();
    assert_eq!(slots[0].as_fixnum(), Some(-1));
    let integer = runtime.eval("'CL:INTEGER").unwrap().pop().unwrap();
    assert!(slots[1].same_identity(&integer));
    assert_eq!(slots[2].as_fixnum(), Some(0));

    let slots = runtime
        .eval(
            "(handler-case (cadr '(1 . 42))
               (type-error (c)
                 (values (type-error-datum c)
                         (eq 'cl:list (type-error-expected-type c)))))",
        )
        .unwrap();
    assert_eq!(slots[0].as_fixnum(), Some(42));
    assert!(!slots[1].is_nil());
}

#[test]
fn list_copies_preserve_the_identity_of_shared_substructure_after_gc() {
    let runtime = Runtime::start().unwrap();
    let result = runtime
        .eval(
            "(let ((tail (list 3 4)))
               (values (append (list* (list 1) (list (list 2))) tail) tail))",
        )
        .unwrap();
    runtime.collect();
    let (first, rest) = runtime.cons_fields(&result[0]).unwrap();
    assert_eq!(runtime.cons_fields(&first).unwrap().0.as_fixnum(), Some(1));
    let (second, tail) = runtime.cons_fields(&rest).unwrap();
    assert_eq!(runtime.cons_fields(&second).unwrap().0.as_fixnum(), Some(2));
    assert!(tail.same_identity(&result[1]));

    let result = runtime
        .eval("(let ((pair (cons (list 1) :tail))) (values pair (copy-list pair)))")
        .unwrap();
    runtime.collect();
    assert!(!result[0].same_identity(&result[1]));
    let (car, cdr) = runtime.cons_fields(&result[0]).unwrap();
    let (copied_car, copied_cdr) = runtime.cons_fields(&result[1]).unwrap();
    assert!(car.same_identity(&copied_car));
    assert!(cdr.same_identity(&copied_cdr));
}

#[test]
fn bignum_list_indices_select_the_corresponding_cell_on_a_cycle() {
    let runtime = Runtime::start().unwrap();
    let finite = runtime
        .eval(
            "(let ((index (ash 1 100)))
               (values (nth index '(a b))
                       (nthcdr (1+ most-positive-fixnum) '(a b))
                       (nth-value index (values :first :second))
                       (handler-case (nthcdr index '(a b . :tail))
                         (type-error (condition) (type-error-datum condition)))))",
        )
        .unwrap();
    assert!(finite[..3].iter().all(Root::is_nil));
    assert_eq!(runtime.repr(&finite[3]).unwrap(), ":TAIL");

    let circular = runtime
        .eval(
            "(let* ((ring (list :first :second :third))
                    (cells (cons :prefix ring))
                    (index (+ (ash 1 100) 2)))
               (setf (cdddr ring) ring)
               (setf (nth index cells) :replaced)
               (values (eq (nthcdr index cells) (cddr ring))
                       (nth (+ (ash 1 20) 2) cells)))",
        )
        .unwrap();
    assert_eq!(runtime.repr(&circular[0]).unwrap(), "T");
    assert_eq!(runtime.repr(&circular[1]).unwrap(), ":REPLACED");
}
