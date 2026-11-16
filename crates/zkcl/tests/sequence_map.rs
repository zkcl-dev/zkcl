use zkcl::{ConditionClass, EvalErrorKind, Runtime};

fn repr(runtime: &Runtime, source: &str) -> Vec<String> {
    runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"))
        .iter()
        .map(|value| runtime.repr(value).unwrap())
        .collect()
}

#[test]
fn map_stops_at_the_shortest_input() {
    let runtime = Runtime::start().unwrap();

    runtime
        .eval("(defun map-test-identity (value) value)")
        .unwrap();

    for (source, expected) in [
        (
            r#"(let ((seen nil))
                 (values (map 'list (lambda (x y z)
                                      (let ((entry (list x y z)))
                                        (push entry seen)
                                        (values entry :extra)))
                              '(1 2 3) #*1010 "雪😀")
                         seen))"#,
            vec![r#"((1 1 #\雪) (2 0 #\😀))"#, r#"((2 0 #\😀) (1 1 #\雪))"#],
        ),
        (
            r#"(values (map '(vector bit 3) 'map-test-identity #*101)
                       (map '(string 2) 'map-test-identity "é雪")
                       (map 'cons 'map-test-identity '(:one))
                       (map 'null 'map-test-identity nil)
                       (map 'vector 'map-test-identity nil))"#,
            vec!["#*101", r#""é雪""#, "(:ONE)", "NIL", "#()"],
        ),
        (
            "(let ((cycle (list 10 20)) (seen nil))
               (rplacd (cdr cycle) cycle)
               (values (map 'vector #'+ cycle #(1 2 3 4 5))
                       (map nil (lambda (x y) (push (+ x y) seen)) '(1 2 3) cycle)
                       seen (map 'list #'list cycle nil)))",
            vec!["#(11 22 13 24 15)", "NIL", "(13 22 11)", "NIL"],
        ),
        (
            "(let ((seen nil))
               (values (map nil (lambda (n) (push n seen)) '(1 2 3))
                       seen
                       (map 'list (lambda (n) (values)) '(1 2))))",
            vec!["NIL", "(3 2 1)", "(NIL NIL)"],
        ),
    ] {
        assert_eq!(repr(&runtime, source), expected, "{source}");
    }
}

#[test]
fn sequence_quantifiers_short_circuit_on_primary_values() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval("(defun map-test-identity (value) value)")
        .unwrap();
    for (source, expected) in [
        (
            "(let ((seen nil))
               (values (every (lambda (x y) (push x seen) (< x y))
                              '(1 8 3) #(2 3 4 5))
                       seen
                       (every #'characterp \"é雪\")
                       (every (lambda (x y) (error \"unexpected call\")) '(1) nil)))",
            vec!["NIL", "(8 1)", "T", "T"],
        ),
        (
            "(let ((seen nil))
               (values (some (lambda (x) (push x seen)
                               (if (= x 2) (values (list :found x) :extra) (values)))
                             #(1 2 3))
                       seen
                       (notany #'oddp #*000)
                       (notevery #'oddp #*110)
                       (notany #'oddp #*001)
                       (notevery #'oddp #*111)
                       (map 'list #'every (list #'map-test-identity #'null) '((1 2) (nil nil)))))",
            vec!["(:FOUND 2)", "(2 1)", "T", "T", "NIL", "NIL", "(T T)"],
        ),
        (
            "(some (lambda (x) (values x :extra)) '(nil :found :later))",
            vec![":FOUND"],
        ),
        (
            "(values (some #'null nil)
                     (notany #'null nil)
                     (notevery #'null nil)
                     (notevery (lambda (x) (values nil t)) '(1)))",
            vec!["NIL", "T", "NIL", "T"],
        ),
    ] {
        assert_eq!(repr(&runtime, source), expected, "{source}");
    }
}

#[test]
fn map_into_sets_the_fill_pointer_after_writing() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval("(defun map-test-identity (value) value)")
        .unwrap();
    for (source, expected) in [
        (
            "(let ((target (make-array 4 :initial-contents '(0 0 0 0) :fill-pointer 2))
                   (seen nil))
               (values (eq target
                           (map-into target (lambda () (push (fill-pointer target) seen)
                                               (length seen))))
                       target seen (fill-pointer target)))",
            vec!["T", "#(1 2 3 4)", "(2 2 2 2)", "4"],
        ),
        (
            "(let ((target (make-array 4 :initial-contents '(1 2 3 4) :fill-pointer 2)))
               (map-into target #'1+ target)
               (values (fill-pointer target) (aref target 0) (aref target 1)
                       (aref target 2) (map-into target #'map-test-identity nil)
                       (fill-pointer target) (aref target 1)))",
            vec!["2", "2", "3", "3", "#()", "0", "3"],
        ),
    ] {
        assert_eq!(repr(&runtime, source), expected, "{source}");
    }
}

#[test]
fn map_into_reads_live_overlapping_storage() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval("(defun map-test-identity (value) value)")
        .unwrap();
    for (source, expected) in [
        (
            "(let* ((backing (vector 1 2 3 4))
                     (from (make-array 3 :displaced-to backing))
                     (to (make-array 3 :displaced-to backing :displaced-index-offset 1))
                     (cells (list 1 2 3 4)))
               (map-into to #'map-test-identity from)
               (map-into (cdr cells) #'map-test-identity cells)
               (values backing cells))",
            vec!["#(1 1 1 1)", "(1 1 1 1)"],
        ),
        (
            r#"(let ((bits (copy-seq #*110))
                      (text (copy-seq "aé雪")))
                 (values (map-into bits #'map-test-identity #*0101)
                         (map-into text #'map-test-identity "😀β")
                         (map-into nil (lambda () (error "unexpected call")))))"#,
            vec!["#*010", r#""😀β雪""#, "NIL"],
        ),
    ] {
        assert_eq!(repr(&runtime, source), expected, "{source}");
    }
}

#[test]
fn map_into_keeps_results_live_during_callback_collection() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-in-sequence-map", |runtime, arguments| {
            runtime.collect();
            Ok(arguments.to_vec())
        })
        .unwrap();
    for (source, expected) in [
        (
            "(let* ((items (list (list :a) (list :b)))
                     (target (list nil nil nil))
                     (output (map '(simple-vector 2) 'collect-in-sequence-map items)))
               (map-into target 'collect-in-sequence-map output)
               (values output target (eq (car target) (aref output 0))))",
            vec!["#((:A) (:B))", "((:A) (:B) NIL)", "T"],
        ),
        (
            "(let ((target (make-array 3 :initial-contents '(0 0 0) :fill-pointer 1))
                   (cleaned nil))
               (multiple-value-bind (a b)
                   (catch :done
                     (unwind-protect
                         (map-into target (lambda (x)
                                            (collect-in-sequence-map)
                                            (if (= x 2) (throw :done (values :exit x)) x))
                                   '(1 2 3))
                       (setq cleaned t)))
                 (values a b cleaned (fill-pointer target) (aref target 0) (aref target 1))))",
            vec![":EXIT", "2", "T", "1", "1", "0"],
        ),
    ] {
        assert_eq!(repr(&runtime, source), expected, "{source}");
    }
}

#[test]
fn sequence_mapping_rejects_invalid_input() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval("(defun map-test-identity (value) value)")
        .unwrap();
    for (source, class) in [
        (
            "(map 'sequence #'map-test-identity '(1))",
            ConditionClass::TypeError,
        ),
        (
            "(map '(vector t 2) #'map-test-identity '(1))",
            ConditionClass::TypeError,
        ),
        (
            "(map 'cons #'map-test-identity nil)",
            ConditionClass::TypeError,
        ),
        (
            "(map 'string #'map-test-identity '(1))",
            ConditionClass::TypeError,
        ),
        (
            "(let ((cycle (list 1))) (rplacd cycle cycle) (map 'list #'list cycle cycle))",
            ConditionClass::TypeError,
        ),
        (
            "(map-into (copy-seq #*0) #'map-test-identity '(2))",
            ConditionClass::TypeError,
        ),
        (
            "(map-into '(1 . 2) #'map-test-identity #(3))",
            ConditionClass::TypeError,
        ),
        (
            "(every #'map-test-identity nil #2A((1)))",
            ConditionClass::TypeError,
        ),
        ("(some 1 '(a))", ConditionClass::TypeError),
        (
            "(map nil #'map-test-identity)",
            ConditionClass::ProgramError,
        ),
        ("(notany #'map-test-identity)", ConditionClass::ProgramError),
        (
            "(let ((cycle (list nil))) (setf (cdr cycle) cycle) (notevery #'map-test-identity cycle))",
            ConditionClass::TypeError,
        ),
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(class),
            "{source}: {error}"
        );
    }
}
