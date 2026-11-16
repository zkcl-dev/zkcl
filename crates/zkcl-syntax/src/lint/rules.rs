//! Static registry of built-in Common Lisp and ZKCL lint rules.

use std::sync::LazyLock;

use rustc_hash::FxHashMap;

use super::{LintCategory, LintPreset, RuleDescriptor, RuleEvidence};

macro_rules! rules {
    ($(
        $id:literal, $short:literal, $cat:ident, $preset:ident, $ev:ident =>
        summary: $summary:literal,
        rationale: $rationale:literal,
        bad: $bad:literal,
        good: $good:literal;
    )*) => {
        pub(super) const RULES: &[RuleDescriptor] = &[
            $(
                RuleDescriptor {
                    id: $id,
                    short_name: $short,
                    category: LintCategory::$cat,
                    default_preset: LintPreset::$preset,
                    evidence: RuleEvidence::$ev,
                    summary: $summary,
                    rationale: $rationale,
                    bad_example: $bad,
                    good_example: $good,
                },
            )*
        ];
    };
}

rules! {
    // ── Correctness ──────────────────────────────────────────────────────────
    "correctness/arity-mismatch", "arity-mismatch", Correctness, Default, LanguageSemantics =>
    summary: "Call passes too few or too many arguments for the resolved callable signature.",
    rationale: "Calling a function, macro, or special operator with an invalid number of arguments fails at compile time or signals a PROGRAM-ERROR at runtime.",
    bad: "(cons a b c)",
    good: "(cons a b)";

    "correctness/ffi-declaration", "ffi-declaration", Correctness, Default, ZkclExtension =>
    summary: "A literal ZK-FFI type or variadic parameter count is invalid.",
    rationale: "Foreign bindings prepare a C ABI from their declaration. Invalid types or fixed counts fail before native code can run.",
    bad: "(zk-ffi:bind-function :host \"size_of_record\" :size :void)",
    good: "(zk-ffi:bind-function :host \"size_of_record\" :size :pointer)";

    "correctness/unknown-keyword-arg", "unknown-keyword-arg", Correctness, Default, LanguageSemantics =>
    summary: "Call supplies a literal keyword argument not accepted by the callee's &key parameters.",
    rationale: "Unless &allow-other-keys is declared or :allow-other-keys is non-NIL, passing an unrecognized keyword signals a PROGRAM-ERROR at runtime.",
    bad: "(defun greet (&key name) name)\n(greet :nmae \"ada\")",
    good: "(defun greet (&key name) name)\n(greet :name \"ada\")";

    "correctness/odd-key-value-args", "odd-key-value-args", Correctness, Default, LanguageSemantics =>
    summary: "Paired assignment or keyword tail form has an odd number of place/key-value arguments.",
    rationale: "Forms such as SETF, PSETF, SETQ, PSETQ, and keyword argument tails require an even number of place/value or key/value arguments.",
    bad: "(setf x 1 y)",
    good: "(setf x 1 y 2)";

    "correctness/format-arity-mismatch", "format-arity-mismatch", Correctness, Default, StandardLibrary =>
    summary: "A statically understood FORMAT control string requires more arguments than supplied.",
    rationale: "CLHS 22.3.10.2 leaves the consequences of argument exhaustion undefined, but permits unused arguments. All argument forms are still evaluated.",
    bad: "(format nil \"~A: ~A\" key)",
    good: "(format nil \"~A: ~A\" key val)";

    "correctness/division-by-zero", "division-by-zero", Correctness, Default, LanguageSemantics =>
    summary: "Arithmetic division or remainder operation uses a literal zero divisor.",
    rationale: "Dividing by literal 0 or 0.0 signals DIVISION-BY-ZERO at runtime.",
    bad: "(/ total 0)",
    good: "(if (zerop count) 0 (/ total count))";

    "correctness/literal-sequence-bounds", "literal-sequence-bounds", Correctness, Default, StandardLibrary =>
    summary: "Sequence index or slice bounds are out of range for the literal string, vector, or list.",
    rationale: "Negative indices or indices exceeding the literal sequence length always signal an error at runtime.",
    bad: "(char \"abc\" 5)",
    good: "(char \"abc\" 2)";

    "correctness/case-quoted-key", "case-quoted-key", Correctness, Default, LanguageSemantics =>
    summary: "CASE/ECASE/CCASE branch key is quoted with 'SYM.",
    rationale: "CASE does not evaluate its keys. Writing 'foo reads as (quote foo), which means a key list matching either QUOTE or FOO rather than just FOO.",
    bad: "(case kind ('read 1) ('write 2))",
    good: "(case kind (read 1) (write 2))";

    "correctness/case-string-or-list-key", "case-string-or-list-key", Correctness, Default, LanguageSemantics =>
    summary: "CASE/ECASE/CCASE branch key is a string or nested compound datum.",
    rationale: "CASE compares keys with EQL, not EQUAL or STRING=. String literals in CASE branches almost never match distinct string objects.",
    bad: "(case mode (\"fast\" 1) (\"safe\" 2))",
    good: "(cond ((string= mode \"fast\") 1) ((string= mode \"safe\") 2))";

    "correctness/case-non-final-default", "case-non-final-default", Correctness, Default, LanguageSemantics =>
    summary: "Catch-all clause (T or OTHERWISE) appears before the final branch or as bare T/OTHERWISE in ECASE/CCASE.",
    rationale: "In COND, CASE, and TYPECASE, a T/OTHERWISE clause matches unconditionally and makes every following branch unreachable. In ECASE/CCASE, bare T or OTHERWISE is prohibited; use ((t) ...) to match the literal symbol T.",
    bad: "(case x (t :default) (:a 1))",
    good: "(case x (:a 1) (otherwise :default))";

    "correctness/duplicate-case-key", "duplicate-case-key", Correctness, Default, LanguageSemantics =>
    summary: "CASE/ECASE/CCASE repeats a key that was already matched in an earlier branch.",
    rationale: "Earlier clauses shadow later duplicates, making the repeated branch dead code or hiding a copy-paste bug.",
    bad: "(case x (:a 1) (:a 2))",
    good: "(case x (:a 1) (:b 2))";

    "correctness/duplicate-binding", "duplicate-binding", Correctness, Default, LanguageSemantics =>
    summary: "Parallel binding form, lambda list, or class/struct definition declares the same name more than once.",
    rationale: "In LET, DO, lambda lists, DEFCLASS, and DEFSTRUCT, duplicate names in the same scope are invalid or ambiguous.",
    bad: "(let ((x 1) (x 2)) x)",
    good: "(let* ((x 1) (y 2)) (+ x y))";

    "correctness/eq-on-non-identity", "eq-on-non-identity", Correctness, Default, LanguageSemantics =>
    summary: "EQ compares a number, character, string, or literal compound datum.",
    rationale: "ANSI Common Lisp explicitly leaves EQ on numbers and characters implementation-dependent, and EQ on strings/lists checks pointer identity rather than contents.",
    bad: "(eq count 0)",
    good: "(eql count 0) ; or (= count 0)";

    "correctness/eql-on-compound", "eql-on-compound", Correctness, Default, LanguageSemantics =>
    summary: "EQL compares a string or quoted list/vector literal.",
    rationale: "EQL only compares object identity for strings, lists, and arrays; distinct objects with equal contents are not EQL.",
    bad: "(eql name \"admin\")",
    good: "(string= name \"admin\")";

    "correctness/literal-mutation", "literal-mutation", Correctness, Default, LanguageSemantics =>
    summary: "Destructive operation mutates a quoted list, literal vector, or literal string.",
    rationale: "Under ANSI CLHS 3.7.1, modifying literal constants has undefined consequences and can corrupt shared constants across calls.",
    bad: "(sort '(3 1 2) #'<)",
    good: "(sort (list 3 1 2) #'<)";

    "correctness/discarded-destructive-result", "discarded-destructive-result", Correctness, Default, StandardLibrary =>
    summary: "Return value of a destructive list/sequence operation (SORT, DELETE, NREVERSE, NCONC) is discarded.",
    rationale: "Destructive CL functions are not guaranteed to preserve the head cons cell or original object reference; ignoring their return value silently drops updates or loses the first element.",
    bad: "(let ((xs (list 3 1 2))) (sort xs #'<) xs)",
    good: "(let ((xs (list 3 1 2))) (setf xs (sort xs #'<)) xs)";

    "correctness/function-of-macro-or-special", "function-of-macro-or-special", Correctness, Default, LanguageSemantics =>
    summary: "FUNCTION (#') references a macro or special operator.",
    rationale: "Macros and special operators cannot be used as first-class functions via FUNCTION, FUNCALL, or APPLY in ANSI Common Lisp.",
    bad: "(apply #'and flags)",
    good: "(every #'identity flags)";

    "correctness/return-outside-nil-block", "return-outside-nil-block", Correctness, Default, LanguageSemantics =>
    summary: "RETURN used with no enclosing NIL block.",
    rationale: "RETURN expands to (RETURN-FROM NIL ...). Unlike C or Python, DEFUN establishes a block named after the function, not NIL; use RETURN-FROM <fn> or return the tail expression.",
    bad: "(defun check (x) (when x (return 1)) 2)",
    good: "(defun check (x) (when x (return-from check 1)) 2)";

    "correctness/unknown-control-target", "unknown-control-target", Correctness, Default, LanguageSemantics =>
    summary: "RETURN-FROM or GO targets a block name or tagbody tag that does not exist in the enclosing lexical scope.",
    rationale: "Jumping to an unbound block or tagbody tag signals a CONTROL-ERROR at runtime.",
    bad: "(defun f () (return-from missing 1))",
    good: "(defun f () (return-from f 1))";

    "correctness/non-toplevel-definition", "non-toplevel-definition", Correctness, Default, LanguageSemantics =>
    summary: "Top-level defining form (DEFVAR, DEFPARAMETER, DEFCONSTANT, DEFPACKAGE, IN-PACKAGE) appears inside a local expression.",
    rationale: "Placing DEFVAR/DEFPARAMETER/IN-PACKAGE inside a function or LET causes global side effects on every call and breaks static scope assumptions.",
    bad: "(defun init () (defvar *cache* (make-hash-table)))",
    good: "(defvar *cache* (make-hash-table))\n(defun init () *cache*)";

    "correctness/defconstant-non-eql-literal", "defconstant-non-eql-literal", Correctness, Default, LanguageSemantics =>
    summary: "DEFCONSTANT initializes a constant with a compound literal or fresh constructor.",
    rationale: "Reloading a file with DEFCONSTANT requires the new value to be EQL to the old value. Quoted lists, strings, vectors, and constructor calls create non-EQL objects and signal errors on reload.",
    bad: "(defconstant +default-opts+ '(:a :b))",
    good: "(defparameter +default-opts+ '(:a :b))";

    "correctness/eval-when-invalid", "eval-when-invalid", Correctness, Default, LanguageSemantics =>
    summary: "EVAL-WHEN has an empty, unknown, or deprecated situation specifier.",
    rationale: "Empty situation lists discard the entire body; COMPILE, LOAD, and EVAL are deprecated in favor of :COMPILE-TOPLEVEL, :LOAD-TOPLEVEL, and :EXECUTE.",
    bad: "(eval-when (compile) (defun helper () 1))",
    good: "(eval-when (:compile-toplevel) (defun helper () 1))";

    "correctness/zk-match-mismatch", "zk-match-mismatch", Correctness, Default, LanguageSemantics =>
    summary: "ZK:MATCH pattern clause column count or ZK:DEFVARIANT constructor arity does not match its definition.",
    rationale: "Mismatched columns or variant fields fail pattern compilation or silently reject valid structures.",
    bad: "(zk:match (:inputs a b) (((pair x)) 1))",
    good: "(zk:match (:inputs a b) (((pair x y) _) 1))";

    "correctness/invalid-lambda-list", "invalid-lambda-list", Correctness, Default, LanguageSemantics =>
    summary: "A parameter specifier or lambda-list marker is in an invalid position.",
    rationale: "Ordinary, generic, specialized, and macro lambda lists have distinct grammars. A rest marker needs a parameter and section markers must appear in order.",
    bad: "(defun f (&key value &optional mode) (list value mode))",
    good: "(defun f (&optional mode &key value) (list value mode))";

    "correctness/invalid-binding-name", "invalid-binding-name", Correctness, Default, LanguageSemantics =>
    summary: "A variable binding or assignment names a constant or uses a non-symbol.",
    rationale: "NIL, T, and keyword symbols denote constant variables. ANSI CL leaves binding or assigning to a constant variable undefined.",
    bad: "(let ((:result 1)) :result)",
    good: "(let ((result 1)) result)";

    "correctness/invalid-binding-spec", "invalid-binding-spec", Correctness, Default, LanguageSemantics =>
    summary: "A binding list or a binding inside it has the wrong shape.",
    rationale: "Binding forms distinguish variable names, initializers, and optional step or result expressions. Extra fields are invalid.",
    bad: "(let ((x 1 2)) x)",
    good: "(let ((x 1)) x)";

    "correctness/invalid-function-name", "invalid-function-name", Correctness, Default, LanguageSemantics =>
    summary: "A function name is neither a symbol nor a (SETF symbol) name.",
    rationale: "FUNCTION can reference a function name or a lambda expression. A computed function value belongs in a value position such as the first argument to FUNCALL.",
    bad: "(function (choose-handler))",
    good: "(funcall (choose-handler))";

    "correctness/invalid-call-operator", "invalid-call-operator", Correctness, Default, LanguageSemantics =>
    summary: "A call has a literal or other invalid object in operator position.",
    rationale: "A compound form starts with an operator symbol or a lambda expression. Common Lisp uses FUNCALL or APPLY to invoke a computed function value.",
    bad: "((if flag #'one #'two) x)",
    good: "(funcall (if flag #'one #'two) x)";

    "correctness/dotted-call", "dotted-call", Correctness, Default, LanguageSemantics =>
    summary: "A standard function or special operator is called with a dotted argument list.",
    rationale: "An ordinary compound call requires a proper list. A macro can explicitly destructure dotted syntax.",
    bad: "(cons x . y)",
    good: "(cons x y)";

    "correctness/unquote-outside-backquote", "unquote-outside-backquote", Correctness, Default, LanguageSemantics =>
    summary: "A comma reader prefix is used outside backquote syntax.",
    rationale: "Comma, comma-at, and comma-dot designate evaluated parts of a backquote template.",
    bad: "(list ,item)",
    good: "(list item)";

    "correctness/misplaced-declaration", "misplaced-declaration", Correctness, Default, LanguageSemantics =>
    summary: "DECLARE is used as an expression instead of a declaration.",
    rationale: "DECLARE belongs in a declaration-bearing body before its executable forms. LOCALLY establishes a body that accepts declarations.",
    bad: "(progn (declare (optimize speed)) (work))",
    good: "(locally (declare (optimize speed)) (work))";

    "correctness/malformed-clause", "malformed-clause", Correctness, Default, LanguageSemantics =>
    summary: "A conditional, handler, or restart clause is missing required fields or list delimiters.",
    rationale: "Clause syntax assigns specific positions to tests, keys, types, and variables. Expressions in those positions are interpreted according to the enclosing operator.",
    bad: "(cond () (t :ok))",
    good: "(cond ((ready-p) :ready) (t :ok))";

    "correctness/invalid-control-target", "invalid-control-target", Correctness, Default, LanguageSemantics =>
    summary: "A block or GO target has the wrong kind of name.",
    rationale: "A block name is a symbol. A tagbody tag is a symbol or an integer.",
    bad: "(block 7 (return-from 7 :done))",
    good: "(block done (return-from done :done))";

    "correctness/duplicate-tagbody-tag", "duplicate-tagbody-tag", Correctness, Default, LanguageSemantics =>
    summary: "Two tags in a single tagbody are EQL.",
    rationale: "A tagbody, including the implicit tagbody of DO or DOLIST, requires distinct tags in its own scope.",
    bad: "(tagbody start (work) start)",
    good: "(tagbody start (work) finish)";

    "suspicious/duplicate-key-parameter", "duplicate-key-parameter", Suspicious, Default, LanguageSemantics =>
    summary: "Two parameters in the same lambda list use the same keyword name.",
    rationale: "The same argument pair supplies both parameters. Repeated keyword names often hide a misspelled explicit keyword.",
    bad: "(defun f (&key ((:mode start)) ((:mode end))) (list start end))",
    good: "(defun f (&key start end) (list start end))";

    "correctness/unknown-common-lisp-symbol", "unknown-common-lisp-symbol", Correctness, Default, LanguageSemantics =>
    summary: "A single-colon reference names a symbol outside the ANSI COMMON-LISP export set.",
    rationale: "The reader requires single-colon names to be exported by the named package.",
    bad: "(cl:consn a b)",
    good: "(cl:cons a b)";

    "correctness/assign-to-constant", "assign-to-constant", Correctness, Default, LanguageSemantics =>
    summary: "Assignment targets a variable declared by DEFCONSTANT.",
    rationale: "The consequences of assigning to a constant variable are undefined. A compiler can embed its value in existing code.",
    bad: "(defconstant +limit+ 8) (setf +limit+ 16)",
    good: "(defparameter *limit* 8) (setf *limit* 16)";

    "correctness/invalid-setq-target", "invalid-setq-target", Correctness, Default, LanguageSemantics =>
    summary: "SETQ or PSETQ receives a compound form where it expects a variable name.",
    rationale: "SETQ and PSETQ assign variables. SETF and PSETF assign generalized places such as array elements or accessors.",
    bad: "(setq (aref items 0) new-item)",
    good: "(setf (aref items 0) new-item)";

    "suspicious/likely-misspelled-operator", "likely-misspelled-operator", Suspicious, Strict, Heuristic =>
    summary: "An unresolved operator differs from an accessible ANSI operator by a small edit.",
    rationale: "A unique near match makes common typos actionable while allowing projects to define their own macros and functions.",
    bad: "(in-package :cl-user) (lenght records)",
    good: "(in-package :cl-user) (length records)";

    "correctness/invalid-class-option", "invalid-class-option", Correctness, Default, LanguageSemantics =>
    summary: "An option in DEFCLASS or DEFINE-CONDITION has invalid fields or a missing value.",
    rationale: "Slot and class options separate syntax from value expressions. A missing option value or invalid allocation kind prevents constructing the intended class.",
    bad: "(defclass box () ((value :initform)))",
    good: "(defclass box () ((value :initform nil)))";

    "correctness/duplicate-class-option", "duplicate-class-option", Correctness, Default, LanguageSemantics =>
    summary: "A single-valued class or slot option is repeated.",
    rationale: "An individual slot permits multiple reader, writer, accessor, and initarg names. Single-valued options and default initarg names must be unique.",
    bad: "(defclass box () ((value :initform 1 :initform 2)))",
    good: "(defclass box () ((value :initform 2)))";

    "correctness/non-symbol-keyword-arg", "non-symbol-keyword-arg", Correctness, Default, LanguageSemantics =>
    summary: "A keyword argument name is a non-symbol literal.",
    rationale: "In an &KEY tail each name must be a symbol, including explicitly named parameters outside the KEYWORD package.",
    bad: "(make-array 3 \"initial-element\" 0)",
    good: "(make-array 3 :initial-element 0)";

    "correctness/invalid-function-designator", "invalid-function-designator", Correctness, Default, StandardLibrary =>
    summary: "FUNCALL or APPLY receives a literal that cannot designate a function.",
    rationale: "These functions require a function object or the symbol naming a global function.",
    bad: "(funcall \"cons\" x y)",
    good: "(funcall #'cons x y)";

    "correctness/invalid-apply-tail", "invalid-apply-tail", Correctness, Default, StandardLibrary =>
    summary: "APPLY receives a last argument that cannot be a proper list.",
    rationale: "APPLY takes the last argument as a list and passes its elements as separate arguments.",
    bad: "(apply #'list #(1 2))",
    good: "(apply #'list '(1 2))";

    "correctness/invalid-format-control", "invalid-format-control", Correctness, Default, StandardLibrary =>
    summary: "A literal FORMAT control string contains an invalid or unbalanced directive.",
    rationale: "FORMAT interprets tilde directives at runtime. An incomplete directive or mismatched group makes the control string invalid.",
    bad: "(format nil \"~{~A~]\" items)",
    good: "(format nil \"~{~A~}\" items)";

    "suspicious/macro-evaluation-in-loop", "macro-evaluation-in-loop", Suspicious, Strict, Dataflow =>
    summary: "A template inserts an expression parameter into a generated iteration body.",
    rationale: "The caller's expression can run once per iteration. Use an outer temporary for an evaluate-once API, or a documented body parameter for intentional repetition.",
    bad: "(defmacro show-three (x) `(dotimes (i 3) (print ,x)))",
    good: "(defmacro show-three (x) (let ((tmp (gensym))) `(let ((,tmp ,x)) (dotimes (i 3) (print ,tmp)))))";

    "suspicious/duplicate-keyword-arg", "duplicate-keyword-arg", Suspicious, Default, LanguageSemantics =>
    summary: "A known &KEY call supplies the same literal keyword more than once.",
    rationale: "The leftmost occurrence supplies the value. Later argument forms still run, even though their values cannot replace the earlier value.",
    bad: "(make-array 3 :initial-element 0 :initial-element 1)",
    good: "(make-array 3 :initial-element 1)";

    "correctness/literal-argument-type", "literal-argument-type", Correctness, Default, StandardLibrary =>
    summary: "A literal cannot satisfy the resolved standard function's argument type.",
    rationale: "Operations such as CAR, CHAR, and AREF require specific kinds of values. A literal proves the mismatch without speculating about program types.",
    bad: "(car 42)",
    good: "(car '(42))";

    "correctness/literal-array-rank", "literal-array-rank", Correctness, Default, StandardLibrary =>
    summary: "AREF has the wrong number of subscripts for a literal rank-one array.",
    rationale: "AREF requires one index per array dimension. String and vector literals always have rank one.",
    bad: "(aref #(1 2) 0 0)",
    good: "(aref #(1 2) 0)";

    "correctness/negative-index", "negative-index", Correctness, Default, StandardLibrary =>
    summary: "An index has a literal negative value.",
    rationale: "Indexing and slicing operations require nonnegative indices even when the sequence length is unknown.",
    bad: "(elt sequence -1)",
    good: "(elt sequence 0)";

    "correctness/invalid-unquote-splicing", "invalid-unquote-splicing", Correctness, Default, LanguageSemantics =>
    summary: "Splicing unquote (,@ or ,.) is used outside a list or vector element position.",
    rationale: "Under ANSI CLHS 2.4.6, ,@ and ,. are only valid inside lists or vectors in a backquote template, and cannot appear as a dotted tail or bare template root.",
    bad: "`(a . ,@tail)",
    good: "`(a ,@tail)";

    "correctness/case-nil-key", "case-nil-key", Correctness, Default, LanguageSemantics =>
    summary: "CASE/ECASE/CCASE clause uses bare NIL or () as its key designator.",
    rationale: "In CASE, a list in key position lists multiple keys. Because NIL is the empty list (), a bare NIL or () clause matches zero keys and can never be taken; write ((nil) ...) to match the symbol NIL.",
    bad: "(case value (nil :none) (otherwise :some))",
    good: "(case value ((nil) :none) (otherwise :some))";

    "correctness/quoted-type-specifier", "quoted-type-specifier", Correctness, Default, LanguageSemantics =>
    summary: "Unevaluated type specifier position is quoted with 'TYPE.",
    rationale: "Forms such as THE, CHECK-TYPE, TYPECASE, HANDLER-CASE, and (DECLARE (TYPE ...)) do not evaluate their type specifier; quoting produces (QUOTE TYPE), which is not a valid type specifier.",
    bad: "(the 'fixnum count)",
    good: "(the fixnum count)";

    "correctness/invalid-declaration", "invalid-declaration", Correctness, Default, LanguageSemantics =>
    summary: "Declaration specifier in DECLARE/DECLAIM is malformed or names an unbound target in IGNORE/IGNORABLE.",
    rationale: "Invalid OPTIMIZE levels, incomplete TYPE/FTYPE specs, or IGNORE/IGNORABLE declarations for variables not bound by the enclosing form violate ANSI CLHS 3.3.4.",
    bad: "(defun f (item) (declare (ignore items)) item)",
    good: "(defun f (item) (declare (ignore item)) nil)";

    "correctness/invalid-defpackage-option", "invalid-defpackage-option", Correctness, Default, LanguageSemantics =>
    summary: "DEFPACKAGE option is malformed, missing required arguments, or repeats a single-valued option.",
    rationale: "DEFPACKAGE permits at most one :DOCUMENTATION and :SIZE option, requires a package name in :IMPORT-FROM/:SHADOWING-IMPORT-FROM, and expects 2-element pairs in :LOCAL-NICKNAMES.",
    bad: "(defpackage #:app (:import-from) (:size :bad))",
    good: "(defpackage #:app (:use #:cl) (:import-from #:util #:helper))";

    // ── Suspicious ───────────────────────────────────────────────────────────
    "suspicious/write-only-variable", "write-only-variable", Suspicious, Default, Dataflow =>
    summary: "Lexical variable is assigned or mutated (SETF, INCF, PUSH) but its value is never read.",
    rationale: "Accumulators or state variables that are updated without ever being read usually indicate a missing return value or broken logic.",
    bad: "(defun total (xs) (let ((sum 0)) (dolist (x xs) (incf sum x))))",
    good: "(defun total (xs) (let ((sum 0)) (dolist (x xs sum) (incf sum x))))";

    "suspicious/overwritten-binding", "overwritten-binding", Suspicious, Default, Dataflow =>
    summary: "Variable's initial or assigned value is unconditionally overwritten before it is ever read.",
    rationale: "Initializing or assigning a value that is immediately replaced wastes work or indicates an accidental assignment to the wrong variable.",
    bad: "(let ((x 10)) (setf x 20) x)",
    good: "(let ((x 20)) x)";

    "suspicious/used-ignored-binding", "used-ignored-binding", Suspicious, Default, Dataflow =>
    summary: "An occurrence reads a variable or calls a local function declared IGNORE.",
    rationale: "IGNORE states that the binding will go unused. IGNORABLE allows both used and unused embodiments of the same declaration.",
    bad: "(defun f (x) (declare (ignore x)) x)",
    good: "(defun f (x) (declare (ignorable x)) x)";

    "suspicious/self-assignment", "self-assignment", Suspicious, Default, Dataflow =>
    summary: "Assignment assigns a variable directly to itself.",
    rationale: "(setf x x) or (setq x x) has no effect and usually indicates a typo.",
    bad: "(setf count count)",
    good: "(setf count new-count)";

    "suspicious/mutated-loop-variable", "mutated-loop-variable", Suspicious, Default, LanguageSemantics =>
    summary: "DOLIST or DOTIMES body assigns to the loop iteration variable.",
    rationale: "Per ANSI CLHS, modifying the iteration variable inside DOLIST/DOTIMES has unintuitive or undefined effects on iteration.",
    bad: "(dotimes (i 10) (incf i))",
    good: "(loop for i from 0 below 10 by 2 do ...)";

    "suspicious/shadowed-handler-clause", "shadowed-handler-clause", Suspicious, Default, StandardLibrary =>
    summary: "HANDLER-CASE clause handles a supertype condition before a more specific subtype.",
    rationale: "HANDLER-CASE dispatches to the first matching clause; placing ERROR or CONDITION before FILE-ERROR or TYPE-ERROR makes the subtype clause unreachable.",
    bad: "(handler-case (work) (error (e) nil) (file-error (e) :retry))",
    good: "(handler-case (work) (file-error (e) :retry) (error (e) nil))";

    "suspicious/shadowed-typecase-clause", "shadowed-typecase-clause", Suspicious, Default, LanguageSemantics =>
    summary: "TYPECASE/ETYPECASE/CTYPECASE clause tests a supertype before a subtype.",
    rationale: "TYPECASE selects the first matching type clause; placing NUMBER before INTEGER or SEQUENCE before LIST makes the subtype branch dead.",
    bad: "(typecase x (number :num) (integer :int))",
    good: "(typecase x (integer :int) (number :num))";

    "suspicious/single-value-in-mvb", "single-value-in-mvb", Suspicious, Default, StandardLibrary =>
    summary: "MULTIPLE-VALUE-BIND binds multiple variables from an expression statically known to return a single value.",
    rationale: "Secondary variables bound from a single-value form are always NIL; use LET instead.",
    bad: "(multiple-value-bind (a b) (+ x y) (list a b))",
    good: "(let ((a (+ x y))) a)";

    "suspicious/prog1-drops-values", "prog1-drops-values", Suspicious, Default, StandardLibrary =>
    summary: "PROG1 discards secondary return values of a multi-value expression.",
    rationale: "PROG1 returns only the primary value of its first form; use MULTIPLE-VALUE-PROG1 when preserving multiple values from VALUES, GETHASH, FLOOR, etc.",
    bad: "(prog1 (gethash key table) (incf *reads*))",
    good: "(multiple-value-prog1 (gethash key table) (incf *reads*))";

    "suspicious/missing-multiple-values", "missing-multiple-values", Suspicious, Default, Dataflow =>
    summary: "A multiple-value binding or assignment requests more values than its producer can return.",
    rationale: "Common Lisp supplies NIL for missing values. A bound that holds for every possible path can reveal a mistaken destructuring or return contract.",
    bad: "(multiple-value-bind (item present source) (gethash key table) (list item present source))",
    good: "(multiple-value-bind (item present) (gethash key table) (list item present))";

    "suspicious/nth-value-out-of-range", "nth-value-out-of-range", Suspicious, Default, Dataflow =>
    summary: "The zero-based NTH-VALUE index is beyond the maximum number of values returned.",
    rationale: "NTH-VALUE evaluates its producer and returns NIL when the index exceeds its returned values. The producer can still have effects.",
    bad: "(nth-value 2 (gethash key table))",
    good: "(nth-value 1 (gethash key table))";

    "suspicious/macro-multiple-evaluation", "macro-multiple-evaluation", Suspicious, Default, Heuristic =>
    summary: "A macro expansion can evaluate the expression supplied for a parameter multiple times.",
    rationale: "An expression such as (incf i) can have effects each time it is evaluated. Evaluate it once in the expansion and reuse the resulting value through a fresh binding.",
    bad: "(defmacro double-it (x) `(+ ,x ,x))",
    good: "(defmacro double-it (x) (let ((val (gensym \"VAL\"))) `(let ((,val ,x)) (+ ,val ,val))))";

    "suspicious/macro-unhygienic-binding", "macro-unhygienic-binding", Suspicious, Default, Heuristic =>
    summary: "DEFMACRO backquote template binds a plain interned symbol, risking variable capture.",
    rationale: "Binding an un-gensymmed symbol like `tmp` inside a macro expansion captures caller variables of the same name. Use GENSYM or uninterned symbols.",
    bad: "(defmacro with-temp (expr &body body) `(let ((tmp ,expr)) ,@body))",
    good: "(defmacro with-temp (expr &body body) (let ((tmp (gensym \"TMP\"))) `(let ((,tmp ,expr)) ,@body)))";

    "suspicious/around-missing-call-next-method", "around-missing-call-next-method", Suspicious, Default, StandardLibrary =>
    summary: ":AROUND method never invokes CALL-NEXT-METHOD.",
    rationale: "An :AROUND method wraps the effective method; omitting CALL-NEXT-METHOD silently suppresses all primary, :BEFORE, and :AFTER methods.",
    bad: "(defmethod save :around ((x doc)) (log-save x))",
    good: "(defmethod save :around ((x doc)) (log-save x) (call-next-method))";

    "suspicious/defclass-literal-initform", "defclass-literal-initform", Suspicious, Default, LanguageSemantics =>
    summary: "DEFCLASS or DEFSTRUCT slot default uses a quoted mutable compound literal.",
    rationale: "A quoted list or vector literal in :INITFORM is shared across every instance created with that default; mutating it corrupts all instances.",
    bad: "(defclass bag () ((items :initform '(1 2))))",
    good: "(defclass bag () ((items :initform (list 1 2))))";

    "suspicious/resource-leak-open", "resource-leak-open", Suspicious, Default, StandardLibrary =>
    summary: "OPEN called outside WITH-OPEN-FILE or UNWIND-PROTECT.",
    rationale: "Raw OPEN calls without WITH-OPEN-FILE or UNWIND-PROTECT leak file descriptors whenever an error or non-local exit occurs.",
    bad: "(let ((s (open path))) (read s))",
    good: "(with-open-file (s path) (read s))";

    "suspicious/redundant-enclosing-condition", "redundant-enclosing-condition", Suspicious, Default, Dataflow =>
    summary: "Inner conditional re-tests a pure condition already established or negated by an enclosing IF/WHEN/UNLESS.",
    rationale: "Checking the same pure test inside its own branch is always true (or false) and indicates dead logic or a typo.",
    bad: "(when (null xs) (if (null xs) :empty :non-empty))",
    good: "(when (null xs) :empty)";

    "suspicious/default-eql-on-string", "default-eql-on-string", Suspicious, Default, StandardLibrary =>
    summary: "Sequence lookup searches for a string literal without specifying :TEST.",
    rationale: "MEMBER, ASSOC, FIND, POSITION, COUNT, REMOVE, DELETE, and ADJOIN default to :TEST #'EQL, which fails on distinct string instances.",
    bad: "(member \"ok\" statuses)",
    good: "(member \"ok\" statuses :test #'string=)";

    "suspicious/setf-non-place", "setf-non-place", Suspicious, Default, LanguageSemantics =>
    summary: "Assignment or mutating macro targets a literal or quoted value instead of a place.",
    rationale: "SETF, SETQ, INCF, DECF, PUSH, and POP require a writable generalized reference (place).",
    bad: "(setf 'x 10)",
    good: "(setf x 10)";

    "suspicious/empty-body", "empty-body", Suspicious, Default, LanguageSemantics =>
    summary: "Binding or conditional form has an empty body and unconditionally returns NIL.",
    rationale: "Forms like (when cond) or (let ((x 1))) almost always indicate a misplaced closing parenthesis.",
    bad: "(when (ready-p)) (start)",
    good: "(when (ready-p) (start))";

    "suspicious/swallowed-condition", "swallowed-condition", Suspicious, Default, CommunityConvention =>
    summary: "Condition handler catches ERROR/CONDITION/T and silently discards it, or uses IGNORE-ERRORS.",
    rationale: "Swallowing all errors hides bugs, masks resource failures, and bypasses Common Lisp's restart-oriented condition system.",
    bad: "(ignore-errors (parse-config path))",
    good: "(handler-case (parse-config path) (parse-error (e) (log-error e) nil))";

    "suspicious/identical-branches", "identical-branches", Suspicious, Default, Dataflow =>
    summary: "Both branches of IF are structurally identical.",
    rationale: "Identical THEN and ELSE expressions indicate a copy-paste error or dead conditional.",
    bad: "(if flag (compute x) (compute x))",
    good: "(compute x)";

    "suspicious/self-comparison", "self-comparison", Suspicious, Default, Dataflow =>
    summary: "Comparison operator compares an expression directly against itself.",
    rationale: "Comparing identical pure operands such as (= x x) or (eq x x) usually indicates a typo for another variable.",
    bad: "(when (equal key key) ...)",
    good: "(when (equal key expected) ...)";

    "suspicious/duplicate-operand", "duplicate-operand", Suspicious, Default, Dataflow =>
    summary: "Operator repeats identical adjacent operands (such as (and x x), (or x x), or (- x x)).",
    rationale: "Repeating the exact same pure expression in AND, OR, or - indicates a copy-paste mistake or dead operation.",
    bad: "(and ready-p ready-p)",
    good: "ready-p";

    "suspicious/constant-condition", "constant-condition", Suspicious, Default, Dataflow =>
    summary: "Conditional branch tests a constant T or NIL.",
    rationale: "Constant conditions make branches unconditionally taken or completely dead.",
    bad: "(if nil (do-a) (do-b))",
    good: "(do-b)";

    "suspicious/unreachable-code", "unreachable-code", Suspicious, Default, Dataflow =>
    summary: "Expressions appear after an unconditional control transfer (ERROR, RETURN, RETURN-FROM, GO, THROW).",
    rationale: "Forms following an unconditional exit in the same sequence body can never execute.",
    bad: "(defun f (x) (error \"bad\") (+ x 1))",
    good: "(defun f () (error \"bad\"))";

    "suspicious/discarded-pure-expression", "discarded-pure-expression", Suspicious, Strict, Dataflow =>
    summary: "Pure expression in non-tail position has its return value discarded.",
    rationale: "Arithmetic, pure comparisons, and list constructors have no side effects; discarding their result usually means a missing SETF, LET binding, or misplaced parenthesis.",
    bad: "(defun f (x) (+ x 1) (* x 2))",
    good: "(defun f (x) (let ((y (+ x 1))) (* y 2)))";

    "suspicious/duplicate-cond-test", "duplicate-cond-test", Suspicious, Default, Dataflow =>
    summary: "COND repeats a test expression already covered by an earlier clause.",
    rationale: "If the earlier test is pure, the later clause can never be selected.",
    bad: "(cond ((null x) 1) ((null x) 2))",
    good: "(cond ((null x) 1) (t 2))";

    "suspicious/contradictory-boolean", "contradictory-boolean", Suspicious, Default, Dataflow =>
    summary: "Boolean combination contains contradictory or mutually exclusive pure tests on the same value.",
    rationale: "Combining a predicate with its negation (such as (and x (not x))) or disjoint type checks (such as (and (null x) (consp x))) is always false or always true.",
    bad: "(and (stringp x) (numberp x))",
    good: "(or (stringp x) (numberp x))";

    // ── Security ─────────────────────────────────────────────────────────────
    "security/unsafe-read-eval", "unsafe-read-eval", Security, Default, StandardLibrary =>
    summary: "READ or READ-FROM-STRING called without binding *READ-EVAL* to NIL.",
    rationale: "When *READ-EVAL* is non-NIL (the default), the #. reader macro executes arbitrary Lisp forms during reading, causing code injection on untrusted input.",
    bad: "(read-from-string user-input)",
    good: "(with-standard-io-syntax (let ((*read-eval* nil)) (read-from-string user-input)))";

    "security/read-time-eval", "read-time-eval", Security, Strict, LanguageSemantics =>
    summary: "Source uses #. read-time evaluation.",
    rationale: "#. executes arbitrary Lisp code during READ before macro expansion or compilation, posing code-execution risks and breaking pure source analysis.",
    bad: "(list #.(compute-const))",
    good: "(load-time-value (compute-const))";

    "security/eval-usage", "eval-usage", Security, Strict, CommunityConvention =>
    summary: "Runtime call to EVAL.",
    rationale: "EVAL executes in the null lexical environment, blocks static analysis and compilation optimizations, and introduces code injection risks. Prefer closures, FUNCALL/APPLY, hash tables, or macros.",
    bad: "(eval `(,op ,x ,y))",
    good: "(funcall (resolve-op op) x y)";

    "security/format-non-constant-control", "format-non-constant-control", Security, Default, StandardLibrary =>
    summary: "FORMAT control string is a non-constant expression.",
    rationale: "Common Lisp FORMAT strings support ~/fn/ function calls and ~{~} loops; passing user-controlled strings as the control argument allows arbitrary function invocation or denial of service.",
    bad: "(format t user-message)",
    good: "(format t \"~A\" user-message)";

    "security/intern-without-package", "intern-without-package", Security, Default, StandardLibrary =>
    summary: "INTERN called with one argument implicitly interns into the dynamic *PACKAGE*.",
    rationale: "Single-argument INTERN depends on whatever *PACKAGE* happens to be active at runtime and pollutes package symbol tables; pass an explicit package or use FIND-SYMBOL.",
    bad: "(intern name)",
    good: "(find-symbol name :my-pkg) ; or (intern name :my-pkg)";

    // ── Complexity ───────────────────────────────────────────────────────────
    "complexity/if-boolean-branch", "if-boolean-branch", Complexity, Default, LanguageSemantics =>
    summary: "IF trivially returns T and NIL booleans.",
    rationale: "(if cond nil t) is always equivalent to (not cond), and (if pred t nil) on a boolean predicate is redundant.",
    bad: "(if (zerop n) nil t)",
    good: "(not (zerop n))";

    "complexity/double-negation", "double-negation", Complexity, Default, LanguageSemantics =>
    summary: "Double negation in a truth test or around a known T/NIL result.",
    rationale: "A truth test only uses truthiness. In a value context, double negation can intentionally normalize a value to T or NIL.",
    bad: "(when (not (not items)) (run items))",
    good: "(when items (run items))";

    "complexity/redundant-progn", "redundant-progn", Complexity, Strict, LanguageSemantics =>
    summary: "Unnecessary PROGN wrapper around a single form or inside an implicit-progn body.",
    rationale: "DEFUN, LET, WHEN, UNLESS, COND clauses, and LAMBDA already evaluate an implicit PROGN body.",
    bad: "(when ok (progn (step-1) (step-2)))",
    good: "(when ok (step-1) (step-2))";

    "complexity/redundant-let", "redundant-let", Complexity, Strict, LanguageSemantics =>
    summary: "LET/LET* has empty bindings, single-binding LET*, independent LET* bindings, immediate return of a single binding, or two-step NIL+SETQ init.",
    rationale: "Use LET* when a later initializer needs an earlier binding. If you remove a binding that is returned immediately, preserve its single primary value with VALUES when needed.",
    bad: "(let* ((x (compute))) x)",
    good: "(values (compute))";

    "complexity/nested-and-or", "nested-and-or", Complexity, Default, LanguageSemantics =>
    summary: "AND/OR has only one argument or nests another AND/OR directly inside itself.",
    rationale: "AND and OR are associative variadic short-circuit operators and can be flattened.",
    bad: "(and a (and b c))",
    good: "(and a b c)";

    "complexity/deep-if-chain", "deep-if-chain", Complexity, Default, CommunityConvention =>
    summary: "Deeply nested IF-else-IF chain should use COND.",
    rationale: "Chained IF branches increase rightward drift; COND expresses multi-branch conditions flatly.",
    bad: "(if a 1 (if b 2 (if c 3 4)))",
    good: "(cond (a 1) (b 2) (c 3) (t 4))";

    "complexity/if-not-with-else", "if-not-with-else", Complexity, Default, CommunityConvention =>
    summary: "IF tests a negated condition (NOT ...) while providing both branches.",
    rationale: "Swapping the THEN and ELSE branches eliminates the negation and makes control flow easier to read.",
    bad: "(if (not empty-p) (process) :done)",
    good: "(if empty-p :done (process))";

    "complexity/if-to-and-or", "if-to-and-or", Complexity, Default, LanguageSemantics =>
    summary: "IF expression duplicates a simple condition as its THEN branch.",
    rationale: "(if var var fallback) is idiomatically written (or var fallback) when VAR is pure.",
    bad: "(if cached cached (compute))",
    good: "(or cached (compute))";

    "complexity/collapsible-conditional", "collapsible-conditional", Complexity, Strict, LanguageSemantics =>
    summary: "Nested single-branch WHEN or UNLESS forms can be combined with AND or OR.",
    rationale: "Collapsing (when a (when b ...)) into (when (and a b) ...) and (unless a (unless b ...)) into (unless (or a b) ...) preserves short-circuit evaluation and return values while reducing nesting.",
    bad: "(when ready-p (when valid-p (run)))",
    good: "(when (and ready-p valid-p) (run))";

    // ── Performance ──────────────────────────────────────────────────────────
    "perf/length-zero-comparison", "length-zero-comparison", Perf, Default, StandardLibrary =>
    summary: "Testing whether a known list is empty by measuring its length.",
    rationale: "LENGTH walks the list in O(n) time. NULL or CONSP tests emptiness in constant time. Vectors and sequences of unknown type are left alone.",
    bad: "(when (= (length (the list items)) 0) :empty)",
    good: "(when (null items) :empty)";

    "perf/foreign-binding-in-loop", "foreign-binding-in-loop", Perf, Default, ZkclExtension =>
    summary: "A loop repeatedly prepares the same fixed ZK-FFI host binding.",
    rationale: "A :HOST address is fixed for a runtime. Reusing the prepared function avoids symbol lookup and ABI preparation at each call.",
    bad: "(dotimes (i n) (funcall (zk-ffi:bind-function :host \"tick\" :void :int) i))",
    good: "(let ((tick (zk-ffi:bind-function :host \"tick\" :void :int))) (dotimes (i n) (funcall tick i)))";

    "perf/append-in-loop", "append-in-loop", Perf, Default, StandardLibrary =>
    summary: "Appending to the end of a list inside a loop.",
    rationale: "Repeated (setf acc (append acc ...)) or (nconc acc ...) inside a loop is O(n^2). Use PUSH + NREVERSE or LOOP ... COLLECT.",
    bad: "(dolist (x xs acc) (setf acc (append acc (list x))))",
    good: "(loop for x in xs collect x)";

    "perf/string-concat-in-loop", "string-concat-in-loop", Perf, Default, StandardLibrary =>
    summary: "Repeatedly calling (CONCATENATE 'STRING ...) inside a loop.",
    rationale: "Concatenating strings in a loop reallocates O(n^2) bytes; use WITH-OUTPUT-TO-STRING.",
    bad: "(dolist (s parts out) (setf out (concatenate 'string out s)))",
    good: "(with-output-to-string (out) (dolist (s parts) (write-string s out)))";

    "perf/apply-call-arguments-limit", "apply-call-arguments-limit", Perf, Default, LanguageSemantics =>
    summary: "APPLY on a variadic accumulator (# '+, #'*, #'max, #'min, #'append) may exceed CALL-ARGUMENTS-LIMIT.",
    rationale: "ANSI Common Lisp only guarantees CALL-ARGUMENTS-LIMIT >= 50. On long lists, APPLY fails or copies the entire list onto the call stack; use REDUCE or MAPCAN/LOOP.",
    bad: "(apply #'+ numbers)",
    good: "(reduce #'+ numbers)";

    "perf/apply-fresh-list", "apply-fresh-list", Perf, Default, StandardLibrary =>
    summary: "APPLY immediately unpacks a newly constructed, fixed-length argument list.",
    rationale: "FUNCALL can pass these arguments directly and preserves the evaluation order and identity of the function designator.",
    bad: "(apply write-entry stream (list key value))",
    good: "(funcall write-entry stream key value)";

    "perf/materialized-multiple-values", "materialized-multiple-values", Perf, Default, StandardLibrary =>
    summary: "A temporary list of multiple values is used only to retrieve values from it.",
    rationale: "NTH-VALUE selects one value, including NIL for a missing value. Returning all values directly preserves their count and order.",
    bad: "(second (multiple-value-list (gethash key cache)))",
    good: "(nth-value 1 (gethash key cache))";

    "perf/indexed-list-loop", "indexed-list-loop", Perf, Default, Dataflow =>
    summary: "A DOTIMES counter repeatedly indexes the list that determines the loop length.",
    rationale: "NTH starts at the head for each index. DOLIST walks each cons cell once. Review mutation and closures before changing the iteration binding.",
    bad: "(dotimes (i (length rows)) (let ((row (nth i rows))) (write-entry row)))",
    good: "(dolist (row rows) (write-entry row))";

    "perf/mapcar-result-discarded", "mapcar-result-discarded", Perf, Default, StandardLibrary =>
    summary: "MAPCAR or MAPLIST builds a result list that is discarded.",
    rationale: "MAPC and MAPL preserve the corresponding calls and their order without constructing a list of return values.",
    bad: "(progn (mapcar #'print items) :done)",
    good: "(progn (mapc #'print items) :done)";

    "perf/map-result-discarded", "map-result-discarded", Perf, Default, StandardLibrary =>
    summary: "MAP builds a list or vector that is discarded.",
    rationale: "A NIL result type keeps the calls and their order and skips collecting the return values. Specialized result type constraints are left intact.",
    bad: "(progn (map 'vector #'print items) :done)",
    good: "(progn (map nil #'print items) :done)";

    "perf/count-filtered-sequence", "count-filtered-sequence", Perf, Default, StandardLibrary =>
    summary: "The result of REMOVE-IF-NOT is used only to count matching elements.",
    rationale: "COUNT-IF counts matching elements without materializing a filtered sequence. Review the predicate and traversal when the predicate can have effects.",
    bad: "(length (remove-if-not #'symbolp items))",
    good: "(count-if #'symbolp items)";

    "perf/length-of-sequence-copy", "length-of-sequence-copy", Perf, Default, StandardLibrary =>
    summary: "LENGTH is the only consumer of a sequence copy or reversal.",
    rationale: "COPY-SEQ and REVERSE preserve the length of a proper sequence. Taking its length directly avoids allocation and an extra traversal of lists.",
    bad: "(length (reverse (copy-seq items)))",
    good: "(length items)";

    "perf/car-of-remove-if-not", "car-of-remove-if-not", Perf, Default, StandardLibrary =>
    summary: "Only the first element of a filtered list is used.",
    rationale: "FIND-IF can return the first matching element without collecting the remaining matches. Review predicate effects because an early stop can change which elements are tested.",
    bad: "(first (remove-if-not #'evenp numbers))",
    good: "(find-if #'evenp numbers)";

    "perf/first-of-sort", "first-of-sort", Perf, Default, StandardLibrary =>
    summary: "Taking FIRST/CAR of SORT performs an O(n log n) sort just to find an extremum.",
    rationale: "Use REDUCE or LOOP MINIMIZE/MAXIMIZE for O(n) single-pass selection without mutating the sequence.",
    bad: "(first (sort (copy-list scores) #'>))",
    good: "(reduce #'max scores)";

    "perf/string-comparison-subseq", "string-comparison-subseq", Perf, Default, StandardLibrary =>
    summary: "String comparison slices an operand with SUBSEQ instead of passing :START/:END bounds.",
    rationale: "STRING=, STRING-EQUAL, STRING<, and related functions natively accept :START1/:END1 and :START2/:END2 without allocating a substring copy.",
    bad: "(string= (subseq line 0 4) \"HTTP\")",
    good: "(string= line \"HTTP\" :end1 4)";

    "perf/string-equal-case-fold", "string-equal-case-fold", Perf, Default, StandardLibrary =>
    summary: "Case-sensitive string comparison normalizes operands with STRING-UPCASE or STRING-DOWNCASE instead of using STRING-EQUAL.",
    rationale: "STRING-EQUAL compares characters case-insensitively without allocating temporary uppercased or lowercased strings.",
    bad: "(string= (string-downcase cmd) \"quit\")",
    good: "(string-equal cmd \"quit\")";

    // ── Style ────────────────────────────────────────────────────────────────
    "style/cross-dialect-hallucination", "cross-dialect-hallucination", Style, Default, LanguageSemantics =>
    summary: "Call to a Scheme, Clojure, or Emacs Lisp symbol that does not exist in ANSI Common Lisp.",
    rationale: "Foreign Lisp symbols (define, set!, begin, null?, string-append, filter, foldl, defn, hash-ref) do not exist in ANSI Common Lisp.",
    bad: "(define (square x) (* x x))",
    good: "(defun square (x) (* x x))";

    "style/if-to-when", "if-to-when", Style, Default, CommunityConvention =>
    summary: "One-armed IF or discarded (IF cond expr NIL) can be written as WHEN.",
    rationale: "WHEN clearly expresses single-branch conditional execution and implicitly allows multiple body forms without PROGN.",
    bad: "(if valid (process item))",
    good: "(when valid (process item))";

    "style/if-to-unless", "if-to-unless", Style, Default, CommunityConvention =>
    summary: "IF with NIL then-branch or negated one-armed IF can be written as UNLESS.",
    rationale: "UNLESS avoids awkward NIL placeholders and nested NOT wrappers.",
    bad: "(if (not ready) (wait))",
    good: "(unless ready (wait))";

    "style/progn-in-conditional", "progn-in-conditional", Style, Default, CommunityConvention =>
    summary: "One-armed IF wraps its body in PROGN instead of using WHEN, or AND wraps PROGN.",
    rationale: "WHEN has an implicit PROGN body, eliminating both the PROGN wrapper and the missing-else IF.",
    bad: "(if ok (progn (step-1) (step-2)))",
    good: "(when ok (step-1) (step-2))";

    "style/when-or-unless-not", "when-or-unless-not", Style, Default, CommunityConvention =>
    summary: "WHEN/UNLESS wraps a negated condition (NOT ...).",
    rationale: "(when (not x) ...) is UNLESS, and (unless (not x) ...) is WHEN.",
    bad: "(when (not done) (step))",
    good: "(unless done (step))";

    "style/cond-simplification", "cond-simplification", Style, Default, CommunityConvention =>
    summary: "COND form can be simplified to WHEN, IF, PROGN, or has a redundant trailing (T NIL).",
    rationale: "Single-clause COND, two-clause COND with T fallback, or trailing (t nil) adds unnecessary nesting; COND already returns NIL when no clause matches.",
    bad: "(cond ((> x 0) (log x) x))",
    good: "(when (> x 0) (log x) x)";

    "style/null-equality-comparison", "null-equality-comparison", Style, Default, CommunityConvention =>
    summary: "Explicit EQ/EQL/EQUAL comparison against NIL.",
    rationale: "Comparing against NIL with eq/eql/equal is idiomatic only as (null x) for lists/values or (not x) for booleans.",
    bad: "(if (eq x nil) 0 x)",
    good: "(if (null x) 0 x)";

    "style/redundant-lambda", "redundant-lambda", Style, Default, CommunityConvention =>
    summary: "Immediately invoked lambda, identity lambda, constant lambda, or eta-reducible lambda.",
    rationale: "Prefer LET for immediate bindings, #'IDENTITY for (lambda (x) x), (constantly c) for constant lambdas, and #'fn for single-argument forwarding.",
    bad: "(mapcar (lambda (x) (string-upcase x)) names)",
    good: "(mapcar #'string-upcase names)";

    "style/unnecessary-funcall", "unnecessary-funcall", Style, Default, CommunityConvention =>
    summary: "FUNCALL called with a static function designator #'FN or 'FN.",
    rationale: "Direct call syntax (fn arg...) is clearer and avoids indirection.",
    bad: "(funcall #'1+ n)",
    good: "(1+ n)";

    "style/useless-format-nil", "useless-format-nil", Style, Default, StandardLibrary =>
    summary: "FORMAT NIL called with a plain string literal containing no ~ directives.",
    rationale: "When there are no format directives, FORMAT NIL simply allocates a copy of the literal string.",
    bad: "(format nil \"hello\")",
    good: "\"hello\"";

    "style/incf-decf", "incf-decf", Style, Default, CommunityConvention =>
    summary: "Manual (+ x 1)/(- x 1) reassignment instead of INCF/DECF.",
    rationale: "INCF and DECF are standard, concise, and evaluate place subforms only once.",
    bad: "(setf count (+ count 1))",
    good: "(incf count)";

    "style/unused-variable", "unused-variable", Style, Default, Dataflow =>
    summary: "Lexical variable or parameter is bound but never referenced.",
    rationale: "Unused bindings indicate dead code or typos; prefix intentional unused names with _ or add (declare (ignore ...)).",
    bad: "(defun f (used unused) used)",
    good: "(defun f (used _unused) used)";

    "style/unused-local-function", "unused-local-function", Style, Strict, Dataflow =>
    summary: "Local function defined in FLET or LABELS is never called or referenced.",
    rationale: "Dead local functions add noise and can be removed or declared IGNORABLE.",
    bad: "(flet ((unused-helper (x) x)) 42)",
    good: "42";

    "style/unused-block-label", "unused-block-label", Style, Strict, Dataflow =>
    summary: "Explicitly named BLOCK is never targeted by RETURN-FROM.",
    rationale: "An unreferenced non-NIL block label adds nesting without serving as a non-local exit target.",
    bad: "(block done (compute))",
    good: "(compute)";

    "style/unused-tag", "unused-tag", Style, Strict, Dataflow =>
    summary: "TAGBODY tag is never targeted by GO.",
    rationale: "Unused tags in TAGBODY or PROG clutter control flow and often remain after refactoring.",
    bad: "(tagbody retry (work))",
    good: "(progn (work) nil)";

    "style/numeric-sign-predicate", "numeric-sign-predicate", Style, Strict, CommunityConvention =>
    summary: "Numeric comparison against zero can be written with ZEROP, PLUSP, or MINUSP.",
    rationale: "Using ZEROP, PLUSP, or MINUSP expresses sign and zero tests directly and idiomatically.",
    bad: "(= count 0)",
    good: "(zerop count)";

    "style/unused-package-import", "unused-package-import", Style, Default, Dataflow =>
    summary: "DEFPACKAGE :IMPORT-FROM or :LOCAL-NICKNAMES entry is never referenced in the package.",
    rationale: "Stale imports and nicknames pollute the package namespace and mislead readers about dependencies.",
    bad: "(defpackage :app (:use :cl) (:import-from :util :unused-fn))\n(in-package :app)\n(defun main () 1)",
    good: "(defpackage :app (:use :cl))\n(in-package :app)\n(defun main () 1)";

    "style/special-earmuffs", "special-earmuffs", Style, Strict, CommunityConvention =>
    summary: "Special variable declared via DEFVAR, DEFPARAMETER, or DECLAIM SPECIAL lacks *earmuffs*.",
    rationale: "DEFVAR and DEFPARAMETER globally proclaim a symbol SPECIAL, turning every subsequent LET binding of that symbol anywhere in the program into dynamic binding. *Earmuffs* make dynamic scope unmistakable.",
    bad: "(defparameter default-timeout 30)",
    good: "(defparameter *default-timeout* 30)";

    "style/constant-pluses", "constant-pluses", Style, Strict, CommunityConvention =>
    summary: "DEFCONSTANT name does not follow the +plus-earmuffs+ convention.",
    rationale: "Wrapping constants in +...+ distinguishes inlined constants from lexical and special variables.",
    bad: "(defconstant max-retries 3)",
    good: "(defconstant +max-retries+ 3)";

    "style/lexical-earmuffs", "lexical-earmuffs", Style, Strict, CommunityConvention =>
    summary: "Lexical variable or parameter uses *earmuffs* without being declared SPECIAL.",
    rationale: "*Earmuffs* signal dynamic variables; using them on lexical bindings misleads readers about dynamic extent.",
    bad: "(defun f (*x*) (+ *x* 1))",
    good: "(defun f (x) (+ x 1))";

    "style/mixed-optional-and-key", "mixed-optional-and-key", Style, Strict, CommunityConvention =>
    summary: "Lambda list combines both &OPTIONAL and &KEY parameters.",
    rationale: "Mixing &OPTIONAL and &KEY forces callers to supply every optional positional argument before passing any keyword argument, causing brittle call sites.",
    bad: "(defun connect (host &optional port &key tls) ...)",
    good: "(defun connect (host &key (port 80) tls) ...)";

    "style/no-allow-other-keys", "no-allow-other-keys", Style, Strict, CommunityConvention =>
    summary: "Lambda list uses &ALLOW-OTHER-KEYS, disabling keyword checking at call sites.",
    rationale: "&ALLOW-OTHER-KEYS silently accepts misspelled keyword arguments unless explicitly intended for forwarding.",
    bad: "(defun make-widget (&key width &allow-other-keys) width)",
    good: "(defun make-widget (&key width height) (list width height))";

    "style/obsolete-aux", "obsolete-aux", Style, Strict, CommunityConvention =>
    summary: "Lambda list uses &AUX instead of LET* inside the function body.",
    rationale: "&AUX mixes implementation-local bindings into the public signature; LET* keeps the lambda list clean.",
    bad: "(defun f (x &aux (y (* x 2))) (+ x y))",
    good: "(defun f (x) (let ((y (* x 2))) (+ x y)))";

    "style/deprecated-cl-feature", "deprecated-cl-feature", Style, Strict, LanguageSemantics =>
    summary: "Uses an ANSI Common Lisp §1.8 deprecated or obsolete feature (:TEST-NOT, GENTEMP, SET, PROG/PROG*, RPLACA/RPLACD).",
    rationale: "Use :TEST with #'COMPLEMENT instead of :TEST-NOT, GENSYM instead of GENTEMP, (SETF (SYMBOL-VALUE ...)) instead of SET, (SETF (CAR/CDR ...)) instead of RPLACA/RPLACD, and LET/LOOP/BLOCK instead of PROG.",
    bad: "(remove x xs :test-not #'eql)",
    good: "(remove x xs :test (complement #'eql))";

    "style/defpackage-use-non-cl", "defpackage-use-non-cl", Style, Strict, CommunityConvention =>
    summary: "DEFPACKAGE :USEs a non-COMMON-LISP package.",
    rationale: ":USE pulls all exported symbols of another package into your namespace and risks silent symbol conflicts whenever that package adds exports. Prefer :IMPORT-FROM or :LOCAL-NICKNAMES.",
    bad: "(defpackage :app (:use :cl :alexandria))",
    good: "(defpackage :app (:use :cl) (:local-nicknames (:a :alexandria)))";

    "style/defpackage-interned-symbol", "defpackage-interned-symbol", Style, Strict, CommunityConvention =>
    summary: "DEFPACKAGE uses a plain or quoted symbol instead of an uninterned #:symbol or keyword.",
    rationale: "Plain or quoted symbols in DEFPACKAGE intern symbols into whatever package reads the DEFPACKAGE form (usually CL-USER). Use #:symbol or :symbol.",
    bad: "(defpackage my-app (:use cl) (:export start))",
    good: "(defpackage #:my-app (:use #:cl) (:export #:start))";

    "style/double-colon-internal-access", "double-colon-internal-access", Style, Strict, CommunityConvention =>
    summary: "Double colon (PKG::SYM) accesses an internal symbol of another package.",
    rationale: "Reaching into another package's internal symbols bypasses its exported API contract.",
    bad: "(other-pkg::internal-helper x)",
    good: "(other-pkg:exported-api x)";

    "style/is-prefix-predicate", "is-prefix-predicate", Style, Strict, CommunityConvention =>
    summary: "Function name uses an IS-* prefix instead of Common Lisp's -P suffix convention.",
    rationale: "In Common Lisp, predicates are conventionally suffixed with -P (or P for single-word names like EVENP), whereas IS-* is usually carried over from other languages.",
    bad: "(defun is-valid (x) (plusp x))",
    good: "(defun valid-p (x) (plusp x))";

    "style/condition-missing-report", "condition-missing-report", Style, Strict, StandardLibrary =>
    summary: "DEFINE-CONDITION lacks a :REPORT option.",
    rationale: "Without :REPORT, condition objects render as opaque #<MY-ERROR ...> strings instead of readable diagnostics.",
    bad: "(define-condition bad-input (error) ((value :initarg :value)))",
    good: "(define-condition bad-input (error) ((value :initarg :value)) (:report (lambda (c s) (format s \"Bad input: ~S\" (slot-value c 'value)))))";

    "style/defclass-inaccessible-slot", "defclass-inaccessible-slot", Style, Strict, CommunityConvention =>
    summary: "DEFCLASS slot defines neither :INITARG nor :ACCESSOR/:READER/:WRITER.",
    rationale: "Slots with no initarg and no accessor force callers to use SLOT-VALUE and make object APIs brittle.",
    bad: "(defclass user () ((name)))",
    good: "(defclass user () ((name :initarg :name :accessor user-name)))";

    // ── Pedantic (enabled in All / Pedantic preset) ──────────────────────────
    "pedantic/missing-docstring", "missing-docstring", Pedantic, All, CommunityConvention =>
    summary: "Top-level definition lacks a documentation string.",
    rationale: "Docstrings power DESCRIBE, DOCUMENTATION, and IDE hover tooltips across Common Lisp systems.",
    bad: "(defun parse-header (bytes) ...)",
    good: "(defun parse-header (bytes)\n  \"Parse a binary header from BYTES.\"\n  ...)";

    "pedantic/deprecated-if-not", "deprecated-if-not", Pedantic, All, LanguageSemantics =>
    summary: "Uses an ANSI CLHS §1.8.1 *-IF-NOT function (e.g. REMOVE-IF-NOT).",
    rationale: "While still common in practice, *-IF-NOT variants are officially deprecated in ANSI CL in favor of positive *-IF with COMPLEMENT.",
    bad: "(remove-if-not #'oddp xs)",
    good: "(remove-if (complement #'oddp) xs)";

    "pedantic/plus-one-minus-one", "plus-one-minus-one", Pedantic, All, CommunityConvention =>
    summary: "Binary (+ x 1), (+ 1 x), or (- x 1) can be written as 1+ or 1-.",
    rationale: "1+ and 1- express increment and decrement idiomatically and clearly.",
    bad: "(+ idx 1)",
    good: "(1+ idx)";

    "pedantic/deep-car-cdr", "deep-car-cdr", Pedantic, All, CommunityConvention =>
    summary: "Deep CAR/CDR composition (3 or 4 A/D letters) obscures data access.",
    rationale: "Deep compositions like CADDR or CDDDR are hard to read; prefer SECOND/THIRD/FOURTH, NTH, or DESTRUCTURING-BIND.",
    bad: "(cadddr record)",
    good: "(fourth record) ; or (destructuring-bind (a b c d) record ...)";

    "pedantic/high-complexity", "high-complexity", Pedantic, All, Heuristic =>
    summary: "Function exceeds complexity or nesting threshold.",
    rationale: "Deeply nested or heavily branched functions are difficult to test and maintain; split into small helper functions.",
    bad: "(defun huge (x) (if a (if b (cond ...))))",
    good: "(defun huge (x) (handle-a x))";

    "pedantic/shadowed-cl-symbol", "shadowed-cl-symbol", Pedantic, All, LanguageSemantics =>
    summary: "Definition shadows an external COMMON-LISP symbol.",
    rationale: "Redefining or locally shadowing standard CL symbols like LIST, COUNT, or ERROR can confuse readers and violate package locks if not explicitly shadowed.",
    bad: "(flet ((list (x) x)) (list 1))",
    good: "(flet ((wrap-item (x) x)) (wrap-item 1))";

    "pedantic/stale-suppression", "stale-suppression", Pedantic, All, Heuristic =>
    summary: "Inline zkcl:allow / suppress directive did not match any active diagnostic.",
    rationale: "Keeping stale lint suppressions hides real intent and accumulates dead annotations after refactoring.",
    bad: "(+ 1 2) ; zkcl:allow if-to-when",
    good: "(+ 1 2)";
}

pub(crate) fn find_rule(query: &str) -> Option<&'static RuleDescriptor> {
    let q = query.trim();
    RULES
        .iter()
        .find(|rule| rule.id.eq_ignore_ascii_case(q) || rule.short_name.eq_ignore_ascii_case(q))
}

pub(super) fn find_rule_id(id: &str) -> Option<&'static RuleDescriptor> {
    static BY_ID: LazyLock<FxHashMap<&str, &RuleDescriptor>> =
        LazyLock::new(|| RULES.iter().map(|rule| (rule.id, rule)).collect());
    BY_ID.get(id).copied()
}
