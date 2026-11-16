use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn structural_equality_is_independent_of_object_addresses() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        ("(eql -123 -123)", true),
        ("(eql 1 2)", false),
        ("(eql 'cl:eql 'eql)", true),
        ("(eql 'symbol :symbol)", false),
        ("(let ((x (cons 1 2))) (eql x x))", true),
        ("(eql (cons 1 2) (cons 1 2))", false),
        ("(equal (cons 1 2) (cons 1 2))", true),
        ("(equal '(1 (2 . 3) 4) '(1 (2 . 3) 4))", true),
        ("(equal '(1 2) '(1 . 2))", false),
        ("(equal '(1 . 2) '(1 . 3))", false),
        ("(equalp 'symbol :symbol)", false),
        ("(equal \"Grüßen東京\" \"Grüßen東京\")", true),
        ("(equal \"Grüßen東京\" \"GRÜßEN東京\")", false),
        ("(equalp \"Grüßen東京\" \"GRÜßEN東京\")", true),
        ("(equalp \"Ⱥ\" \"ⱥ\")", true),
        ("(equalp \"Straße\" \"STRASSE\")", false),
        ("(equalp \"é\" \"e\u{301}\")", false),
        ("(equalp \"foo\" \"food\")", false),
        ("(equalp \"foo\" 'foo)", false),
        (
            "(equalp (cons \"Foo\" \"Bar\") (cons \"fOO\" \"bAr\"))",
            true,
        ),
        (
            "(equalp (make-condition 'simple-error) (make-condition 'simple-error))",
            false,
        ),
        ("(typep \"a\" '(eql \"A\"))", false),
        ("(typep 3 '(member 1 2 3))", true),
        (
            "(= (sxhash '(list \"ab\")) (sxhash (list 'list \"ab\")))",
            true,
        ),
        (
            "(let* ((part (list 1 2)) (shared (cons part part))
                    (copied (cons (list 1 2) (list 1 2))))
               (= (sxhash shared) (sxhash copied)))",
            true,
        ),
        (
            "(let* ((p (make-package \"SXHASH-TEST\" :use nil))
                    (symbol (intern \"Example\" p))
                    (code (sxhash symbol)))
               (unintern symbol p)
               (let ((same-name (make-symbol \"Example\")))
                 (and (= code (sxhash symbol)) (= code (sxhash same-name)))))",
            true,
        ),
        (
            "(let ((left (list 1 2)) (right (list 1 3)))
               (rplacd (cdr left) left)
               (rplacd (cdr right) right)
               (and (typep (sxhash left) '(integer 0)) (not (equal left right))))",
            true,
        ),
        (
            "(let ((left (make-hash-table :test 'equal))
                   (right (make-hash-table :test 'equal))
                   (outer (make-hash-table :test 'equalp)))
               (setf (gethash (list \"same\") left) '(\"Lower\")
                     (gethash :b left) nil
                     (gethash :b right) nil
                     (gethash (list \"same\") right) '(\"LOWER\")
                     (gethash left outer) t)
               (and (not (equal left right)) (equalp left right) (gethash right outer)
                    (progn (setf (gethash :b right) 2) (not (equalp left right)))))",
            true,
        ),
        (
            "(let ((left (make-hash-table :test 'equal))
                   (right (make-hash-table :test 'equalp)))
               (equalp left right))",
            false,
        ),
        (
            "(let ((left (make-hash-table :test 'equalp))
                   (right (make-hash-table :test 'equalp))
                   (a (make-hash-table)) (b (make-hash-table))
                   (c (make-hash-table)) (d (make-hash-table)))
               (setf (gethash :name a) (list 1 2)
                     (gethash :name b) (list 1 3)
                     (gethash :name c) (list 1 2)
                     (gethash :name d) (list 1 3)
                     (gethash a left) \"first\" (gethash b left) \"second\"
                     (gethash d right) \"SECOND\" (gethash c right) \"FIRST\")
               (equalp left right))",
            true,
        ),
        (
            "(let ((left (make-hash-table)) (right (make-hash-table)))
               (setf (gethash :loop left) left (gethash :loop right) right)
               (equalp left right))",
            true,
        ),
    ] {
        let result = runtime.eval(source).unwrap();
        assert_eq!(result.len(), 1, "{source}");
        assert_eq!(!result[0].is_nil(), expected, "{source}");
    }

    for source in ["(eql 1)", "(equal)", "(equalp 1 2 3)", "(sxhash)"] {
        let error = runtime.eval(source).err().expect("expected an arity error");
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::ProgramError),
            "{source}"
        );
    }
}

#[test]
fn deep_equal_conses_keep_their_hash_across_collection() {
    let runtime = Runtime::start().unwrap();
    let equal = runtime.eval("#'cl:equal").unwrap().pop().unwrap();
    let sxhash = runtime.eval("#'cl:sxhash").unwrap().pop().unwrap();
    let zero = runtime.fixnum(0).unwrap();
    let mut left = runtime.nil();
    let mut right = runtime.nil();
    for _ in 0..20_000 {
        left = runtime.cons(&left, &zero).unwrap();
        right = runtime.cons(&right, &zero).unwrap();
    }
    let before = runtime.call(&sxhash, &[&left]).unwrap()[0].as_fixnum();
    assert!(before.is_some_and(|hash| hash >= 0));
    runtime.collect();
    assert!(!runtime.call(&equal, &[&left, &right]).unwrap()[0].is_nil());
    assert_eq!(
        runtime.call(&sxhash, &[&left]).unwrap()[0].as_fixnum(),
        before
    );

    // Shared subtrees should only be compared once when they occur in
    // both fields of the same pair of conses.
    for _ in 0..48 {
        left = runtime.cons(&left, &left).unwrap();
        right = runtime.cons(&right, &right).unwrap();
    }
    assert!(!runtime.call(&equal, &[&left, &right]).unwrap()[0].is_nil());
}

#[test]
fn equalp_compares_deeply_nested_hash_table_keys() {
    let runtime = Runtime::start().unwrap();
    // Structural key comparison must stay off the native stack even when
    // every matching key is itself a hash table.
    let nested = runtime
        .eval(
            "(let ((left :leaf) (right :leaf))
               (dotimes (i 4000)
                 (let ((a (make-hash-table :test 'equalp :size 1))
                       (b (make-hash-table :test 'equalp :size 1)))
                   (setf (gethash left a) i (gethash right b) i)
                   (setq left a right b)))
               (equalp left right))",
        )
        .unwrap();
    assert!(!nested[0].is_nil());
}

#[test]
fn equalp_hash_keys_match_structures_with_equal_slots_of_the_same_type() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defstruct comparison-record payload) (defstruct comparison-other payload)")
        .unwrap();
    let structures = runtime
        .eval(
            "(let ((left (make-comparison-record :payload (list \"ABC\")))
                   (right (make-comparison-record :payload (list \"abc\")))
                   (different (make-comparison-other :payload (list \"abc\")))
                   (table (make-hash-table :test 'equalp)))
               (setf (gethash left table) t)
               (and (not (equal left right)) (equalp left right)
                    (not (equalp left different)) (gethash right table)))",
        )
        .unwrap();
    assert!(!structures[0].is_nil());
}
