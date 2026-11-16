use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn exponential_rounding_keeps_the_requested_scale_after_a_carry() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(format nil "~,1,,0e" 9.8776395e-34)"#, "0.1e-32"),
        (r#"(format nil "~,1,,0e" 9.49)"#, "0.9e+1"),
        (r#"(format nil "~,1e" 99.9)"#, "1.0e+2"),
        (r#"(format nil "~,1,,2e" 99.9)"#, "10.e+1"),
        (r#"(format nil "~,2,,-1e" 99.9)"#, "0.01e+4"),
        (r#"(format nil "~,3,,-2e" -99.9)"#, "-0.001e+5"),
        (r#"(format nil "~9,1,2,0@e" 99.9)"#, " +0.1e+03"),
        (r#"(format nil "~,2e" 637.5)"#, "6.38e+2"),
    ] {
        let values = runtime.eval(source).unwrap();
        assert_eq!(
            runtime.string(&values[0]).as_deref(),
            Some(expected),
            "{source}"
        );
    }
}

#[test]
fn exponential_keeps_one_fractional_zero_when_it_fits() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(format nil "~5e" 1)"#, "1.e+0"),
        (r#"(format nil "~6e" 1.0d0)"#, "1.0e+0"),
        (r#"(format nil "~7,0e" 1)"#, "  1.e+0"),
        (r#"(format nil "~6@e" 1)"#, "+1.e+0"),
        (r#"(format nil "~7@e" 1)"#, "+1.0e+0"),
        (r#"(format nil "~7e" -1)"#, "-1.0e+0"),
        (r#"(format nil "~7,,,,,'_e" 1)"#, "_1.0e+0"),
        (r#"(format nil "~6,,2e" 1)"#, "1.e+00"),
        (r#"(format nil "~7,,2e" 1)"#, "1.0e+00"),
        (r#"(format nil "~,,2e" 1)"#, "1.0e+00"),
    ] {
        let values = runtime.eval(source).unwrap();
        assert_eq!(
            runtime.string(&values[0]).as_deref(),
            Some(expected),
            "{source}"
        );
    }
}

#[test]
fn format_emits_the_text_selected_by_the_control_string() {
    let runtime = Runtime::start().unwrap();
    let cases = [
        (r#"(format nil "plain text 雪 ~~~%")"#, "plain text 雪 ~\n"),
        (
            r#"(format nil "~A ~S ~:A ~:S" "雪" "雪" nil nil)"#,
            "雪 \"雪\" () ()",
        ),
        (
            r#"(format nil "[~5,2A][~5,2@A]" "ab" "ab")"#,
            "[ab    ][    ab]",
        ),
        (
            r#"(format nil "[~V,,,'_A][~5,'0D]" 4 "ab" 9)"#,
            "[ab__][00009]",
        ),
        (
            r#"(format nil "~:D ~@D ~X ~B ~O ~36R" 1234567 8 255 6 9 35)"#,
            "1,234,567 +8 FF 110 11 Z",
        ),
        (
            r#"(format nil "~D berr~:@P ~D berr~:@P" 1 2)"#,
            "1 berry 2 berries",
        ),
        (
            r#"(format nil "~D test~:P ~D test~:P" 1 2)"#,
            "1 test 2 tests",
        ),
        (
            r#"(format nil "~(~A~)|~:(~A~)|~@(~A~)|~:@(~A~)" "HELLO WORLD" "HELLO WORLD" "HELLO WORLD" "hello World")"#,
            "hello world|Hello World|Hello world|HELLO WORLD",
        ),
        (
            r#"(format nil "~:[no~;yes~]|~@[arg=~A~]|~[blue~;green~:;other~]" t 'foo 8)"#,
            "yes|arg=FOO|other",
        ),
        (
            r#"(format nil "~:{~A ~A~:^, ~}" '((1 2) (3 4)))"#,
            "1 2, 3 4",
        ),
        (
            r#"(format nil "~{~A~^, ~}|~@{~A~^+~}" '(x y z) 1 2 3)"#,
            "X, Y, Z|1+2+3",
        ),
        (r#"(format nil "~2@{~A~}~A" 'a 'b 'c)"#, "ABC"),
        (r#"(format nil "~1{~:}" "done" nil)"#, "done"),
        (
            r#"(format nil "~?~@?" "~D~*~D" '(1 2 3) "~D~:*~A" 4)"#,
            "1344",
        ),
        (r#"(format nil "~D~@*~D" 42)"#, "4242"),
        (
            r#"(let ((*print-pretty* t))
                 (with-output-to-string (out)
                   (pprint-logical-block (out nil)
                     (let ((outer out))
                       (write-string "lead" out)
                       (pprint-logical-block (out nil)
                         (format outer "~3:TX~:@_Y"))))))"#,
            "lead   X\n    Y",
        ),
        (
            r#"(let ((*print-pretty* t) (*print-right-margin* 6))
                 (format nil "zz~@<~@<aa ~_bb~:>~:>"))"#,
            "zzaa\n  bb",
        ),
        (
            r#"(let ((*print-pretty* t)) (format nil "~@<hi  there~:@>"))"#,
            "hi  there",
        ),
        (
            r#"(progn
                 (defun zk-format-align (stream destination colon atsign)
                   (declare (ignore colon atsign))
                   (assert (eq stream destination))
                   (format stream "~5TX"))
                 (with-output-to-string (out)
                   (format out "ab~/zk-format-align/!" out)))"#,
            "ab   X!",
        ),
        (
            r#"(progn
                 (defun zk-format-break (out value colon atsign)
                   (declare (ignore value colon atsign))
                   (write-string "xx" out)
                   (pprint-newline :mandatory out)
                   (write-string "yy" out))
                 (let ((*print-pretty* t))
                   (format nil "pre~@<~/zk-format-break/~:>" nil)))"#,
            "prexx\n   yy",
        ),
        (
            r#"(let ((*print-pretty* t))
                 (format nil "zz~@<A  ~%B~:@_~4:TC~:>"))"#,
            "zzA  \nB\n      C",
        ),
        (
            r#"(progn
                 (defclass zk-format-field () ())
                 (defmethod print-object ((value zk-format-field) out)
                   (pprint-logical-block (out nil)
                     (format out "a~:@_b")))
                 (let ((*print-pretty* t) (object (make-instance 'zk-format-field)))
                   (with-output-to-string (out)
                     (format out "one~A~%xy~@<~W~:>" object object))))"#,
            "onea\n   b\nxya\n  b",
        ),
        (
            r#"(progn
                 (defclass zk-format-nested () ())
                 (defmethod print-object ((value zk-format-nested) out)
                   (pprint-logical-block (out nil)
                     (format out "a~:@_b")))
                 (let ((*print-pretty* t))
                   (format nil "xy~S" (list (make-instance 'zk-format-nested)))))"#,
            "xy(a\n   b)",
        ),
        (
            r#"(let ((*print-pretty* t)
                      (*print-pprint-dispatch* (copy-pprint-dispatch)))
                 (set-pprint-dispatch 'string
                   (lambda (stream value)
                     (write value :stream stream :pretty nil :escape t)))
                 (princ-to-string '(outer (inner "quoted") #("vector"))))"#,
            "(OUTER (INNER \"quoted\") #(\"vector\"))",
        ),
        (
            r#"(let ((*print-pretty* t))
                 (format nil "zz~<~@;~A~:>" (list (format nil "a~%b"))))"#,
            "zza\n  b",
        ),
        (r#"(format nil "X~5TY~2@TZ")"#, "X    Y  Z"),
        (r#"(format nil "~10<~S~;~S~>" 'foo 'bar)"#, "FOO    BAR"),
        (r#"(format nil "xxxx~<~%~0,5:;~A~>" "XY")"#, "xxxx\nXY"),
        (r#"(format nil "~@[~<~%~:; ~:@(~S~)~>~]" 'foo)"#, " FOO"),
        (r#"(format nil "~S~:[~; [~2:*~A]~]" "雪" t)"#, "\"雪\" [雪]"),
        (
            r#"(format nil "~C ~:C ~@C" #\X #\Space #\A)"#,
            "X Space #\\A",
        ),
        // A colon escape leaves the next sublist unprocessed. Other escapes
        // stop only the current repetition when using sublists.
        (r#"(format nil "~:{~A~^ ~A~}" '((1) (2 3)))"#, "12 3"),
        (
            "(format nil \"A~\n            B~:\n  C~@\n   D\")",
            "AB  C\nD",
        ),
        (
            r#"(let ((*print-base* 16) (*print-level* 1)) (format nil "~S ~D" '((16)) 16))"#,
            "(#) 16",
        ),
    ];
    for (source, expected) in cases {
        let result = runtime
            .eval(source)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        assert_eq!(
            runtime.string(&result[0]).as_deref(),
            Some(expected),
            "{source}"
        );
    }
}

#[test]
fn at_sign_logical_block_consumes_all_remaining_arguments() {
    let runtime = Runtime::start().unwrap();
    for (source, expected) in [
        (r#"(format nil "~@<~A~:>~#[done~]" 1 2)"#, "1done"),
        (r#"(format nil "~A:~@<~A~:>~#[done~]" 1 2 3)"#, "1:2done"),
        (r#"(format nil "~@<~0^unused~:>~#[done~]" 1 2)"#, "done"),
        (r#"(format nil "~<~A~:>~A" '(1 2) 3)"#, "13"),
    ] {
        let values = runtime.eval(source).unwrap();
        assert_eq!(
            runtime.string(&values[0]).as_deref(),
            Some(expected),
            "{source}"
        );
    }
    runtime
        .eval(r#"(assert (null (funcall (formatter "~@<~A~:>") (make-broadcast-stream) 1 2)))"#)
        .unwrap();
}

#[test]
fn logical_blocks_abbreviate_at_the_dynamic_print_level() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r##"
            (defun format-depth-callback (stream argument colon atsign)
              (declare (ignore colon atsign))
              (pprint-logical-block (stream argument :prefix "[" :suffix "]")
                (write-string "body" stream)))
            (defun format-depth-throw (stream argument colon atsign)
              (declare (ignore stream argument colon atsign))
              (throw 'depth-escape :escaped))
            (defun format-depth-fresh (stream argument colon atsign)
              (declare (ignore colon atsign))
              (write-string (format nil "~:<~W~:>" argument) stream))
            (defclass format-depth-object () ())
            (defmethod print-object ((object format-depth-object) stream)
              (declare (ignore object))
              (pprint-logical-block (stream nil :prefix "[" :suffix "]")
                (format stream "~:<~W~:>" '(((x))))))

            (dolist (pretty '(nil t))
              (let ((*print-pretty* pretty) (*print-level* 0))
                (assert (equal (catch 'depth-escape
                                 (format nil "a~:<~/format-depth-throw/~:>z" '(x))) "a#z"))
                (assert (string= (format nil "~:<body~:>|~A" nil "tail") "#|tail"))
                (assert (string= (format nil "~@:<body~:>~#[done~;leaked~]" 'x) "#done"))
                (assert (string= (format nil "~@:<body~:>~#[done~]") "#done"))
                (assert (string= (format nil "~:<body~:>" 'x) "X"))
                (assert (string= (format nil "~:<body~:>" #(x)) "#"))
                (assert (string= (with-output-to-string (s)
                                   (pprint-logical-block (s 'x) (error "not a list"))) "X"))
                (let ((*print-readably* t))
                  (assert (string= (format nil "~:<~W~:>" '((x))) "((X))"))))
              (let ((*print-pretty* pretty) (*print-level* 1))
                (assert (string= (format nil "~:<~:<~W~:>~:>|~W" '((x)) '(x)) "(#)|(X)"))
                (assert (string= (format nil "~:<~/format-depth-callback/~:>" '((x))) "(#)"))
                (assert (string= (format nil "~:<~/format-depth-fresh/~:>" '((x))) "(#)"))
                (assert (string= (format nil "~:<~?~:>" (list (formatter "~W") '((x)))) "(#)"))
                (assert (string= (with-output-to-string (s)
                                   (pprint-logical-block (s nil :prefix "[" :suffix "]")
                                     (write-string (format nil "~:<~W~:>" '(x)) s))
                                   (write '(x) :stream s)) "[#](X)"))
                (assert (string= (write-to-string (make-instance 'format-depth-object)) "[#]"))
                (assert (string= (format nil "~:<~W~:>" '((x))) "(#)"))
                (assert (string= (format nil "~:<~S~:>" '((x))) "(#)"))
                (assert (string= (format nil "~:<~@W~:>" '((x))) "((X))"))
                (assert (string= (with-output-to-string (s)
                                   (pprint-logical-block (s nil :prefix (format nil "~S" '(x)))
                                     (write-string "body" s))) "(X)body"))
                (assert (eq (catch 'depth-escape
                              (format nil "~:<~/format-depth-throw/~:>" '(x))) :escaped))
                (assert (string= (format nil "~:<~W~:>" '(x)) "(X)")))
              (let ((*print-pretty* pretty) (*print-level* 2)
                    (objects (list (make-instance 'format-depth-object))))
                (assert (string= (format nil "~:<~W~:>" objects) "([#])"))
                (assert (string= (write-to-string objects) "([#])"))))
            (let ((*print-pretty* nil) (*print-right-margin* 6) (*print-length* 1))
              (assert (string= (format nil "~W" '(one two three)) "(ONE ...)"))
              (assert (string= (format nil "~:@W" '(one two three)) "(ONE
 TWO
 THREE)"))
              (assert (string= (format nil "~W" '(one two three)) "(ONE ...)")))
            "##,
        )
        .unwrap();
}

#[test]
fn format_indirection_accepts_function_controls() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval(
            r#"(let ((one (formatter "<~A>"))
                     (skip (formatter "skip")))
                  (assert (string= (format nil "a~?z" one '(1 99)) "a<1>z"))
                  (assert (string= (format nil "a~?b~A" skip '(99) 2) "askipb2"))
                  (assert (string= (format nil "a~@?b~A" one 1 2) "a<1>b2"))
                  (assert (string= (format nil "a~@?b~A" skip 2) "askipb2"))
                  (assert (string= (format nil "a~?z" "b~?y" (list one '(1))) "ab<1>yz"))
                  (let ((out (make-string-output-stream)))
                    (assert (equal (funcall (formatter "~@?~A") out one 1 2 3 4) '(3 4)))
                    (assert (string= (get-output-stream-string out) "<1>2")))
                  (let* ((out (make-string-output-stream))
                         (callback (lambda (stream &rest args)
                                     (assert (eq stream out))
                                     (format stream "~5TX")
                                     args)))
                    (format out "ab~?Y" callback nil)
                    (assert (string= (get-output-stream-string out) "ab   XY")))
                  (assert (equal (handler-case (format nil "~?" 17 nil)
                                    (type-error (c) (type-error-expected-type c)))
                                 '(or string function))))"#,
        )
        .unwrap();
}

#[test]
fn format_appends_to_the_selected_output_stream() {
    let runtime = Runtime::start().unwrap();
    let stream = runtime
        .eval(
            r#"(let* ((stream (make-string-output-stream))
                     (start (fresh-line stream))
                     (one (princ "prefix" stream))
                     (filled (fresh-line stream))
                     (empty (fresh-line stream)))
                  (format stream "~&~S~%" "body")
                  (let ((*standard-output* stream))
                    (format t "~&~D" 42))
                  (print :printed stream)
                  (prin1 "escaped" stream)
                  (terpri stream)
                  (values start one filled empty (get-output-stream-string stream)))"#,
        )
        .unwrap();
    assert!(stream[0].is_nil());
    assert_eq!(runtime.string(&stream[1]).as_deref(), Some("prefix"));
    assert!(!stream[2].is_nil());
    assert!(stream[3].is_nil());
    assert_eq!(
        runtime.string(&stream[4]).as_deref(),
        Some("prefix\n\"body\"\n42\n:PRINTED \"escaped\"\n")
    );
}

#[test]
fn format_keeps_its_destination_when_a_directive_changes_standard_output() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(defun format-redirect-output (stream target colon atsign)
                 (declare (ignore colon atsign))
                 (write-char #\C stream)
                 (setq *standard-output* target))"#,
        )
        .unwrap();
    let result = runtime
        .eval(
            r#"(let ((first (make-string-output-stream))
                     (second (make-string-output-stream)))
                 (let ((*standard-output* first))
                   (format t "A~/cl-user::format-redirect-output/B" second))
                 (values (get-output-stream-string first)
                         (get-output-stream-string second)))"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&result[0]).as_deref(), Some("ACB"));
    assert_eq!(runtime.string(&result[1]).as_deref(), Some(""));
}

#[test]
fn format_appends_string_and_function_controls_to_fill_pointer_destinations() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(defun format-write-a-character (stream value colon atsign)
                 (declare (ignore colon atsign))
                 (write-char value stream))"#,
        )
        .unwrap();
    let values = runtime
        .eval(
            r#"(let* ((adjustable (make-array 2 :element-type 'character
                                              :initial-contents "ab"
                                              :fill-pointer 2 :adjustable t))
                      (backing (copy-seq "_ab____z"))
                      (displaced (make-array 6 :element-type 'character
                                               :displaced-to backing
                                               :displaced-index-offset 1
                                               :fill-pointer 2)))
                 (values
                   (format adjustable "C~/cl-user::format-write-a-character/E" #\D)
                   adjustable (fill-pointer adjustable)
                   (format displaced (lambda (stream &rest args)
                                       (write-char (first args) stream)
                                       (format stream "|~A" (second args)))
                           #\C #\D)
                   displaced (fill-pointer displaced) backing
                   (handler-case (format (copy-seq "no fill pointer") "x")
                     (type-error () :invalid-destination))))"#,
        )
        .unwrap();
    assert!(values[0].is_nil());
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("abCDE"));
    assert_eq!(values[2].as_fixnum(), Some(5));
    assert!(values[3].is_nil());
    assert_eq!(runtime.string(&values[4]).as_deref(), Some("abC|D"));
    assert_eq!(values[5].as_fixnum(), Some(5));
    assert_eq!(runtime.string(&values[6]).as_deref(), Some("_abC|D_z"));
    assert_eq!(runtime.repr(&values[7]).unwrap(), ":INVALID-DESTINATION");
}

#[test]
fn invalid_format_directives_signal_program_error() {
    let runtime = Runtime::start().unwrap();
    for source in [
        r#"(format nil "~")"#,
        r#"(format nil "~{~D~]")"#,
        r#"(format nil "~D")"#,
        r#"(format nil "~{constant~}" '(x))"#,
    ] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(
            error.kind(),
            EvalErrorKind::LispCondition(ConditionClass::ProgramError),
            "{source}"
        );
    }
}

#[test]
fn unqualified_format_function_name_uses_common_lisp_user_package() {
    let runtime = Runtime::start().unwrap();
    let result = runtime
        .eval(
            r#"(progn
                 (setf (symbol-function 'cl-user::zk-format-function-test)
                       (lambda (stream object colon atsign) (princ "CL-USER" stream)))
                 (let* ((package (make-package "ZK-FORMAT-FUNCTION-TEST-PACKAGE"
                                               :use '("COMMON-LISP")))
                        (*package* package))
                   (setf (symbol-function (intern "ZK-FORMAT-FUNCTION-TEST" package))
                         (lambda (stream object colon atsign) (princ "CURRENT" stream)))
                   (format nil "~/ZK-FORMAT-FUNCTION-TEST/" 42)))"#,
        )
        .unwrap();

    assert_eq!(runtime.string(&result[0]).as_deref(), Some("CL-USER"));
}
