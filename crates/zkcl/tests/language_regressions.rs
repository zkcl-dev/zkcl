use std::collections::BTreeMap;

use zkcl::{Root, Runtime};

mod common;

use common::lisp::lisp_string;

#[derive(Debug)]
enum ExpectedValue {
    Fixnum(isize),
    Nil,
    String(&'static str),
}

impl ExpectedValue {
    fn matches(&self, runtime: &Runtime, actual: &Root<'_>) -> bool {
        match self {
            Self::Fixnum(expected) => actual.as_fixnum() == Some(*expected),
            Self::Nil => actual.is_nil(),
            Self::String(expected) => runtime.string(actual).as_deref() == Some(expected),
        }
    }
}

struct LanguageCase {
    chapter: &'static str,
    name: &'static str,
    source: &'static str,
    expected: &'static [ExpectedValue],
    required: bool,
}

impl LanguageCase {
    const fn new(
        chapter: &'static str,
        name: &'static str,
        source: &'static str,
        expected: &'static [ExpectedValue],
    ) -> Self {
        Self {
            chapter,
            name,
            source,
            expected,
            required: false,
        }
    }

    const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    fn check(&self, runtime: &Runtime) -> Result<(), String> {
        let actual = runtime
            .eval(self.source)
            .map_err(|error| format!("{:?} — {error}", error.kind()))?;
        if actual.len() == self.expected.len()
            && actual
                .iter()
                .zip(self.expected)
                .all(|(value, expected)| expected.matches(runtime, value))
        {
            return Ok(());
        }

        let actual = actual
            .iter()
            .map(|value| {
                if value.is_nil() {
                    "nil".into()
                } else if let Some(integer) = value.as_fixnum() {
                    integer.to_string()
                } else if let Some(text) = runtime.string(value) {
                    format!("{text:?}")
                } else if let Some(symbol) = runtime.symbol_name(value) {
                    symbol
                } else {
                    "<other Lisp object>".into()
                }
            })
            .collect::<Vec<_>>();
        Err(format!("expected {:?}, received {actual:?}", self.expected))
    }
}

// Sample behavior, including expected outcomes that are still under development.
// `.required()` marks cases whose failure gates the normal test suite.
const LANGUAGE_CASES: &[LanguageCase] = &{
    use ExpectedValue::{Fixnum, Nil, String};
    [
        LanguageCase::new(
            "02 Syntax",
            "string-single-escape",
            r#""a\"b\\c""#,
            &[String("a\"b\\c")],
        )
        .required(),
        LanguageCase::new(
            "02 Syntax",
            "nested-block-comment",
            "(+ 40 #| outer #| inner |# outer |# 2)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "02 Syntax",
            "escaped-colon-constituent",
            r#"(if (eq '|X:Y| 'x\:y) 42 0)"#,
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "03 Evaluation and Compilation",
            "lexical-macro-environment",
            "(macrolet ((answer () 42)
                        (relay (&environment env) (macroexpand-1 '(answer) env)))
               (relay))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "03 Evaluation and Compilation",
            "compile-anonymous-function",
            "(funcall (compile nil '(lambda (n) (+ n 2))) 40)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "04 Types and Classes",
            "fixnum-membership",
            "(values (if (typep 42 'fixnum) 42 0) (typep nil 'fixnum))",
            &[Fixnum(42), Nil],
        )
        .required(),
        LanguageCase::new(
            "04 Types and Classes",
            "bounded-integer-type-specifier",
            "(if (typep 42 '(integer 40 45)) 42 0)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "05 Data and Control Flow",
            "parallel-and-sequential-binding",
            "(let ((x 40)) (values (let ((x 42) (y x)) y) (let* ((x 42) (y x)) y)))",
            &[Fixnum(40), Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "05 Data and Control Flow",
            "closure-mutation",
            "(let* ((x 40) (bump (lambda () (setq x (+ x 1)))) (read-x (lambda () x)))
               (values (funcall bump) (funcall bump) (funcall read-x)))",
            &[Fixnum(41), Fixnum(42), Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "05 Data and Control Flow",
            "lambda-list-order",
            "(funcall #'(lambda (x &optional (y (+ x 1) yp) &aux (z (+ y 1)))
                          (values x y z yp))
                      40)",
            &[Fixnum(40), Fixnum(41), Fixnum(42), Nil],
        )
        .required(),
        LanguageCase::new(
            "05 Data and Control Flow",
            "zero-values",
            "(if t (values) nil)",
            &[],
        )
        .required(),
        LanguageCase::new(
            "05 Data and Control Flow",
            "multiple-value-padding",
            "(multiple-value-bind (x y z) (values 41 42) (values z y x))",
            &[Nil, Fixnum(42), Fixnum(41)],
        )
        .required(),
        LanguageCase::new(
            "05 Data and Control Flow",
            "cleanup-on-transfer",
            "(let ((cleanup 0))
               (values (catch 'done (unwind-protect (throw 'done 42) (setq cleanup 1)))
                       cleanup))",
            &[Fixnum(42), Fixnum(1)],
        )
        .required(),
        LanguageCase::new(
            "05 Data and Control Flow",
            "multiple-value-call",
            "(multiple-value-call #'+ (values 3 9) (values) (values 30))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "05 Data and Control Flow",
            "setq-of-symbol-macro-sets-place",
            "(let ((cell (list 40)))
               (symbol-macrolet ((x (car cell))) (setq x (+ x 2)) (car cell)))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "06 Iteration",
            "dotimes-result",
            "(let ((sum 0)) (dotimes (i 4 sum) (setq sum (+ sum (* i 7)))))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "07 Objects",
            "class-initarg-lookup",
            "(progn (defclass sized-object () ((x :initarg :x)))
                    (slot-value (make-instance 'sized-object :x 42) 'x))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "08 Structures",
            "structure-generated-accessor",
            "(progn (defstruct default-record (x 42))
                    (default-record-x (make-default-record)))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "09 Conditions",
            "type-error-datum",
            "(handler-case (car 42) (type-error (c) (type-error-datum c)))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "09 Conditions",
            "invoke-restart-from-function-handler",
            "(handler-bind ((error (lambda (c) (invoke-restart (find-restart 'use-number c) 42))))
               (restart-case (error \"replace me\") (use-number (n) n)))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "09 Conditions",
            "define-condition-slot",
            "(progn (define-condition invalid-value (error)
                      ((x :initarg :x :reader invalid-value-x)))
                    (invalid-value-x (make-condition 'invalid-value :x 42)))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "10 Symbols",
            "keyword-is-self-evaluating",
            "(if (eq :ready ':ready) 42 0)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "10 Symbols",
            "uninterned-symbols-have-separate-identity",
            "(if (eq '#:same '#:same) 0 42)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "11 Packages",
            "standard-package-nickname",
            "(cl:car '(42))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "11 Packages",
            "intern-existing-symbol-status",
            "(let ((pkg (make-package \"SYMBOL-REGISTRY\" :use nil)))
               (unwind-protect
                   (let ((s (intern \"X\" pkg)))
                     (multiple-value-bind (name status) (find-symbol \"X\" pkg)
                       (if (eq s name) (if (eq status :internal) 42 0) 0)))
                 (delete-package pkg)))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "12 Numbers",
            "fixnum-arithmetic",
            "(+ (* 6 7) (- 11 11))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "12 Numbers",
            "bignum-round-trip",
            "(- (+ 123456789012345678901234567890 42) 123456789012345678901234567890)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "12 Numbers",
            "ratio-canonicalization",
            "(* 2/3 63)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "13 Characters",
            "reader-character-matches-string-element",
            r#"(if (char= #\A (char "A" 0)) 42 0)"#,
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "14 Conses",
            "dotted-pair-cdr",
            "(cdr '(40 . 42))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "14 Conses",
            "list-indexing",
            "(nth 1 (list 40 42))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "15 Arrays",
            "rank-two-initialization",
            "(aref (make-array '(2 2) :initial-element 42) 1 1)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "16 Strings",
            "string-case-conversion",
            r#"(string-upcase "zkcl")"#,
            &[String("ZKCL")],
        )
        .required(),
        LanguageCase::new(
            "17 Sequences",
            "list-length",
            "(length (list 1 2 3))",
            &[Fixnum(3)],
        )
        .required(),
        LanguageCase::new(
            "18 Hash Tables",
            "equal-key-lookup",
            "(let ((table (make-hash-table :test 'equal)))
               (setf (gethash (list 1 2) table) 42)
               (multiple-value-bind (value present) (gethash (list 1 2) table)
                 (if present value 0)))",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "19 Filenames",
            "pathname-name-component",
            r#"(pathname-name (make-pathname :name "zkcl"))"#,
            &[String("zkcl")],
        )
        .required(),
        LanguageCase::new(
            "21 Streams",
            "input-from-string",
            r#"(with-input-from-string (stream "42") (values (read-line stream)))"#,
            &[String("42")],
        )
        .required(),
        LanguageCase::new(
            "22 Printer",
            "decimal-printer",
            "(write-to-string 42 :base 10 :radix nil)",
            &[String("42")],
        )
        .required(),
        LanguageCase::new(
            "23 Reader",
            "read-from-string-end-position",
            r#"(read-from-string " 42")"#,
            &[Fixnum(42), Fixnum(3)],
        )
        .required(),
        LanguageCase::new(
            "23 Reader",
            "feature-expression-controls-reading",
            "(+ 40 #-(and) 100 #+(and) 2)",
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "24 System Construction",
            "provide-registers-module",
            r#"(progn (provide "SAMPLE-MODULE")
                      (if (member "SAMPLE-MODULE" *modules* :test #'string=) 42 0))"#,
            &[Fixnum(42)],
        )
        .required(),
        LanguageCase::new(
            "25 Environment",
            "implementation-type-is-string",
            "(if (stringp (lisp-implementation-type)) 42 0)",
            &[Fixnum(42)],
        )
        .required(),
    ]
};

#[test]
fn required_language_cases_match_expected_values() {
    let runtime = Runtime::start().unwrap();
    let mut counts_by_chapter = BTreeMap::<_, (usize, usize)>::new();
    let mut required_failures = Vec::new();

    for case in LANGUAGE_CASES {
        let (passed, run) = counts_by_chapter.entry(case.chapter).or_default();
        *run += 1;
        match case.check(&runtime) {
            Ok(()) => *passed += 1,
            Err(error) => {
                let detail = format!("{} / {} — {error}", case.chapter, case.name);
                if case.required {
                    required_failures.push(detail);
                } else {
                    println!("Open case {detail}");
                }
            }
        }
    }

    let mut total_passed = 0;
    for (chapter, (passed, run)) in counts_by_chapter {
        println!("{chapter}: {passed}/{run} language cases passed");
        total_passed += passed;
    }
    println!(
        "Total language cases passed: {total_passed}/{}",
        LANGUAGE_CASES.len()
    );
    assert!(
        required_failures.is_empty(),
        "Common Lisp language regressions:\n{}",
        required_failures.join("\n")
    );
}

#[test]
#[ignore = "requires an SBCL executable on PATH"]
fn sbcl_agrees_with_expected_values() {
    use std::fmt::Write as _;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut script = String::new();
    for case in LANGUAGE_CASES {
        let source = lisp_string(case.source);
        let name = lisp_string(case.name);
        write!(script, "(let ((expected '(").unwrap();
        for value in case.expected {
            match value {
                ExpectedValue::Fixnum(integer) => write!(script, "{integer} "),
                ExpectedValue::Nil => write!(script, "nil "),
                ExpectedValue::String(text) => write!(script, "{} ", lisp_string(text)),
            }
            .unwrap();
        }
        writeln!(
            script,
            ")) (actual (multiple-value-list (eval (read-from-string {source})))))
               (unless (equal actual expected)
                 (error \"Case ~A expected ~S but returned ~S\" {name} expected actual)))"
        )
        .unwrap();
    }

    let mut process = Command::new("sbcl")
        .args([
            "--noinform",
            "--no-userinit",
            "--no-sysinit",
            "--non-interactive",
            "--eval",
            "(let ((eof (gensym)))
               (loop for form = (read *standard-input* nil eof)
                     until (eq form eof) do (eval form)))",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .expect("start SBCL");
    let input = process
        .stdin
        .take()
        .expect("open SBCL stdin")
        .write_all(script.as_bytes());
    let status = process.wait().expect("wait for SBCL");
    assert!(status.success(), "SBCL rejected one or more language cases");
    input.expect("write language cases to SBCL");
    println!(
        "Validated {} expected results with SBCL",
        LANGUAGE_CASES.len()
    );
}
