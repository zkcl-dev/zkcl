use zkcl::Runtime;

#[test]
fn displaced_character_arrays_use_view_local_fill_pointers() {
    let runtime = Runtime::start().unwrap();
    runtime
        .register_function("collect-character-arrays", |runtime, _| {
            runtime.collect();
            Ok(vec![])
        })
        .unwrap();
    let values = runtime
        .eval(
            r#"(let* ((matrix (make-array '(2 3) :element-type 'character
                                         :initial-contents '("aé雪" "b😀c")))
                      (window (make-array 4 :element-type 'character :displaced-to matrix
                                         :displaced-index-offset 1 :fill-pointer 2))
                      (nested (make-array 2 :element-type 'character :displaced-to window
                                         :displaced-index-offset 2)))
                 (assert (not (stringp matrix)))
                 (assert (typep matrix '(array character (2 3))))
                 (assert (typep window '(vector character 4)))
                 (assert (typep window '(string 4)))
                 (assert (not (typep window '(simple-string))))
                 (assert (stringp nested))
                 (assert (equal window "é雪"))
                 (assert (equalp window (vector #\É #\雪)))
                 (assert (= (sxhash window) (sxhash "é雪")))
                 (assert (= (length window) 2))
                 (assert (char= (aref window 3) #\😀))
                 (assert (char= (char window 3) #\😀))
                 (assert (not (ignore-errors (elt window 3))))
                 (assert (not (ignore-errors (schar window 0))))
                 (setf (char nested 1) #\x)
                 (setf (row-major-aref matrix 1) #\😀)
                 (setf (fill-pointer window) 4)
                 (collect-character-arrays)
                 (multiple-value-bind (target offset) (array-displacement nested)
                   (assert (eq target window))
                   (assert (= offset 2)))
                 (assert (char= (aref matrix 1 1) #\x))
                 (assert (char= (elt window 0) #\😀))
                 (values window nested))"#,
        )
        .unwrap();

    assert_eq!(runtime.string(&values[0]).as_deref(), Some("😀雪bx"));
    assert_eq!(runtime.string(&values[1]).as_deref(), Some("bx"));
    assert_eq!(runtime.repr(&values[0]).unwrap(), "\"😀雪bx\"");

    let values = runtime
        .eval(
            r#"(let* ((source (make-array 4 :element-type 'character :initial-contents "ab雪c"))
                      (matrix (make-array '(1 2) :element-type 'character
                                          :displaced-to source :displaced-index-offset 1))
                      (scalar (make-array nil :element-type 'character :displaced-to matrix)))
                 (setf (aref scalar) #\😀)
                 (values source (aref matrix 0 0) (aref scalar)))"#,
        )
        .unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("a😀雪c"));
    assert_eq!(values[1].as_character(), Some('😀'));
    assert_eq!(values[2].as_character(), Some('😀'));
}
