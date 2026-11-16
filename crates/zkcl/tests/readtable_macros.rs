use zkcl::Runtime;

fn check(runtime: &Runtime, source: &str) {
    let values = runtime
        .eval(source)
        .unwrap_or_else(|error| panic!("{source}: {error}"));
    assert_eq!(values.len(), 1, "{source}");
    assert!(!values[0].is_nil(), "{source}");
}

#[test]
fn copy_readtable_preserves_standard_macro_character_entries() {
    let runtime = &Runtime::start().unwrap();
    check(
        runtime,
        r#"(and
             (loop for c across "()';\"`,#" always
               (multiple-value-bind (fn non-terminating)
                   (get-macro-character c)
                 (and fn
                      (if (char= c #\#) non-terminating
                          (not non-terminating)))))
             (let ((copy (copy-readtable)))
               (loop for c across "()';\"`,#" always
                 (multiple-value-bind (fn flag) (get-macro-character c)
                   (multiple-value-bind (copy-fn copy-flag)
                       (get-macro-character c copy)
                     (and (eq fn copy-fn)
                          (eq (if flag t nil) (if copy-flag t nil)))))))
             (not (get-macro-character #\a))
             (not (get-macro-character #\|)))"#,
    );
}

#[test]
fn macro_character_termination_controls_token_scanning() {
    let runtime = &Runtime::start().unwrap();
    check(
        runtime,
        r#"(let ((*package* (find-package :cl-user)))
             (let ((*readtable* (copy-readtable nil)))
               (flet ((answer (stream char)
                        (declare (ignore stream char))
                        17))
                 (set-macro-character #\? #'answer nil)
                 (and (eql (read-from-string "?!") 17)
                      (eql (read-from-string "!?") '!)))))"#,
    );
    check(
        runtime,
        r#"(let ((*package* (find-package :cl-user)))
             (let ((*readtable* (copy-readtable nil)))
               (flet ((answer (stream char)
                        (declare (ignore stream char))
                        17))
                 (set-macro-character #\? #'answer t)
                 (and (eql (read-from-string "?!") 17)
                      (string= (symbol-name (read-from-string "!?")) "!?")))))"#,
    );
    check(
        runtime,
        r#"(let ((*package* (find-package :cl-user)))
             (let ((*readtable* (copy-readtable nil)))
               (set-macro-character #\? (lambda (stream char)
                                           (declare (ignore stream))
                                           (if (char= char #\?) 23 0))
                                    nil)
               (eql (read-from-string "?x") 23)))"#,
    );
}

#[test]
fn set_syntax_from_char_reuses_the_source_character_syntax() {
    let runtime = &Runtime::start().unwrap();
    check(
        runtime,
        r#"(let ((*readtable* (copy-readtable nil))
                 (*package* (find-package :cl-user)))
             (and
               (set-syntax-from-char #\@ #\()
               (equal (read-from-string "@1 2)") '(1 2))
               (set-syntax-from-char #\^ #\')
               (equal (read-from-string "^x") '(quote x))
               (set-syntax-from-char #\! #\")
               (string= (read-from-string "!abc!") "abc")
               (set-syntax-from-char #\] #\))
               (handler-case (progn (read-from-string "]") nil)
                 (reader-error () t))))"#,
    );
    check(
        runtime,
        r#"(let ((*readtable* (copy-readtable nil))
                 (*package* (find-package :cl-user)))
             (and
               (set-syntax-from-char #\@ #\\)
               (string= (symbol-name (read-from-string "@Z")) "Z")
               (set-syntax-from-char #\^ #\|)
               (string= (symbol-name (read-from-string "^Mix^")) "Mix")
               (set-syntax-from-char #\( #\X)
               (string= (symbol-name (read-from-string "(")) "(")
               (set-syntax-from-char #\; #\X)
               (string= (symbol-name (read-from-string ";")) ";")))"#,
    );
}

#[test]
fn read_delimited_list_consumes_the_delimiter() {
    let runtime = &Runtime::start().unwrap();
    check(
        runtime,
        r##"(let ((*readtable* (copy-readtable nil)))
              (set-macro-character #\[ (lambda (s c) (declare (ignore c))
                                         (read-delimited-list #\] s t)))
              (set-macro-character #\] (get-macro-character #\) nil))
              (set-macro-character #\! (lambda (s c) (declare (ignore c))
                                         (read-char s) (values)))
              (with-input-from-string (s "[[a] !] #| ] |# (\"]\") |x]y| ]z")
                (and (equal (read s) '((a) ("]") |x]y|))
                     (char= (read-char s) #\z))))"##,
    );
    check(
        runtime,
        r#"(equal (with-input-from-string (s "a] ]") (read-delimited-list #\] s))
                  '(|A]|))"#,
    );
    check(
        runtime,
        r#"(equal
             (multiple-value-list
               (with-input-from-string (s "1 2 3)X")
                 (values (read-delimited-list #\) s) (read-char s))))
             '((1 2 3) #\X))"#,
    );
    check(
        runtime,
        r#"(equal
             (with-input-from-string (s "1 2 3 ]")
               (read-delimited-list #\] s))
             '(1 2 3))"#,
    );
}

#[test]
fn reader_macros_run_during_read_suppress() {
    let runtime = &Runtime::start().unwrap();
    check(
        runtime,
        r##"(equal
             (let ((*read-suppress* t))
               (multiple-value-list (read-from-string "#garbage")))
             '(nil 8))"##,
    );
    check(
        runtime,
        r##"(let ((*readtable* (copy-readtable nil))
                 (*package* (find-package :cl-user)))
             (set-macro-character #\?
                                  (lambda (stream char)
                                    (declare (ignore char))
                                    (unless *read-suppress*
                                      (error "macro must run with *read-suppress* bound"))
                                    (read-char stream))
                                  nil)
             (and (equal (let ((*read-suppress* t))
                           (multiple-value-list (read-from-string "?foo")))
                         '(nil 2))
                  (eql (read-from-string "#- (and) ?f 42") 42)))"##,
    );
}

#[test]
fn dispatch_character_rejects_an_unregistered_subcharacter() {
    let runtime = &Runtime::start().unwrap();
    check(
        runtime,
        r#"(let ((*readtable* (copy-readtable nil))
                 (*package* (find-package :cl-user)))
             (and (make-dispatch-macro-character #\!)
                  (eql (read-from-string "123!") 123)
                  (handler-case (progn (read-from-string "!a") nil)
                    (reader-error () t))
                  (handler-case (get-dispatch-macro-character #\a #\b)
                    (error () t))))"#,
    );
}

#[test]
fn reader_macros_distinguish_skipped_input_from_nil() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        r#"(let ((*readtable* (copy-readtable nil)))
             (set-macro-character #\? (lambda (stream char)
                                       (declare (ignore char))
                                       (read-char stream)
                                       (values)))
             (set-macro-character #\! (lambda (stream char)
                                       (declare (ignore stream char))
                                       nil))
             (and (equal (read-from-string "(1 ?x ! 2)") '(1 nil 2))
                  (eql (read-from-string "?x" nil :eof) :eof)
                  (equal (read-from-string "'?x 7") '(quote 7))))"#,
    );
}

#[test]
fn dispatch_macro_receives_the_original_prefix_arguments() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        r##"(let ((*readtable* (copy-readtable nil)))
              (flet ((dispatch (stream sub number)
                       (list sub number (read-char stream))))
                (make-dispatch-macro-character #\!)
                (set-dispatch-macro-character #\! #\q #'dispatch)
                (set-dispatch-macro-character #\# #\q #'dispatch)
                (and (eq (get-dispatch-macro-character #\! #\Q) #'dispatch)
                     (equal (read-from-string "!qx") '(#\q nil #\x))
                     (equal (read-from-string "#0Qy") '(#\Q 0 #\y))
                     (equal (read-from-string "!12345678901234567890123Qz")
                            '(#\Q 12345678901234567890123 #\z))
                     (= (read-from-string "#x10") 16)
                     (handler-case (set-dispatch-macro-character #\! #\0 #'dispatch)
                       (error () t)))))"##,
    );
}

#[test]
fn recursive_read_preserves_backquote_nesting_through_dispatch_macros() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        r##"(let ((*readtable* (copy-readtable nil)))
              (flet ((wrap (stream char argument)
                       (declare (ignore char argument))
                       (list 'wrapped (read stream t nil t)))
                     (independent (stream char argument)
                       (declare (ignore char argument))
                       (read stream)))
                (set-dispatch-macro-character #\# #\Q #'wrap)
                (set-dispatch-macro-character #\# #\W #'independent)
                (and (equal (eval (read-from-string
                                    "(let ((x 37)) `(before #Q(#Q(,x ,@'(1 2))) after))"))
                            '(before (wrapped ((wrapped (37 1 2)))) after))
                     (equal (eval (read-from-string "(let ((x 'a)) `(#Q(',x)))"))
                            '((wrapped ('a))))
                     (handler-case (progn (read-from-string "`(#W(,x))") nil)
                       (reader-error () t)))))"##,
    );
}

#[test]
fn string_reader_macro_refreshes_package_base_and_readtable_bindings() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        r#"(let ((*readtable* (copy-readtable nil))
                  (*package* (find-package :cl-user))
                  (*read-base* 10))
             (set-macro-character #\!
               (lambda (stream character)
                 (declare (ignore stream character))
                 (setq *read-base* 16 *package* (find-package :keyword))
                 (setf (readtable-case *readtable*) :preserve)
                 (set-syntax-from-char #\@ #\Space)
                 (values)))
             (with-input-from-string (input "(before 10 ! @after 10)")
               (let ((form (read input)))
                 (and (eq (first form) 'cl-user::before)
                      (= (second form) 10)
                      (eq (symbol-package (third form)) (find-package :keyword))
                      (string= (symbol-name (third form)) "after")
                      (= (fourth form) 16)))))"#,
    );
}

#[test]
fn standard_dispatch_reads_builtin_literals() {
    let runtime = Runtime::start().unwrap();
    check(
        &runtime,
        r##"(and (equalp #() (read-from-string "#()"))
                  (equalp #(1 2 3) (read-from-string "#(1 2 3)"))
                  (equalp #(1 (2 3) 4) (read-from-string "#(1 (2 3) 4)"))
                  (equalp #*1010 (read-from-string "#*1010"))
                  (char= #\a (read-from-string "#\\a")))"##,
    );
}
