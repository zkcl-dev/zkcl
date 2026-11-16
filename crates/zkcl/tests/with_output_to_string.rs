use zkcl::Runtime;

#[test]
fn output_to_an_existing_string_appends_through_its_fill_pointer() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-output", |runtime, _| {
            runtime.collect();
            Ok(Vec::new())
        })
        .unwrap();

    let values = runtime
        .eval(
            r#"(let* ((backing (copy-seq "abçdefg"))
                      (string (make-array 4 :element-type 'character
                                          :displaced-to backing :displaced-index-offset 1
                                          :fill-pointer 1)))
                 (with-output-to-string (out string)
                   (declare (type stream out))
                   (format out "~A" "界λ")
                   (collect-output)
                   (write-char #\終 out)
                   (values))
                 (values string backing (fill-pointer string)
                         (eq (array-displacement string) backing)))"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("b界λ終"));
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("ab界λ終fg"));
    assert_eq!(values[2].as_fixnum(), Some(4));
    assert!(!values[3].is_nil());

    let values = runtime
        .eval(
            r#"(let* ((backing (copy-seq "abcdefg"))
                      (string (make-array 3 :element-type 'character :adjustable t
                                          :displaced-to backing :displaced-index-offset 1
                                          :fill-pointer 1))
                      (alias string)
                      (calls 0))
                 (multiple-value-bind (first second)
                     (with-output-to-string (out (progn (incf calls) string)
                                                 :element-type (progn (incf calls) 'bit))
                       (write-string "界λ漢" out)
                       (collect-output)
                       (write-char #\終 out)
                       (values 19 #\x))
                   (values string backing (fill-pointer alias) (eq string alias)
                           first second calls (array-displacement string))))"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("b界λ漢終"));
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("ab界λefg"));
    assert_eq!(values[2].as_fixnum(), Some(5));
    assert!(!values[3].is_nil());
    assert_eq!(values[4].as_fixnum(), Some(19));
    assert_eq!(values[5].as_character(), Some('x'));
    assert_eq!(values[6].as_fixnum(), Some(2));
    assert!(values[7].is_nil());

    let values = runtime
        .eval(
            r#"(let ((string (make-array 3 :element-type 'character :fill-pointer 2
                                           :initial-contents (list #\λ #\Newline #\_)))
                     escaped)
                 (multiple-value-bind (result index)
                     (catch 'done
                       (with-output-to-string (out string)
                         (setq escaped out)
                         (unless (fresh-line out) (write-char #\終 out))
                         (throw 'done (values 42 (fill-pointer string)))))
                   (values string (open-stream-p escaped) result index)))"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("λ\n終"));
    assert!(values[1].is_nil());
    assert_eq!(values[2].as_fixnum(), Some(42));
    assert_eq!(values[3].as_fixnum(), Some(3));

    let values = runtime
        .eval(
            r#"(let ((string (make-array 1 :element-type 'character :fill-pointer 0)))
                 (values (handler-case
                             (with-output-to-string (out string) (write-string "éλ" out))
                           (type-error (condition) (eq (type-error-datum condition) string)))
                         string (fill-pointer string)
                         (handler-case (with-output-to-string (out "no fill pointer") (values))
                           (type-error () t))
                         (with-output-to-string (out nil :element-type 'character)
                           (write-string "new" out))))"#,
        )
        .unwrap();
    assert!(!values[0].is_nil());
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("é"));
    assert_eq!(values[2].as_fixnum(), Some(1));
    assert!(!values[3].is_nil());
    assert_eq!(runtime.string(&values[4]).as_deref(), Some("new"));
}
