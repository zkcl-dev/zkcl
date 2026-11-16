use std::fs;

use zkcl::Runtime;
use zkcl_bytecode::{ArrayContents, ArrayElement, CompiledFile, LiteralNode};

mod common;

use common::files::TempDir;

#[test]
fn fasl_round_trip_restores_nested_character_array_views() {
    let temp_dir = TempDir::new("character-array-fasl");
    let runtime = Runtime::start().unwrap();
    let input = temp_dir.path().join("character-arrays.lisp");
    fs::write(
        &input,
        r#"(eval-when (:compile-toplevel)
             (let* ((base "猫λaz")
                    (middle (make-array 3 :element-type 'character :adjustable t
                                          :fill-pointer 2 :displaced-to base
                                          :displaced-index-offset 1))
                    (view (make-array 1 :element-type 'character :displaced-to middle
                                        :displaced-index-offset 1))
                    (owned (make-array 4 :element-type 'character :adjustable t
                                         :fill-pointer 2 :initial-contents "界abz"))
                    (matrix (make-array '(1 2) :element-type 'character
                                               :initial-contents '((#\λ #\猫)))))
               (defparameter *saved-character-arrays* (list view middle base owned matrix)))
             (defmacro take-character-arrays () (list 'quote *saved-character-arrays*)))
           (defun stored-character-arrays () (take-character-arrays))"#,
    )
    .unwrap();
    let compiled = runtime.compile_file(&input, None).unwrap();
    let file = CompiledFile::decode(&fs::read(&compiled).unwrap()).unwrap();
    let middle = file
        .nodes
        .iter()
        .position(|node| {
            matches!(
                node,
                LiteralNode::Array {
                    element: ArrayElement::Character,
                    adjustable: true,
                    fill_pointer: Some(2),
                    contents: ArrayContents::Displaced { target, offset: 1 },
                    ..
                } if file.nodes[*target as usize] == LiteralNode::String("猫λaz".into())
            )
        })
        .expect("the intermediate view points directly to the native string");
    assert!(file.nodes.iter().any(|node| matches!(
        node,
        LiteralNode::Array {
            element: ArrayElement::Character,
            contents: ArrayContents::Displaced { target, offset: 1 },
            ..
        } if *target as usize == middle
    )));
    fs::remove_file(input).unwrap();
    runtime.collect();
    runtime.load_file(&compiled).unwrap();
    runtime.collect();

    let results = runtime
        .eval(
            r#"(destructuring-bind (view middle base owned matrix) (stored-character-arrays)
                 (multiple-value-bind (view-target view-offset) (array-displacement view)
                   (multiple-value-bind (middle-target middle-offset) (array-displacement middle)
                     (let ((before (aref view 0)))
                       (setf (aref view 0) #\🦀)
                       (values (eq view-target middle)
                               (= view-offset 1)
                               (eq middle-target base)
                               (= middle-offset 1)
                               (and (stringp view) (stringp middle) (stringp base) (stringp owned))
                               (and (adjustable-array-p middle) (adjustable-array-p owned))
                               (and (= (fill-pointer middle) 2) (= (fill-pointer owned) 2))
                               (and (= (length middle) 2) (= (length owned) 2))
                               (eql before #\a)
                               (and (eql (aref base 2) #\🦀) (eql (aref middle 1) #\🦀))
                               (eql (aref owned 3) #\z)
                               (and (= (array-rank matrix) 2) (eql (aref matrix 0 1) #\猫)))))))"#,
        )
        .unwrap();
    assert_eq!(results.len(), 12);
    assert!(results.iter().all(|value| !value.is_nil()));
}
