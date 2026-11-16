#![cfg(unix)]

use std::error::Error;
use std::fs;

use zkcl::Runtime;

mod common;

use common::files::TempDir;

fn file_path(directory: &TempDir, name: &str) -> String {
    directory.path().join(name).to_str().unwrap().to_owned()
}

macro_rules! text {
    ($runtime:expr, $value:expr) => {
        $runtime.string(&$value).expect("string value")
    };
}

#[test]
fn read_sequence_overwrites_the_requested_range() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let directory = TempDir::new("stream-sequences");

    let values = runtime.eval(
        r#"(let ((s (copy-seq "     ")))
             (with-input-from-string (in "abcdefgh")
               (values (read-sequence s in) s)))"#,
    )?;
    assert_eq!(values[0].as_fixnum(), Some(5));
    assert_eq!(text!(runtime, values[1]), "abcde");

    let values = runtime.eval(
        r#"(let ((s (copy-seq "     ")))
             (with-input-from-string (in "abcdefgh")
               (values (read-sequence s in :start 1 :end 3) s)))"#,
    )?;
    assert_eq!(values[0].as_fixnum(), Some(3));
    assert_eq!(text!(runtime, values[1]), " ab  ");

    let values = runtime.eval(
        r#"(let ((v (vector nil nil nil)))
             (with-input-from-string (in "xy")
               (values (read-sequence v in :start 1)
                       (aref v 0) (aref v 1) (aref v 2))))"#,
    )?;
    assert_eq!(values[0].as_fixnum(), Some(3));
    assert!(values[1].is_nil());
    assert_eq!(values[2].as_character(), Some('x'));
    assert_eq!(values[3].as_character(), Some('y'));

    let values = runtime.eval(
        r#"(let ((v (make-array 5 :initial-element nil :fill-pointer 3)))
             (with-input-from-string (in "abcdef")
               (values (read-sequence v in) (fill-pointer v)
                       (aref v 0) (aref v 2) (aref v 4))))"#,
    )?;
    assert_eq!(values[0].as_fixnum(), Some(3));
    assert_eq!(values[1].as_fixnum(), Some(3));
    assert_eq!(values[2].as_character(), Some('a'));
    assert_eq!(values[3].as_character(), Some('c'));
    assert!(values[4].is_nil());

    let bits = file_path(&directory, "bits.bin");
    let source = r#"(let ((bits (make-array 4 :element-type 'bit :initial-element 0)))
         (with-open-file (out "<BITS>" :direction :output
                              :if-exists :supersede
                              :element-type '(unsigned-byte 8))
           (write-byte 0 out)
           (write-byte 1 out)
           (write-byte 1 out)
           (write-byte 0 out))
         (with-open-file (in "<BITS>" :direction :input
                             :element-type '(unsigned-byte 8))
           (values (read-sequence bits in) (aref bits 0) (aref bits 1)
                   (aref bits 2) (aref bits 3))))"#
        .replace("<BITS>", &bits);
    let values = runtime.eval(&source)?;
    assert_eq!(values[0].as_fixnum(), Some(4));
    assert_eq!(values[1].as_fixnum(), Some(0));
    assert_eq!(values[2].as_fixnum(), Some(1));
    assert_eq!(values[3].as_fixnum(), Some(1));
    assert_eq!(values[4].as_fixnum(), Some(0));

    fs::write(&bits, [0, 1, 2, 0])?;
    let source = format!(
        r#"(let ((bits (make-array 4 :element-type 'bit :initial-element 0)))
             (with-open-file (in "{bits}" :element-type '(unsigned-byte 8))
               (handler-case (read-sequence bits in)
                 (type-error () (values (file-position in) (aref bits 1) (aref bits 2))))))"#
    );
    let values = runtime.eval(&source)?;
    assert_eq!(values[0].as_fixnum(), Some(3));
    assert_eq!(values[1].as_fixnum(), Some(1));
    assert_eq!(values[2].as_fixnum(), Some(0));

    Ok(())
}

#[test]
fn write_sequence_emits_the_requested_slice() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let values = runtime.eval(
        r#"(let ((seq (coerce "abcde" 'list)))
             (values (with-output-to-string (out)
                       (write-sequence seq out :start 1 :end 4))
                     seq))"#,
    )?;
    assert_eq!(text!(runtime, values[0]), "bcd");

    let values = runtime.eval(
        r#"(let ((v (make-array 5 :initial-contents "abcde" :fill-pointer 4)))
             (with-output-to-string (out)
               (write-sequence v out :start 1)))"#,
    )?;
    assert_eq!(text!(runtime, values[0]), "bcd");

    let values = runtime.eval(
        r#"(let ((seq "abcdef"))
             (values (with-output-to-string (out)
                       (write-sequence seq out :start 2 :end 5))
                     seq))"#,
    )?;
    assert_eq!(text!(runtime, values[0]), "cde");
    assert_eq!(text!(runtime, values[1]), "abcdef");

    Ok(())
}

#[test]
fn file_sequence_io_roundtrips_octet_slices() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let directory = TempDir::new("stream-sequences");
    let bytes = file_path(&directory, "bytes.bin");

    let source = r#"(progn
         (with-open-file (out "<BYTES>" :direction :output
                              :if-exists :supersede
                              :element-type '(unsigned-byte 8))
           (write-sequence (vector 9 8 7 6 5) out :start 1 :end 4))
         (with-open-file (in "<BYTES>" :direction :input
                             :element-type '(unsigned-byte 8))
           (let ((v (make-array 5)))
             (values (read-sequence v in) v))))"#
        .replace("<BYTES>", &bytes);
    let values = runtime.eval(&source)?;
    assert_eq!(values[0].as_fixnum(), Some(3));
    assert_eq!(fs::read(&bytes)?, vec![8u8, 7, 6]);

    let source = format!(
        r#"(with-open-file (in "{bytes}" :direction :input
                              :element-type '(unsigned-byte 8))
             (values (read-byte in) (read-byte in) (read-byte in) (read-byte in nil :eof)))"#
    );
    let values = runtime.eval(&source)?;
    assert_eq!(values[0].as_fixnum(), Some(8));
    assert_eq!(values[1].as_fixnum(), Some(7));
    assert_eq!(values[2].as_fixnum(), Some(6));
    assert_eq!(runtime.symbol_name(&values[3]).as_deref(), Some("EOF"));

    Ok(())
}

#[test]
fn file_sequence_io_roundtrips_character_slices() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let directory = TempDir::new("stream-sequences");
    let chars = file_path(&directory, "chars.txt");
    let source = r#"(progn
         (with-open-file (out "<CHARS>" :direction :output :if-exists :supersede)
           (write-sequence "hello" out :start 1 :end 4))
         (with-open-file (in "<CHARS>" :direction :input)
           (let ((s (copy-seq "xxxxx")))
             (values (read-sequence s in :end 3) s))))"#
        .replace("<CHARS>", &chars);
    let values = runtime.eval(&source)?;
    assert_eq!(values[0].as_fixnum(), Some(3));
    assert_eq!(text!(runtime, values[1]), "ellxx");
    assert_eq!(fs::read(&chars)?, b"ell");

    Ok(())
}

#[test]
fn binary_sequence_write_stops_at_an_invalid_byte() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let directory = TempDir::new("stream-sequences");
    let partial = file_path(&directory, "partial.bin");
    let source = format!(
        r#"(with-open-file (out "{partial}" :direction :output :element-type '(unsigned-byte 8))
             (handler-case (write-sequence #(1 2 256 3) out)
               (type-error (c) (values (type-error-datum c) (file-position out)))))"#
    );
    let values = runtime.eval(&source)?;
    assert_eq!(values[0].as_fixnum(), Some(256));
    assert_eq!(values[1].as_fixnum(), Some(2));
    assert_eq!(fs::read(&partial)?, [1, 2]);

    Ok(())
}

#[test]
fn broadcast_streams_send_output_to_each_component() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let values = runtime.eval(
        r#"(let ((a (make-string-output-stream))
                 (b (make-string-output-stream)))
             (let ((s (make-broadcast-stream a b)))
               (write-string "hi" s)
               (format s " ~A" 42)
               (values (get-output-stream-string a)
                       (get-output-stream-string b)
                       (streamp s)
                       (output-stream-p s)
                       (input-stream-p s)
                       (length (broadcast-stream-streams s)))))"#,
    )?;
    assert_eq!(text!(runtime, values[0]), "hi 42");
    assert_eq!(text!(runtime, values[1]), "hi 42");
    assert!(!values[2].is_nil());
    assert!(!values[3].is_nil());
    assert!(values[4].is_nil());
    assert_eq!(values[5].as_fixnum(), Some(2));

    let values = runtime.eval(
        r#"(let ((s (make-broadcast-stream)))
             (write-char #\x s)
             (values (streamp s) (input-stream-p s)
                     (file-length s) (file-position s)
                     (file-string-length s "abc")))"#,
    )?;
    assert!(!values[0].is_nil());
    assert!(values[1].is_nil());
    assert_eq!(values[2].as_fixnum(), Some(0));
    assert_eq!(values[3].as_fixnum(), Some(0));
    assert_eq!(values[4].as_fixnum(), Some(1));

    Ok(())
}

#[test]
fn concatenated_streams_continue_at_component_boundaries() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let directory = TempDir::new("stream-sequences");
    let values = runtime.eval(
        r#"(with-input-from-string (a "ab")
             (with-input-from-string (b "cd")
               (let ((s (make-concatenated-stream a b)))
                 (values (read-char s) (read-char s)
                         (length (concatenated-stream-streams s))
                         (read-char s) (read-char s)
                         (read-char s nil :eof)
                         (streamp s) (input-stream-p s) (output-stream-p s)))))"#,
    )?;
    assert_eq!(values[0].as_character(), Some('a'));
    assert_eq!(values[1].as_character(), Some('b'));
    assert_eq!(values[2].as_fixnum(), Some(2));
    assert_eq!(values[3].as_character(), Some('c'));
    assert_eq!(values[4].as_character(), Some('d'));
    assert_eq!(runtime.symbol_name(&values[5]).as_deref(), Some("EOF"));
    assert!(!values[6].is_nil());
    assert!(!values[7].is_nil());
    assert!(values[8].is_nil());

    let values = runtime.eval(
        r#"(let ((s (make-concatenated-stream
                    (make-string-input-stream "ab")
                    (make-string-input-stream "cd"))))
             (multiple-value-bind (line missing-newline-p) (read-line s)
               (values line missing-newline-p)))"#,
    )?;
    assert_eq!(text!(runtime, values[0]), "abcd");
    assert!(!values[1].is_nil());

    let values = runtime.eval(
        r#"(with-input-from-string (a "a")
             (with-input-from-string (b "b")
               (let ((s (make-concatenated-stream a b)))
                 (values (listen s) (read-char s) (unread-char #\a s)
                         (read-char s) (read-char s) (listen s)))))"#,
    )?;
    assert!(!values[0].is_nil());
    assert_eq!(values[1].as_character(), Some('a'));
    assert!(values[2].is_nil());
    assert_eq!(values[3].as_character(), Some('a'));
    assert_eq!(values[4].as_character(), Some('b'));
    assert!(values[5].is_nil());

    let values = runtime.eval(
        r#"(let* ((a (make-string-input-stream "(ab"))
                  (b (make-string-input-stream "c) 9"))
                  (s (make-concatenated-stream a b)))
             (values (write-to-string (read s))
                     (read s)
                     (read s nil :eof)))"#,
    )?;
    assert_eq!(text!(runtime, values[0]), "(ABC)");
    assert_eq!(values[1].as_fixnum(), Some(9));
    assert_eq!(runtime.symbol_name(&values[2]).as_deref(), Some("EOF"));

    let values = runtime.eval(
        r##"(let ((s (make-concatenated-stream
                    (make-string-input-stream "#| split")
                    (make-string-input-stream " comment |# 42"))))
             (read s))"##,
    )?;
    assert_eq!(values[0].as_fixnum(), Some(42));

    let file = file_path(&directory, "split-form.lisp");
    fs::write(&file, "(fi")?;
    let values = runtime.eval(
        &r#"(let* ((a (open "<FILE>" :direction :input))
                   (b (make-string-input-stream "le) 7"))
                   (s (make-concatenated-stream a b)))
             (let ((form (write-to-string (read s)))
                   (position (file-position a)))
               (close a)
               (values form (read s) position)))"#
            .replace("<FILE>", &file),
    )?;
    assert_eq!(text!(runtime, values[0]), "(FILE)");
    assert_eq!(values[1].as_fixnum(), Some(7));
    assert_eq!(values[2].as_fixnum(), Some(3));

    let values = runtime.eval(
        r#"(with-input-from-string (s "abc")
             (let ((c (make-concatenated-stream s)))
               (values (close c) (open-stream-p c)
                       (open-stream-p s))))"#,
    )?;
    assert!(!values[0].is_nil());
    assert!(values[1].is_nil());
    assert!(!values[2].is_nil());

    Ok(())
}

#[test]
fn echo_streams_copy_consumed_input_to_output() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let values = runtime.eval(
        r#"(let* ((is (make-string-input-stream "foo"))
                  (os (make-string-output-stream))
                  (s (make-echo-stream is os)))
             (values (read-char s)
                     (get-output-stream-string os)
                     (read-line s)
                     (get-output-stream-string os)
                     (eq (echo-stream-input-stream s) is)
                     (eq (echo-stream-output-stream s) os)))"#,
    )?;
    assert_eq!(values[0].as_character(), Some('f'));
    assert_eq!(text!(runtime, values[1]), "f");
    assert_eq!(text!(runtime, values[2]), "oo");
    assert_eq!(text!(runtime, values[3]), "oo");
    assert!(!values[4].is_nil());
    assert!(!values[5].is_nil());

    let values = runtime.eval(
        r#"(let* ((is (make-string-input-stream "(ab)"))
                  (os (make-string-output-stream))
                  (s (make-echo-stream is os)))
             (values (write-to-string (read s))
                     (get-output-stream-string os)))"#,
    )?;
    assert_eq!(text!(runtime, values[0]), "(AB)");
    assert_eq!(text!(runtime, values[1]), "(ab)");

    let values = runtime.eval(
        r#"(let* ((is (make-string-input-stream "  ab"))
                  (os (make-string-output-stream))
                  (s (make-echo-stream is os)))
             (values (peek-char t s)
                     (get-output-stream-string os)))"#,
    )?;
    assert_eq!(values[0].as_character(), Some('a'));
    assert_eq!(text!(runtime, values[1]), "  ");

    let values = runtime.eval(
        r#"(let* ((is (make-string-input-stream "abc"))
                  (os (make-string-output-stream))
                  (s (make-echo-stream is os)))
             (read-char s)
             (unread-char #\a s)
             (values (read-char s)
                     (read-char s)
                     (get-output-stream-string os)))"#,
    )?;
    assert_eq!(values[0].as_character(), Some('a'));
    assert_eq!(values[1].as_character(), Some('b'));
    assert_eq!(text!(runtime, values[2]), "ab");

    let values = runtime.eval(
        r#"(let* ((is (make-string-input-stream "foo"))
                  (os (make-string-output-stream))
                  (s (make-echo-stream is os)))
             (write-string "159" s)
             (values (close s) (get-output-stream-string os)
                     (open-stream-p is) (open-stream-p os)))"#,
    )?;
    assert!(!values[0].is_nil());
    assert_eq!(text!(runtime, values[1]), "159");
    assert!(!values[2].is_nil());
    assert!(!values[3].is_nil());

    Ok(())
}

#[test]
fn two_way_streams_route_io_to_the_corresponding_component() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let values = runtime.eval(
        r#"(let* ((is (make-string-input-stream "in"))
                  (os (make-string-output-stream))
                  (s (make-two-way-stream is os)))
             (values (read-char s) (write-string "out" s)
                     (get-output-stream-string os)
                     (input-stream-p s) (output-stream-p s)
                     (eq (two-way-stream-input-stream s) is)
                     (eq (two-way-stream-output-stream s) os)
                     (close s) (open-stream-p s)))"#,
    )?;
    assert_eq!(values[0].as_character(), Some('i'));
    assert_eq!(text!(runtime, values[1]), "out");
    assert_eq!(text!(runtime, values[2]), "out");
    assert!(!values[3].is_nil());
    assert!(!values[4].is_nil());
    assert!(!values[5].is_nil());
    assert!(!values[6].is_nil());
    assert!(!values[7].is_nil());
    assert!(values[8].is_nil());

    Ok(())
}

#[test]
fn synonym_streams_follow_the_symbol_value() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let values = runtime.eval(
        r#"(with-input-from-string (*syn* "hi")
             (declare (special *syn*))
             (let ((s (make-synonym-stream '*syn*)))
               (values (symbol-name (synonym-stream-symbol s))
                       (read-char s)
                       (read-char s)
                       (input-stream-p s)
                       (output-stream-p s)
                       (open-stream-p s))))"#,
    )?;
    assert_eq!(text!(runtime, values[0]), "*SYN*");
    assert_eq!(values[1].as_character(), Some('h'));
    assert_eq!(values[2].as_character(), Some('i'));
    assert!(!values[3].is_nil());
    assert!(values[4].is_nil());
    assert!(!values[5].is_nil());

    let values = runtime.eval(
        r#"(let ((*syn* (make-string-output-stream)))
             (declare (special *syn*))
             (let ((s (make-synonym-stream '*syn*)))
               (write-string "syn" s)
               (values (output-stream-p s) (input-stream-p s)
                       (get-output-stream-string *syn*))))"#,
    )?;
    assert!(!values[0].is_nil());
    assert!(values[1].is_nil());
    assert_eq!(text!(runtime, values[2]), "syn");

    Ok(())
}

#[test]
fn read_skips_repeated_exhausted_components() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let values = runtime.eval(
        r#"(let* ((leaf (make-string-input-stream "(12"))
                  (rest (make-string-input-stream "3)"))
                  (s (make-concatenated-stream leaf leaf rest)))
             (values (car (read s)) (read s nil :end)
                     (concatenated-stream-streams s)))"#,
    )?;
    assert_eq!(values[0].as_fixnum(), Some(123));
    assert_eq!(runtime.symbol_name(&values[1]).as_deref(), Some("END"));
    assert!(values[2].is_nil());

    Ok(())
}

#[test]
fn read_follows_the_stream_after_reader_macro_side_effects() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    runtime.register_function("collect-during-read", |rt, _| {
        rt.collect();
        Ok(Vec::new())
    })?;
    let values = runtime.eval(
        r##"(let* ((*readtable* (copy-readtable nil))
                   (*read-source* (make-string-input-stream "(1 !ignored"))
                   (out (make-string-output-stream))
                   (s (make-echo-stream (make-synonym-stream '*read-source*) out)))
              (declare (special *read-source*))
              (set-macro-character #\!
                (lambda (actual ch)
                  (declare (ignore ch))
                  (assert (eq actual s))
                  (assert (equal (get-output-stream-string out) "(1 !"))
                  (collect-during-read)
                  (setq *read-source* (make-string-input-stream "23) 42"))
                  (char-code (read-char actual))))
              (values (read s) (read s) (get-output-stream-string out)))"##,
    )?;
    assert_eq!(runtime.repr(&values[0])?, "(1 50 3)");
    assert_eq!(values[1].as_fixnum(), Some(42));
    assert_eq!(text!(runtime, values[2]), "23) 42");

    let values = runtime.eval(
        r#"(let* ((*readtable* (copy-readtable nil))
                  (open-list (get-macro-character #\())
                  (out (make-string-output-stream))
                  (s (make-echo-stream
                       (make-concatenated-stream (make-string-input-stream "!1 ")
                                                 (make-string-input-stream "2)3 ]"))
                       out)))
             (set-macro-character #\!
               (lambda (actual ch) (declare (ignore ch)) (funcall open-list actual #\()))
             (values (read s) (read-delimited-list #\] s)
                     (get-output-stream-string out)))"#,
    )?;
    assert_eq!(runtime.repr(&values[0])?, "(1 2)");
    assert_eq!(runtime.repr(&values[1])?, "(3)");
    assert_eq!(text!(runtime, values[2]), "!1 2)3 ]");

    let values = runtime.eval(
        r#"(let ((*readtable* (copy-readtable nil))
                 (s (make-string-input-stream "é(1 !x废3) 42" 1)))
             (set-macro-character #\!
               (lambda (actual ch)
                 (declare (ignore ch))
                 (assert (eq actual s))
                 (assert (char= (read-char actual) #\x))
                 (file-position actual 6)
                 2))
             (values (read s) (read s)))"#,
    )?;
    assert_eq!(runtime.repr(&values[0])?, "(1 2 3)");
    assert_eq!(values[1].as_fixnum(), Some(42));

    Ok(())
}

#[test]
fn read_defers_stream_errors_until_the_next_form() -> Result<(), Box<dyn Error>> {
    let runtime = Runtime::start()?;
    let directory = TempDir::new("stream-sequences");
    let file = file_path(&directory, "invalid-utf8");
    fs::write(&file, [0xff])?;
    let values = runtime.eval(
        &r#"(with-open-file (bad "<FILE>" :external-format :utf-8)
        (let ((s (make-concatenated-stream
                   (make-string-input-stream "(4") (make-string-input-stream "2)") bad)))
          (values (car (read s))
                  (handler-case (progn (read s) nil)
                     (stream-error (c) (eq bad (stream-error-stream c)))))))"#
            .replace("<FILE>", &file),
    )?;
    assert_eq!(values[0].as_fixnum(), Some(42));
    assert!(!values[1].is_nil());
    Ok(())
}

#[test]
fn read_waits_for_host_input_only_when_the_next_form_needs_it() -> Result<(), Box<dyn Error>> {
    use std::cell::Cell;
    use std::io::{BufReader, Read};
    use std::rc::Rc;

    struct HostSource {
        ready: Rc<Cell<bool>>,
        bytes: &'static [u8],
    }
    impl Read for HostSource {
        fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
            assert!(self.ready.get(), "READ waited for the next component");
            self.bytes.read(target)
        }
    }
    let ready = Rc::new(Cell::new(false));
    let mut runtime = Runtime::start()?;
    runtime.set_standard_input(BufReader::new(HostSource {
        ready: Rc::clone(&ready),
        bytes: b"(2\n3) 4\n#+#.(cl:progn (cl:incf cl-user::*feature-reads*) :present) (5\n6) !Q",
    }));
    runtime.load_str(
        "(defvar *joined* (make-concatenated-stream (make-string-input-stream \"(1)\") *standard-input*))",
    )?;
    assert_eq!(runtime.repr(&runtime.eval("(read *joined*)")?[0])?, "(1)");
    ready.set(true);
    assert_eq!(runtime.repr(&runtime.eval("(read *joined*)")?[0])?, "(2 3)");
    assert_eq!(runtime.eval("(read *joined*)")?[0].as_fixnum(), Some(4));

    let values = runtime.eval(
        r##"(let ((*features* '(:present)) (*feature-reads* 0)
                  (*readtable* (copy-readtable nil)) (calls 0))
              (declare (special *feature-reads*))
              (set-macro-character #\!
                (lambda (actual ch)
                  (declare (ignore ch))
                  (assert (eq actual *standard-input*))
                  (incf calls)
                  (char-code (read-char actual))))
              (values (read) (read) *feature-reads* calls))"##,
    )?;
    assert_eq!(runtime.repr(&values[0])?, "(5 6)");
    assert_eq!(values[1].as_fixnum(), Some(81));
    assert_eq!(values[2].as_fixnum(), Some(1));
    assert_eq!(values[3].as_fixnum(), Some(1));
    Ok(())
}
