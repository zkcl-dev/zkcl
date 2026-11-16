#![cfg(unix)]

use std::error::Error;
use std::fs;

use zkcl::{ConditionClass, EvalError, EvalErrorKind, Runtime};

mod common;

use common::files::TempDir;

fn file_path(directory: &TempDir, name: &str) -> String {
    directory.path().join(name).to_str().unwrap().to_owned()
}

#[test]
fn text_file_append_starts_at_end_of_file() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let text = file_path(&directory, "text.txt");

    let source = format!(
        r#"(with-open-file (out "{text}" :direction :output :if-exists :supersede)
             (write-line "alpha" out)
             (write-line "beta" out))"#
    );
    let written = runtime.eval(&source).unwrap();
    assert_eq!(runtime.string(&written[0]).as_deref(), Some("beta"));
    assert_eq!(fs::read(&text).unwrap(), b"alpha\nbeta\n");

    let source = format!(
        r#"(with-open-file (in "{text}" :direction :input)
             (let ((first (read-line in))
                   (second (read-line in))
                   (length (file-length in)))
               (values (typep in 'file-stream)
                       (open-stream-p in)
                       (symbol-name (stream-element-type in))
                       first second length)))"#
    );
    let values = runtime.eval(&source).unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    assert_eq!(runtime.string(&values[2]).as_deref(), Some("CHARACTER"));
    assert_eq!(runtime.string(&values[3]).as_deref(), Some("alpha"));
    assert_eq!(runtime.string(&values[4]).as_deref(), Some("beta"));
    assert_eq!(values[5].as_fixnum(), Some(11));

    let source = format!(
        r#"(progn
             (with-open-file (out "{text}" :direction :output :if-exists :append)
                (assert (= (file-position out) 11))
               (write-line "gamma" out))
             (with-open-file (in "{text}" :direction :input)
               (read-line in)
               (read-line in)
               (read-line in)))"#
    );
    let appended = runtime.eval(&source).unwrap();
    assert_eq!(runtime.string(&appended[0]).as_deref(), Some("gamma"));

    let source = format!(
        r#"(with-open-file (in "{text}" :direction :input)
             (values (file-position in)
                     (read-char in)
                     (peek-char nil in)
                     (read-char in)
                     (unread-char #\l in)
                     (read-char in)
                     (file-position in :start)
                     (file-position in)))"#
    );
    let positions = runtime.eval(&source).unwrap();
    assert_eq!(positions[0].as_fixnum(), Some(0));
    assert_eq!(positions[1].as_character(), Some('a'));
    assert_eq!(positions[2].as_character(), Some('l'));
    assert_eq!(positions[3].as_character(), Some('l'));
    assert!(positions[4].is_nil());
    assert_eq!(positions[5].as_character(), Some('l'));
    assert!(!positions[6].is_nil());
    assert_eq!(positions[7].as_fixnum(), Some(0));

    let source = format!(r#"(open "{text}" :direction :output :if-exists :error)"#);
    let error = runtime.eval(&source).err().unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::FileError)
    );
    assert!(error.source().is_some());

    let source = format!(
        r#"(values
             (let ((s (open "{text}" :direction :input)))
               (prog1 (open-stream-p s)
                 (close s)))
             (let ((s (open "{text}" :direction :input)))
               (close s)
               (open-stream-p s)))"#
    );
    let values = runtime.eval(&source).unwrap();
    assert!(!values[0].is_nil());
    assert!(values[1].is_nil());
}

#[test]
fn concurrent_conditional_creates_return_nil_to_all_but_one_caller() {
    const WORKERS: usize = 8;
    const ROUNDS: usize = 12;
    let directory = TempDir::new("stream-open-exclusive");
    let sources: Vec<_> = (0..ROUNDS)
        .map(|round| {
            let path = file_path(&directory, &format!("{round}.lock"));
            let direction = if round % 2 == 0 { ":output" } else { ":io" };
            format!(
                r#"(let ((stream (open "{path}" :direction {direction}
                                 :if-exists nil :if-does-not-exist :create)))
                     (when stream (write-line "winner" stream) (close stream) t))"#
            )
        })
        .collect();
    let barrier = std::sync::Barrier::new(WORKERS);
    let outcomes = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                scope.spawn(|| {
                    let runtime = Runtime::start().unwrap();
                    sources
                        .iter()
                        .map(|source| {
                            barrier.wait();
                            runtime
                                .eval(source)
                                .map(|values| !values[0].is_nil())
                                .map_err(|error| error.to_string())
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    for round in 0..ROUNDS {
        let count = outcomes
            .iter()
            .filter(|worker| {
                *worker[round]
                    .as_ref()
                    .unwrap_or_else(|error| panic!("round {round}: {error}"))
            })
            .count();
        assert_eq!(count, 1, "round {round}");
        assert_eq!(
            fs::read(file_path(&directory, &format!("{round}.lock"))).unwrap(),
            b"winner\n"
        );
    }
}

#[test]
fn latin1_source_can_be_read_after_file_compilation() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let latin = file_path(&directory, "latin.lisp");
    let source = format!(
        r#"(with-open-file (out "{latin}" :direction :output :external-format :latin-1)
             (assert (= (file-string-length out "é") 1))
             (write-line "(defparameter *sample* \"café\")" out)
             (write-line "42" out))"#
    );
    runtime.eval(&source).unwrap();
    assert!(fs::read(&latin).unwrap().contains(&0xe9));
    let source = format!(
        r#"(with-open-file (in "{latin}" :external-format :iso-8859-1)
             (read in)
             (values (stream-external-format in) (read in)))"#
    );
    let forms = runtime.eval(&source).unwrap();
    assert_eq!(runtime.symbol_name(&forms[0]).as_deref(), Some("LATIN-1"));
    assert_eq!(forms[1].as_fixnum(), Some(42));
    runtime
        .eval(&format!(r#"(load "{latin}" :external-format :latin-1)"#))
        .unwrap();
    assert_eq!(
        runtime.eval("*sample*").unwrap()[0]
            .as_string(&runtime)
            .as_deref(),
        Some("café")
    );
    runtime
        .eval(&format!(
            r#"(compile-file "{latin}" :external-format :latin-1)"#
        ))
        .unwrap();
    runtime.eval("(makunbound '*sample*)").unwrap();
    runtime
        .load_file(&directory.path().join("latin.zkclfasl"))
        .unwrap();
    assert_eq!(
        runtime.eval("*sample*").unwrap()[0]
            .as_string(&runtime)
            .as_deref(),
        Some("café")
    );
    let source = format!(
        r#"(with-open-file (out "{latin}" :direction :output :if-exists :append :external-format :latin-1)
             (write-string "雪" out))"#
    );
    assert_eq!(
        runtime.eval(&source).err().unwrap().kind(),
        EvalErrorKind::LispCondition(ConditionClass::StreamError)
    );
}

#[test]
fn opening_a_missing_input_file_can_create_it() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let missing = file_path(&directory, "new-input.txt");
    runtime
        .eval(&format!(
            r#"(with-open-file (in "{missing}" :direction :input :if-does-not-exist :create)
                 (assert (eq (read-char in nil :empty) :empty)))"#
        ))
        .unwrap();
    assert_eq!(fs::metadata(&missing).unwrap().len(), 0);
}

#[test]
fn utf8_stream_positions_count_bytes() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let utf8 = file_path(&directory, "utf8.txt");
    fs::write(&utf8, "é😀\nλ").unwrap();
    runtime
        .eval(&format!(
            r#"(with-open-file (in "{utf8}" :external-format :utf-8)
                 (assert (char= (peek-char nil in) #\é))
                 (assert (zerop (file-position in)))
                 (assert (char= (read-char in) #\é))
                 (assert (= (file-position in) 2))
                 (assert (char= (read-char in) #\😀))
                 (assert (= (file-position in) 6))
                 (unread-char #\😀 in)
                 (assert (= (file-position in) 2))
                 (assert (string= (read-line in) "😀"))
                 (assert (= (file-position in) 7))
                 (assert (char= (read-char in) #\λ)))"#
        ))
        .unwrap();
}

#[test]
fn invalid_utf8_does_not_advance_the_failed_read() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let malformed = file_path(&directory, "malformed.txt");
    fs::write(&malformed, b"a\xc2z\xe2\x82").unwrap();
    runtime
        .eval(&format!(
            r#"(with-open-file (in "{malformed}")
                 (assert (char= (read-char in) #\a))
                 (assert (handler-case (progn (peek-char nil in) nil)
                           (stream-error (c) (eq (stream-error-stream c) in))))
                 (assert (= (file-position in) 1))
                 (assert (file-position in 2))
                 (assert (char= (read-char in) #\z))
                 (assert (handler-case (progn (read-char in nil :eof) nil)
                           (stream-error (c) (eq (stream-error-stream c) in))))
                 (assert (= (file-position in) 3))
                 (assert (file-position in :start))
                 (assert (handler-case (progn (read in) nil)
                           (stream-error (c) (eq (stream-error-stream c) in))))
                 (assert (zerop (file-position in))))"#
        ))
        .unwrap();
}

#[test]
fn loading_a_closed_stream_signals_a_stream_error() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval(
            r#"(let ((in (make-string-input-stream "")))
                 (close in)
                 (assert (handler-case (progn (load in) nil)
                           (stream-error (c) (eq (stream-error-stream c) in)))))"#,
        )
        .unwrap();
}

#[test]
fn file_read_preserves_byte_positions_across_buffer_refills() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str("(defvar *file-reader-effects* 0)")
        .unwrap();

    let utf8 = file_path(&directory, "long-form.lisp");
    let prefix = "(#.(incf *file-reader-effects*) \"";
    // Place the first four-byte character across the initial 512-byte window.
    let padding = "x".repeat(511 - prefix.len());
    let payload = "😀λ".repeat(6000);
    let form = format!("{prefix}{padding}{payload}\")");
    // From the end of the first form, the final number also spans that boundary.
    let gap = " ".repeat(511);
    fs::write(&utf8, format!("{form}{gap}42")).unwrap();
    let end = form.len();
    let characters = padding.len() + 12_000;
    runtime
        .eval(&format!(
            r#"(with-open-file (in "{utf8}" :external-format :utf-8)
                 (let ((form (read-preserving-whitespace in)))
                   (assert (= (first form) 1))
                   (assert (= (length (second form)) {characters}))
                   (assert (char= (char (second form) {padding_length}) #\😀)))
                 (assert (= (file-position in) {end}))
                 (assert (char= (read-char in) #\Space))
                 (unread-char #\Space in)
                 (assert (= (file-position in) {end}))
                 (assert (= (read in) 42))
                 (assert (eq (read in nil :end) :end))
                 (assert (file-position in {end}))
                 (assert (= (read in) 42))
                 (assert (= *file-reader-effects* 1)))"#,
            padding_length = padding.len(),
        ))
        .unwrap();
}

#[test]
fn latin1_reader_errors_report_byte_offsets() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let latin1 = file_path(&directory, "latin1-form.lisp");
    fs::write(&latin1, b"x\"caf\xe9\" \xe9 42\n;\xe9\n)").unwrap();
    let error = runtime
        .eval(&format!(
            r#"(with-open-file (in "{latin1}" :external-format :latin-1)
                 (assert (char= (read-char in) #\x))
                 (assert (string= (read-preserving-whitespace in) "café"))
                 (assert (= (file-position in) 7))
                 (assert (char= (read-char in) #\Space))
                 (assert (char= (read-char in) #\é))
                 (assert (= (file-position in) 9))
                 (assert (= (read in) 42))
                 (read in))"#,
        ))
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::ReaderError)
    );
    let nested = error.source().unwrap().downcast_ref::<EvalError>().unwrap();
    assert_eq!(nested.kind(), EvalErrorKind::Read);
    // The unmatched parenthesis follows a Latin-1 byte in the comment.
    assert_eq!(nested.byte_range(), 16..17);
}

#[test]
fn file_reader_macros_read_delimited_forms_from_the_original_stream() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let file = file_path(&directory, "reader-macro.lisp");
    let payload = "λ😀".repeat(10_000);
    let form = format!("@7 \"{payload}\" 41)");
    fs::write(&file, format!("{form} !42")).unwrap();
    let end = form.len();
    runtime
        .eval(&format!(
            r#"(with-open-file (in "{file}")
                 (let ((*readtable* (copy-readtable nil))
                       (calls 0))
                   (set-macro-character
                    #\@ (lambda (stream character)
                          (declare (ignore character))
                          (assert (eq stream in))
                          (assert (= (file-position stream) 1))
                          (incf calls)
                          (assert (= (read stream t nil t) 7))
                          (let ((items (read-delimited-list #\) stream t)))
                            (assert (= (file-position stream) {end}))
                            (assert (char= (read-char stream) #\Space))
                            (assert (file-position stream {end}))
                            (set-syntax-from-char #\! #\Space)
                            items)))
                   (let ((result (read-preserving-whitespace in)))
                     (assert (= (length (first result)) 20000))
                     (assert (= (second result) 41)))
                   (assert (= (file-position in) {end}))
                   (assert (char= (read-char in) #\Space))
                   (assert (= (read in) 42))
                   (assert (= calls 1))))"#,
        ))
        .unwrap();
}

#[test]
fn read_eval_can_change_the_rest_of_a_file_form() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let file = file_path(&directory, "read-eval.lisp");
    let prefix = r#"(#.(progn
        (incf *file-reader-effects*)
        (set-macro-character #\!
          (lambda (stream character)
            (declare (ignore character))
            (read-char stream)
            (read stream t nil t)))
        *file-reader-effects*) !)"#;
    // The macro introduced by #. consumes the apparent closing parenthesis
    // and a string whose end lies beyond the initial input prefix.
    let payload = "λ".repeat(32_000);
    fs::write(&file, format!("{prefix} \"{payload}\") 42")).unwrap();
    runtime
        .eval(&format!(
            r#"(let ((*readtable* (copy-readtable nil))
                     (*file-reader-effects* 0))
                 (declare (special *file-reader-effects*))
                 (with-open-file (in "{file}")
                   (let ((form (read in)))
                     (assert (= (first form) 1))
                     (assert (= (length (second form)) 32000)))
                   (assert (= *file-reader-effects* 1))
                   (assert (= (read in) 42))))"#,
        ))
        .unwrap();
}

#[test]
fn read_eval_refreshes_passive_stream_bindings() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let file = file_path(&directory, "reader-bindings.lisp");
    let source = r#"(before 10 #.(progn
        (setq cl:*read-base* 16 cl:*package* (cl:find-package :keyword))
        (setf (cl:readtable-case cl:*readtable*) :preserve)
        (cl:set-syntax-from-char #\@ #\Space)
        (cl:values)) @after 10)"#;
    fs::write(&file, source).unwrap();
    let source_literal = format!("\"{}\"", source.replace('\\', "\\\\").replace('"', "\\\""));
    runtime
        .eval(&format!(
            r#"(flet ((check-input (input)
                       (let ((*readtable* (copy-readtable nil))
                             (*package* (find-package :cl-user))
                             (*read-base* 10))
                         (let ((form (read input)))
                           (assert (eq (first form) 'cl-user::before))
                           (assert (= (second form) 10))
                           (assert (eq (symbol-package (third form))
                                       (find-package :keyword)))
                           (assert (string= (symbol-name (third form)) "after"))
                           (assert (= (fourth form) 16))))))
                 (with-input-from-string (input {source_literal}) (check-input input))
                 (with-open-file (input "{file}") (check-input input)))"#,
        ))
        .unwrap();
}

#[test]
fn file_read_sees_overwrites_after_repositioning() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let file = file_path(&directory, "overwrite.lisp");
    fs::write(&file, "\"éλ\" 73 99").unwrap();
    runtime
        .eval(&format!(
            r#"(with-open-file (in "{file}" :direction :io :if-exists :overwrite)
                 (assert (string= (read in) "éλ"))
                 (let ((position (file-position in)))
                   (assert (= (read in) 73))
                   (assert (file-position in position))
                   (write-string "42 " in)
                   (assert (file-position in position))
                   (assert (= (read in) 42))
                   (assert (= (read in) 99))
                   (assert (eq (read in nil :end) :end))))"#,
        ))
        .unwrap();
}

#[test]
fn stream_element_types_expand_user_aliases() {
    let directory = TempDir::new("stream-element-alias");
    let path = file_path(&directory, "aliases.bin");
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(&format!(
            r#"(deftype word-sized (n) (list 'unsigned-byte n))
               (deftype octet () '(word-sized 8))
               (deftype text-element () 'standard-char)
               (with-open-file (s "{path}" :direction :output :element-type 'octet)
                 (assert (equal (stream-element-type s) '(unsigned-byte 8)))
                 (write-byte 255 s))
               (with-open-file (s "{path}" :element-type '(word-sized 8))
                 (assert (= (read-byte s) 255)))
               (with-open-file (s "{path}" :direction :output :if-exists :supersede
                                   :element-type '(or octet (signed-byte 8)))
                 (write-byte -128 s) (write-byte 255 s))
               (with-open-file (s "{path}" :element-type '(or octet (signed-byte 8)))
                 (assert (= (read-byte s) -128)) (assert (= (read-byte s) 255)))
               (let ((s (make-string-output-stream :element-type 'text-element)))
                 (write-char #\A s)
                 (assert (equal (get-output-stream-string s) "A")))"#
        ))
        .unwrap();
}

#[test]
fn file_reader_parses_a_complete_list_before_eof() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let form = file_path(&directory, "form.lisp");
    fs::write(&form, b"(1 2 3)").unwrap();
    let source = format!(
        r#"(with-open-file (in "{form}" :direction :input)
             (values (length (read in))
                     (not (read in nil))))"#
    );
    let forms = runtime.eval(&source).unwrap();
    assert_eq!(forms[0].as_fixnum(), Some(3));
    assert!(!forms[1].is_nil());
}

#[test]
fn binary_file_streams_roundtrip_octets() {
    let directory = TempDir::new("stream-open");
    let runtime = Runtime::start().unwrap();
    let bytes = file_path(&directory, "bytes.bin");
    let source = format!(
        r#"(with-open-file (out "{bytes}" :direction :output
                                :if-exists :supersede
                                :element-type '(unsigned-byte 8))
             (write-byte 65 out)
             (write-byte 66 out))"#
    );
    runtime.eval(&source).unwrap();
    let source = format!(
        r#"(with-open-file (in "{bytes}" :direction :input
                               :element-type '(unsigned-byte 8))
             (values (read-byte in)
                     (read-byte in)
                     (eq (read-byte in nil :eof) :eof)
                     (file-length in)
                     (equal (stream-element-type in) '(unsigned-byte 8))))"#
    );
    let values = runtime.eval(&source).unwrap();
    assert_eq!(values[0].as_fixnum(), Some(65));
    assert_eq!(values[1].as_fixnum(), Some(66));
    assert!(!values[2].is_nil());
    assert_eq!(values[3].as_fixnum(), Some(2));
    assert!(!values[4].is_nil());
}
