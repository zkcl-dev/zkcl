use zkcl::Runtime;

fn runtime_with_collection_callback() -> Runtime {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-string-heap", |rt, _| {
            rt.collect();
            Ok(vec![])
        })
        .unwrap();
    runtime
}

#[test]
fn unicode_string_writes_keep_aliases_at_the_same_character_indices() {
    let runtime = runtime_with_collection_callback();
    let values = runtime
        .eval(
            r#"(let* ((text (make-string 3 :initial-element #\a))
                      (alias text)
                      (wide (setf (char text 1) #\😀)))
                 (collect-string-heap)
                 (let ((saved (char alias 1)))
                   (funcall (fdefinition '(setf schar)) #\雪 alias 0)
                   (setf (elt text 1) #\b)
                   (setf (char alias 2) #\c)
                   (collect-string-heap)
                   (values text alias wide saved (length text)
                           (char text 0) (schar text 1) (elt text 2))))"#,
        )
        .unwrap();
    assert!(values[0].same_identity(&values[1]));
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("雪bc"));
    assert_eq!(values[2].as_character(), Some('😀'));
    assert_eq!(values[3].as_character(), Some('😀'));
    assert_eq!(values[4].as_fixnum(), Some(3));
    assert_eq!(values[5].as_character(), Some('雪'));
    assert_eq!(values[6].as_character(), Some('b'));
    assert_eq!(values[7].as_character(), Some('c'));
}

#[test]
fn displaced_string_writes_update_the_shared_character_storage() {
    let runtime = Runtime::start().unwrap();
    runtime
        .load_str(
            r#"(defvar *wide-text* (copy-seq "abcdefgh"))
               (defvar *wide-view* (make-array 5 :element-type 'character
                                    :displaced-to *wide-text* :displaced-index-offset 2))
               (setf (char *wide-view* 1) #\😀)
               (setf (char *wide-text* 7) #\é)"#,
        )
        .unwrap();
    let text = runtime.eval("*wide-text*").unwrap().remove(0);
    let view = runtime.eval("*wide-view*").unwrap().remove(0);
    assert_eq!(runtime.string(&view).as_deref(), Some("c😀efg"));
    assert_eq!(runtime.string(&text).as_deref(), Some("abc😀efgé"));
    runtime.collect();
    runtime
        .load_str(
            r#"(replace *wide-view* "雪xñ" :start1 1)
               (setf (schar *wide-text* 2) #\𐍈)"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&text).as_deref(), Some("ab𐍈雪xñgé"));
    assert_eq!(runtime.string(&view).as_deref(), Some("𐍈雪xñg"));
    runtime.eval("(fill *wide-text* #\\z)").unwrap();
    assert_eq!(runtime.string(&text).as_deref(), Some("zzzzzzzz"));
    assert_eq!(runtime.string(&view).as_deref(), Some("zzzzz"));
}

#[test]
fn a_string_input_stream_keeps_its_input_after_source_mutation() {
    let runtime = runtime_with_collection_callback();
    let values = runtime
        .eval(
            r#"(let ((text (make-string 3 :initial-element #\é)))
                 (with-input-from-string (input text :start 1)
                   (setf (char text 0) #\a)
                   (setf (char text 1) #\雪)
                   (collect-string-heap)
                   (values (read-char input) (read-char input) (read-char input nil nil))))"#,
        )
        .unwrap();
    assert_eq!(values[0].as_character(), Some('é'));
    assert_eq!(values[1].as_character(), Some('é'));
    assert!(values[2].is_nil());
}

#[test]
fn make_string_preserves_the_requested_length_for_large_strings() {
    let runtime = Runtime::start().unwrap();
    let values = runtime
        .eval("(length (make-string 100000 :initial-element #\\g))")
        .unwrap();
    assert_eq!(values[0].as_fixnum(), Some(100000));
}

#[test]
fn string_operations_reject_invalid_characters_or_indices() {
    let runtime = Runtime::start().unwrap();
    assert!(runtime.eval("(setf (char (make-string 1) 0) 42)").is_err());
    assert!(runtime.eval("(setf (elt (make-string 1) 1) #\\a)").is_err());
    assert!(
        runtime
            .eval("(make-string 1 :element-type 'standard-char :initial-element #\\雪)")
            .is_err()
    );
}
