use std::fs;

use zkcl::Runtime;
use zkcl_bytecode::{ArrayContents, ArrayElement, CompiledFile, LiteralNode};

mod common;

use common::files::TempDir;

#[test]
fn fasl_round_trip_keeps_octets_packed_and_preserves_displacement() {
    let directory = TempDir::new("octet-fasl");
    let source = directory.path().join("octets.lisp");
    fs::write(&source, r#"
      (defmacro octet-pair ()
        (let* ((base (make-array 3 :element-type '(unsigned-byte 8) :initial-contents '(0 254 255)
                                 :fill-pointer 2 :adjustable t))
               (view (make-array 2 :element-type '(unsigned-byte 8) :displaced-to base :displaced-index-offset 1)))
          (list 'quote (list base view))))
      (defun octet-literals () (octet-pair))
    "#).unwrap();
    let compiled = Runtime::start()
        .unwrap()
        .compile_file(&source, None)
        .unwrap();
    let file = CompiledFile::decode(&fs::read(&compiled).unwrap()).unwrap();
    assert!(file.nodes.iter().any(|node| matches!(node, LiteralNode::Array {
        element: ArrayElement::Octet, contents: ArrayContents::Octets(bytes), fill_pointer: Some(2), ..
    } if bytes == &[0, 254, 255])));
    fs::remove_file(source).unwrap();
    let runtime = Runtime::start().unwrap();
    runtime.load_file(&compiled).unwrap();
    runtime
        .load_str(
            r#"
      (destructuring-bind (base view) (octet-literals)
        (zk:gc)
        (assert (= 2 (length base)))
        (assert (equal '(unsigned-byte 8) (upgraded-array-element-type '(integer 0 255))))
        (assert (typep view (type-of view)))
        (assert (subtypep '(vector (unsigned-byte 8)) '(vector (integer 0 255))))
        (assert (not (subtypep '(vector (unsigned-byte 8)) 'simple-vector)))
        (assert (= 255 (aref view 1)))
        (setf (aref view 0) 17)
        (assert (= 17 (aref base 1)))
        (assert (handler-case (progn (setf (aref view 0) 256) nil) (type-error () t)))
        (assert (equal '(unsigned-byte 8) (array-element-type (subseq view 0 1)))))
    "#,
        )
        .unwrap();
}

#[test]
fn fasl_round_trip_restores_displaced_array_views() {
    let temp_dir = TempDir::new("array-fasl");
    let runtime = Runtime::start().unwrap();
    let input = temp_dir.path().join("arrays.lisp");
    fs::write(
        &input,
        r#"(eval-when (:compile-toplevel)
             (let* ((base (make-array 2 :initial-contents '(nil :before)
                                       :fill-pointer 1 :adjustable t))
                    (view (make-array 1 :displaced-to base :displaced-index-offset 1))
                    (bits (make-array 3 :element-type 'bit :initial-contents '(1 0 1))))
               (setf (aref base 0) view)
               (defparameter *saved-array-literals* (list view bits)))
             (defmacro take-array-literals () (list 'quote *saved-array-literals*)))
           (defun stored-array-literals () (take-array-literals))"#,
    )
    .unwrap();
    let compiled = runtime.compile_file(&input, None).unwrap();
    let file = CompiledFile::decode(&fs::read(&compiled).unwrap()).unwrap();
    assert!(file.nodes.iter().any(|node| matches!(
        node,
        LiteralNode::Array {
            contents: ArrayContents::Displaced { offset: 1, .. },
            ..
        }
    )));
    fs::remove_file(input).unwrap();
    runtime.collect();
    runtime.load_file(&compiled).unwrap();
    runtime.collect();

    let results = runtime
        .eval(
            "(let* ((literals (stored-array-literals)) (view (first literals)) (bits (second literals)))
               (multiple-value-bind (base offset) (array-displacement view)
                 (let ((before (aref view 0)))
                   (setf (aref view 0) :after)
                   (values (eq (aref base 0) view)
                           (= offset 1)
                           (= (fill-pointer base) 1)
                           (adjustable-array-p base)
                           (eq before :before)
                           (eq (aref base 1) :after)
                           (bit-vector-p bits)
                           (= (bit bits 2) 1)))))",
        )
        .unwrap();
    assert_eq!(results.len(), 8);
    assert!(results.iter().all(|value| !value.is_nil()));
}

#[test]
fn fasl_round_trip_preserves_specialized_float_storage() {
    let temp_dir = TempDir::new("float-array-fasl");
    let runtime = Runtime::start().unwrap();
    let input = temp_dir.path().join("float-arrays.lisp");
    fs::write(
        &input,
        r#"(eval-when (:compile-toplevel)
             (let* ((single (make-array 2 :element-type 'single-float
                                        :initial-contents (list -0.0f0 least-positive-single-float)))
                    (double (make-array 2 :element-type 'double-float :adjustable t :fill-pointer 1
                                        :initial-contents (list -0.0d0 least-positive-double-float)))
                    (view (make-array 1 :element-type 'double-float
                                      :displaced-to double :displaced-index-offset 1)))
               (defparameter *saved-float-arrays* (list single double view))))
           (defmacro take-float-arrays () (list 'quote *saved-float-arrays*))
           (defun stored-float-arrays () (take-float-arrays))"#,
    )
    .unwrap();
    let compiled = runtime.compile_file(&input, None).unwrap();
    let file = CompiledFile::decode(&fs::read(&compiled).unwrap()).unwrap();
    assert!(
        file.nodes
            .iter()
            .any(|node| matches!(node, LiteralNode::Array {
        element: ArrayElement::SingleFloat,
        contents: ArrayContents::SingleFloats(bits),
        ..
    } if bits == &[(-0.0f32).to_bits(), 1]))
    );
    let double = file
        .nodes
        .iter()
        .position(|node| {
            matches!(node, LiteralNode::Array {
        element: ArrayElement::DoubleFloat,
        contents: ArrayContents::DoubleFloats(bits),
        adjustable: true,
        fill_pointer: Some(1),
        ..
    } if bits == &[(-0.0f64).to_bits(), 1])
        })
        .unwrap();
    assert!(
        file.nodes
            .iter()
            .any(|node| matches!(node, LiteralNode::Array {
        element: ArrayElement::DoubleFloat,
        contents: ArrayContents::Displaced { target, offset: 1 },
        ..
    } if *target as usize == double))
    );

    fs::remove_file(input).unwrap();
    runtime.collect();
    runtime.load_file(&compiled).unwrap();
    runtime.collect();

    let results = runtime
        .eval(
            "(destructuring-bind (single double view) (stored-float-arrays)
                 (values (aref single 0) (aref single 1) (aref double 0) (aref view 0)))",
        )
        .unwrap();
    assert_eq!(
        results[0].as_single_float().unwrap().to_bits(),
        (-0.0f32).to_bits()
    );
    assert_eq!(results[1].as_single_float().unwrap().to_bits(), 1);
    assert_eq!(
        results[2].as_double_float().unwrap().to_bits(),
        (-0.0f64).to_bits()
    );
    assert_eq!(results[3].as_double_float().unwrap().to_bits(), 1);

    let results = runtime
        .eval(
            "(destructuring-bind (single double view) (stored-float-arrays)
                 (declare (ignore single))
                 (setf (aref view 0) 2.5d0)
                 (values (eq (array-displacement view) double) (aref double 1)))",
        )
        .unwrap();
    assert!(!results[0].is_nil());
    assert_eq!(results[1].as_double_float(), Some(2.5));
}
