use zkcl::{ConditionClass, EvalErrorKind, Runtime};

#[test]
fn listening_to_concatenated_input_preserves_the_last_read_character() {
    let runtime = Runtime::start().unwrap();
    runtime
        .eval(
            r#"(let* ((first (make-string-input-stream "λ"))
                      (second (make-string-input-stream "x"))
                      (inner (make-concatenated-stream first))
                      (stream (make-concatenated-stream inner second)))
                 (assert (char= (read-char stream) #\λ))
                 (assert (listen stream))
                 (unread-char #\λ stream)
                 (assert (char= (read-char stream) #\λ))
                 (assert (char= (read-char stream) #\x))
                 (assert (not (listen stream)))
                 (unread-char #\x stream)
                 (assert (char= (read-char stream) #\x)))"#,
        )
        .unwrap();
}

#[test]
fn no_hang_read_reports_known_host_input_eof() {
    let mut runtime = Runtime::start().unwrap();
    runtime.set_standard_input(std::io::Cursor::new(b""));
    runtime
        .eval(
            r#"(let ((tail (make-string-input-stream "λ"))
                     (out (make-string-output-stream)))
                 (assert (eq (read-char *standard-input* nil :end) :end))
                 (assert (eq (read-char-no-hang *standard-input* nil :end) :end))
                 (let ((stream (make-echo-stream
                                 (make-concatenated-stream *standard-input* tail)
                                 out)))
                   (assert (listen stream))
                   (assert (char= (read-char-no-hang stream) #\λ))
                   (assert (eq (read-char-no-hang stream nil :end) :end))
                   (assert (string= (get-output-stream-string out) "λ"))))"#,
        )
        .unwrap();
}

#[test]
fn string_input_stream_positions_count_characters() {
    let runtime = Runtime::start().unwrap();

    let values = runtime
        .eval(
            r#"(let ((s (make-string-input-stream "éλaβ
終" 1 5)))
                 (values (streamp s) (input-stream-p s) (output-stream-p s)
                         (typep s 'string-stream) (type-of s)
                         (peek-char nil s) (read-char s)
                         (unread-char #\λ s) (read-char s)
                         (read-line s) (file-position s)
                         (read-char s nil :eof)))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert!(!values[1].is_nil());
    assert!(values[2].is_nil());
    assert!(!values[3].is_nil());
    assert_eq!(
        runtime.symbol_name(&values[4]).as_deref(),
        Some("STRING-STREAM")
    );
    assert_eq!(values[5].as_character(), Some('λ'));
    assert_eq!(values[6].as_character(), Some('λ'));
    assert!(values[7].is_nil());
    assert_eq!(values[8].as_character(), Some('λ'));
    assert_eq!(runtime.string(&values[9]).as_deref(), Some("aβ"));
    assert_eq!(values[10].as_fixnum(), Some(4));
    assert_eq!(runtime.symbol_name(&values[11]).as_deref(), Some("EOF"));
}

#[test]
fn string_input_scope_stores_its_final_index() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-streams", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();

    let values = runtime
        .eval(
            r#"(let ((index (list 0)))
                 (multiple-value-bind (first second)
                     (with-input-from-string (s "λ 42 7" :start 2 :index (car index))
                       (collect-streams)
                       (values (read s) (peek-char nil s)))
                   (values first second (car index))))"#,
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(42));
    assert_eq!(values[1].as_character(), Some('7'));
    assert_eq!(values[2].as_fixnum(), Some(5));
}

#[test]
fn nonlocal_exit_closes_string_input_without_storing_its_index() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(let ((index 8) escaped)
                 (catch 'done
                   (with-input-from-string (s "42" :index index)
                     (setq escaped s)
                     (throw 'done (read s))))
                 (values index (open-stream-p escaped)
                         (handler-case (read escaped)
                           (stream-error (condition)
                             (eq (stream-error-stream condition) escaped)))))"#,
        )
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(8));
    assert!(values[1].is_nil());
    assert!(!values[2].is_nil());
}

#[test]
fn string_reads_report_end_of_file_on_the_input_stream() {
    let runtime = Runtime::start().unwrap();
    let error = runtime
        .eval(r#"(read (make-string-input-stream "(a") nil :eof)"#)
        .err()
        .unwrap();
    assert_eq!(
        error.kind(),
        EvalErrorKind::LispCondition(ConditionClass::EndOfFile)
    );
    let values = runtime
        .eval(
            r#"(let ((s (make-string-input-stream "a b")))
                 (values (read-preserving-whitespace s)
                         (read-char s)
                         (read s)
                         (handler-case (read-char s)
                           (end-of-file (condition)
                             (eq (stream-error-stream condition) s)))))"#,
        )
        .unwrap();
    assert_eq!(runtime.symbol_name(&values[0]).as_deref(), Some("A"));
    assert_eq!(values[1].as_character(), Some(' '));
    assert_eq!(runtime.symbol_name(&values[2]).as_deref(), Some("B"));
    assert!(!values[3].is_nil());
}

#[test]
fn extracting_string_output_empties_the_stream() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-streams", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();

    let values = runtime
        .eval(
            r#"(let ((s (make-string-output-stream)))
                 (let ((*standard-output* s))
                   (fresh-line)
                   (write-string "αλω" nil :start 1 :end 2)
                   (collect-streams)
                   (fresh-line)
                   (write-line "β")
                   (finish-output))
                 (values (get-output-stream-string s)
                         (get-output-stream-string s)
                         (stream-element-type s)))"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("λ\nβ\n"));
    assert_eq!(runtime.string(&values[1]).as_deref(), Some(""));
    assert_eq!(
        runtime.symbol_name(&values[2]).as_deref(),
        Some("CHARACTER")
    );
}

#[test]
fn with_output_to_string_returns_the_emitted_text() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            r#"(with-output-to-string (s)
                 (princ "value: " s)
                 (prin1 '(1 2) s)
                 (terpri s))"#,
        )
        .unwrap();
    assert_eq!(
        runtime.string(&values[0]).as_deref(),
        Some("value: (1 2)\n")
    );
}

#[test]
fn standard_streams_report_their_io_direction() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval(
            "(values (input-stream-p *standard-input*) (output-stream-p *standard-output*) \
                     (output-stream-p *error-output*) (input-stream-p *terminal-io*) \
                     (output-stream-p *terminal-io*))",
        )
        .unwrap();
    assert_eq!(values.len(), 5);
    assert!(values.iter().all(|value| !value.is_nil()));
}
