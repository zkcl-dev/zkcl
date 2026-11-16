use std::fs;
use std::path::Path;
use std::process::Command;

use zkcl::Runtime;

mod common;

use common::files::TempDir;
use common::lisp::{assert_truthy, eval, lisp_path};

#[test]
fn simd_values_have_distinct_types_and_object_identity() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(progn
             (require :zk-simd)
             (assert (member :zk-simd *features*))
             (assert (stringp (documentation 'zk-simd:load-masked 'function)))
             (defgeneric simd-kind (value))
             (defmethod simd-kind ((value zk-simd:pack)) :pack)
             (defmethod simd-kind ((value zk-simd:mask)) :mask)
             (let* ((pack (zk-simd:make-pack 'zk-simd:f32x4 1.0 2.0 3.0 4.0))
                    (copy (zk-simd:make-pack '(zk-simd:pack :f32 4) 1.0 2.0 3.0 4.0))
                    (mask (zk-simd:mask-first 4 2))
                    (other-mask (zk-simd:mask-from-bits 4 3)))
               (assert (zk-simd:pack-p pack))
               (assert (zk-simd:mask-p mask))
               (assert (not (zk-simd:mask-p pack)))
               (assert (not (zk-simd:pack-p mask)))
               (assert (eq (type-of pack) 'zk-simd:f32x4))
               (assert (equal (type-of mask) '(zk-simd:mask 4)))
               (assert (eq (class-of pack) (find-class 'zk-simd:pack)))
               (assert (eq (class-of mask) (find-class 'zk-simd:mask)))
               (assert (eq (simd-kind pack) :pack))
               (assert (eq (simd-kind mask) :mask))
               (dolist (type '(zk-simd:pack zk-simd:f32x4 (zk-simd:pack :f32 4)
                               (zk-simd:pack :f32 *) (zk-simd:pack * 4)))
                 (assert (typep pack type))
                 (assert (equal '(t t) (multiple-value-list (subtypep 'zk-simd:f32x4 type)))))
               (assert (equal '(t t) (multiple-value-list (subtypep '(zk-simd:mask 4) 'zk-simd:mask))))
               (assert (equal '(nil t) (multiple-value-list (subtypep 'zk-simd:f32x4 'zk-simd:f64x4))))
               (assert (equal '(nil t) (multiple-value-list (subtypep 'zk-simd:pack 'number))))
               (dolist (type '(number complex array vector sequence zk-simd:mask zk-simd:i32x4))
                 (assert (not (typep pack type))))
               (assert (not (typep mask '(zk-simd:mask 8))))
               (dolist (test (list #'eq #'eql #'equal #'equalp))
                 (assert (funcall test pack pack))
                 (assert (funcall test mask mask))
                 (assert (not (funcall test pack copy)))
                 (assert (not (funcall test mask other-mask))))
               (assert (zk-simd:same-bits-p pack copy))
               (assert (zk-simd:same-bits-p mask other-mask))
               (assert (not (zk-simd:same-bits-p pack mask)))
               (assert (= (zk-simd:lane-count pack) (zk-simd:lane-count mask) 4))
               (assert (equalp (zk-simd:pack-lanes pack) #(1.0 2.0 3.0 4.0)))
               (setf (aref (zk-simd:pack-lanes pack) 0) 42)
               (assert (= (zk-simd:lane pack 0) 1.0))
               (assert (not (eq pack (zk-simd:select (zk-simd:mask-first 4 4) pack copy)))))
             t)"#,
    );
}

#[test]
fn simd_printer_preserves_conditions_and_describes_values() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let ((pack (zk-simd:make-pack 'zk-simd:f32x4 1.0 -0.0 3.0 4.0))
                 (mask (zk-simd:mask-from-bits 4 9)))
             (let ((printed (string-upcase (write-to-string pack :readably nil))))
               (assert (search "F32X4" printed))
               (assert (search "-0.0" printed)))
             (assert (search "MASK" (string-upcase (write-to-string mask :readably nil))))
             (dolist (value (list pack mask))
               (assert (handler-case (progn (write-to-string value :readably t) nil)
                         (print-not-readable (condition)
                           (eq (print-not-readable-object condition) value)))))
             t)"#,
    );
}

const SIMD_DEFINITIONS: &str = r#"
    (defun simd-step (array)
      (let* ((x (zk-simd:load-pack 'zk-simd:f32x4 array 0))
             (twice (zk-simd:mul x (zk-simd:splat 'zk-simd:f32x4 2.0)))
             (result (zk-simd:add twice (zk-simd:splat 'zk-simd:f32x4 1.0))))
        (values (zk-simd:pack-lanes result)
                (zk-simd:mask-bits (zk-simd:cmp> result twice)))))
    (defun saved-simd-objects ()
      #.(let ((pack (zk-simd:make-pack 'zk-simd:f32x4 1.0 -0.0 3.0 4.0))
              (mask (zk-simd:mask-from-bits 4 9)))
          (list 'quote (list pack mask pack mask
                            (zk-simd:make-pack 'zk-simd:f32x4 1.0 -0.0 3.0 4.0)
                            (zk-simd:mask-from-bits 4 9)))))
"#;

const CHECK_SIMD_DEFINITIONS: &str = r#"(progn
    (let ((array (make-array 4 :element-type 'single-float :initial-contents '(0.0 1.0 2.0 3.0))))
      (multiple-value-bind (lanes bits) (simd-step array)
        (assert (equalp lanes #(1.0 3.0 5.0 7.0)))
        (assert (= bits #b1111))))
    (destructuring-bind (pack mask shared-pack shared-mask distinct-pack distinct-mask) (saved-simd-objects)
      (zk:gc)
      (assert (eq pack shared-pack))
      (assert (eq mask shared-mask))
      (assert (eq pack (first (saved-simd-objects))))
      (assert (not (eq pack distinct-pack)))
      (assert (not (eq mask distinct-mask)))
      (assert (zk-simd:same-bits-p pack distinct-pack))
      (assert (zk-simd:same-bits-p mask distinct-mask))
      (assert (equalp (zk-simd:pack-lanes (zk-simd:bitcast 'zk-simd:u32x4 pack))
                      #(#x3f800000 #x80000000 #x40400000 #x40800000)))
      (assert (= (zk-simd:mask-bits mask) 9)))
    t)"#;

#[test]
fn simd_source_compile_and_fasl_agree() {
    const FASL: &str = "ZKCL_TEST_SIMD_FASL";
    if let Some(path) = std::env::var_os(FASL) {
        let loader = Runtime::start().unwrap();
        assert_truthy(&loader, &format!("(load {})", lisp_path(Path::new(&path))));
        loader.collect();
        assert_truthy(&loader, CHECK_SIMD_DEFINITIONS);
        return;
    }

    let compiler = Runtime::start().unwrap();
    compiler.load_str(SIMD_DEFINITIONS).unwrap();
    assert_truthy(&compiler, CHECK_SIMD_DEFINITIONS);
    eval(
        &compiler,
        "(progn (compile 'simd-step) (compile 'saved-simd-objects))",
    );
    assert_truthy(&compiler, CHECK_SIMD_DEFINITIONS);

    let directory = TempDir::new("simd-fasl");
    let source = directory.write("simd.lisp", SIMD_DEFINITIONS);
    let compiled = directory.path().join("simd.zkclfasl");
    assert_truthy(
        &compiler,
        &format!(
            "(multiple-value-bind (file warnings failure) (compile-file {} :output-file {})
               (declare (ignore warnings))
               (and (pathnamep file) (not failure)))",
            lisp_path(&source),
            lisp_path(&compiled)
        ),
    );
    fs::remove_file(source).unwrap();
    drop(compiler);

    let loaded = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "simd_source_compile_and_fasl_agree",
            "--nocapture",
        ])
        .env(FASL, &compiled)
        .output()
        .expect("start a fresh process to load the FASL");
    assert!(
        loaded.status.success(),
        "FASL loader failed:\n{}\n{}",
        String::from_utf8_lossy(&loaded.stdout),
        String::from_utf8_lossy(&loaded.stderr),
    );
}

#[test]
fn simd_integer_operations_keep_lane_width_and_report_first_overflow() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let* ((x (zk-simd:make-pack 'zk-simd:i32x4 7 2147483647 -2147483648 4))
                  (y (zk-simd:make-pack 'zk-simd:i32x4 2 1 -1 1))
                  (max #xffffffffffffffff)
                  (wide (zk-simd:make-pack 'zk-simd:u64x2 max 0))
                  (wide-y (zk-simd:make-pack 'zk-simd:u64x2 1 max)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:add x y))
                             #(9 -2147483648 2147483647 5)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:add-saturating x y))
                             #(9 2147483647 -2147483648 5)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:sub-saturating wide wide-y))
                             (vector (1- max) 0)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:sub wide wide-y))
                             (vector (1- max) 1)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:add wide wide-y)) (vector 0 max)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:add-saturating wide wide-y)) (vector max max)))
             (assert (= (zk-simd:lane wide 0) max))
             (assert (= (zk-simd:mask-bits (zk-simd:cmp> wide wide-y)) 1))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:shift-right x 100000000000000000000))
                             #(0 0 -1 0)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:shift-left wide 64)) #(0 0)))
             (assert (handler-case (progn (zk-simd:add-checked x y) nil)
                       (zk-simd:integer-overflow (condition)
                         (zk:gc)
                         (and (typep condition 'arithmetic-error)
                              (= (zk-simd:condition-lane condition) 1)
                              (eq (arithmetic-error-operation condition) 'zk-simd:add-checked)
                              (let ((operands (arithmetic-error-operands condition)))
                                (and (eq (first operands) x) (eq (second operands) y)))))))
             (assert (handler-case (progn (zk-simd:sub-checked wide wide-y) nil)
                       (zk-simd:integer-overflow (condition)
                         (= (zk-simd:condition-lane condition) 1))))
             t)"#,
    );
}

#[test]
fn simd_conversions_distinguish_values_from_little_endian_bits() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let* ((words (zk-simd:make-pack 'zk-simd:u32x4 #x04030201 #x80706050 #x0000ffff #xff000000))
                  (bytes (zk-simd:pack-bits words))
                  (octets #(1 2 3 4 80 96 112 128 255 255 0 0 0 0 0 255))
                  (rounding-input (zk-simd:make-pack 'zk-simd:i32x4 16777217 -16777217 16777219 0))
                  (truncation-input (zk-simd:make-pack 'zk-simd:f64x4 2.9d0 -2.9d0 -0.75d0 0.75d0)))
             (assert (typep bytes '(vector (unsigned-byte 8))))
             (assert (equalp bytes octets))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:bitcast 'zk-simd:u8x16 words)) octets))
             (assert (zk-simd:same-bits-p words (zk-simd:pack-from-bits 'zk-simd:u32x4 bytes)))
             (setf (aref bytes 0) 99)
             (assert (= (zk-simd:lane words 0) #x04030201))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:convert 'zk-simd:f32x4 rounding-input))
                             #(16777216.0 -16777216.0 16777220.0 0.0)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:convert 'zk-simd:i32x4 truncation-input))
                             #(2 -2 0 0)))
             (assert (handler-case
                         (progn (zk-simd:convert 'zk-simd:u32x4 truncation-input) nil)
                       (zk-simd:integer-overflow (condition)
                         (= (zk-simd:condition-lane condition) 1))))
             (assert (handler-case (progn (zk-simd:convert 'zk-simd:f64x2 rounding-input) nil)
                       (type-error () t)))
             t)"#,
    );
}

#[test]
fn simd_float_operations_keep_exact_zeros_subnormals_and_fused_rounding() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(flet ((f32 (bits) (zk-simd:bitcast 'zk-simd:f32x4 (apply #'zk-simd:make-pack 'zk-simd:u32x4 bits)))
                 (f64 (bits) (zk-simd:bitcast 'zk-simd:f64x2 (apply #'zk-simd:make-pack 'zk-simd:u64x2 bits)))
                 (bits32 (pack) (zk-simd:pack-lanes (zk-simd:bitcast 'zk-simd:u32x4 pack)))
                 (bits64 (pack) (zk-simd:pack-lanes (zk-simd:bitcast 'zk-simd:u64x2 pack))))
             (let* ((x (f32 '(#x3f800400 #x00800000 #x80000000 1)))
                    (y (f32 '(#x3f7ff800 #x3f000000 #x3f800000 #x3f800000)))
                    (z (f32 '(#xbf800000 0 #x80000000 1)))
                    (zero (zk-simd:splat 'zk-simd:f32x4 0.0))
                    (signed-zero (f32 '(0 #x80000000 #x80000000 0)))
                    (a (f64 '(#x3ff0000002000000 #x0010000000000000)))
                    (b (f64 '(#x3feffffffc000000 #x3fe0000000000000)))
                    (c (f64 '(#xbff0000000000000 0))))
               (assert (equalp (bits32 (zk-simd:add (zk-simd:mul x y) z))
                               #(0 #x00400000 #x80000000 2)))
               (assert (equalp (bits32 (zk-simd:fma x y z))
                               #(#xb2800000 #x00400000 #x80000000 2)))
               (assert (equalp (bits64 (zk-simd:add (zk-simd:mul a b) c))
                               #(0 #x0008000000000000)))
               (assert (equalp (bits64 (zk-simd:fma a b c))
                               #(#xbc90000000000000 #x0008000000000000)))
               (assert (zk-simd:mask-all-p (zk-simd:cmp= zero signed-zero)))
               (assert (not (zk-simd:same-bits-p zero signed-zero)))
               (assert (zk-simd:same-bits-p (zk-simd:sqrt signed-zero) signed-zero))
               (assert (handler-case (progn (f32 '(0 #x7f800000 #x7fc00000 0)) nil)
                         (floating-point-invalid-operation (condition)
                           (= (zk-simd:condition-lane condition) 1))))
               (assert (handler-case
                           (progn (zk-simd:mul (f32 '(0 #x7f7fffff #x7f7fffff 0))
                                             (zk-simd:splat 'zk-simd:f32x4 2.0)) nil)
                         (floating-point-overflow (condition)
                           (= (zk-simd:condition-lane condition) 1)))))
             t)"#,
    );
}

#[test]
fn simd_masked_arithmetic_evaluates_only_active_lanes() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let* ((x (zk-simd:make-pack 'zk-simd:f32x4 8.0 -1.0 16.0 -4.0))
                  (y (zk-simd:make-pack 'zk-simd:f32x4 2.0 0.0 4.0 -0.0))
                  (pass (zk-simd:splat 'zk-simd:f32x4 -0.0))
                  (active (zk-simd:mask-from-bits 4 #b0101))
                  (invalid (zk-simd:mask-not active))
                  (sqrt-input (zk-simd:make-pack 'zk-simd:f32x4 4.0 -1.0 16.0 -4.0)))
             (assert (zk-simd:same-bits-p (zk-simd:div-masked active x y pass)
                         (zk-simd:make-pack 'zk-simd:f32x4 4.0 -0.0 4.0 -0.0)))
             (assert (zk-simd:same-bits-p (zk-simd:sqrt-masked active sqrt-input pass)
                         (zk-simd:make-pack 'zk-simd:f32x4 2.0 -0.0 4.0 -0.0)))
             (assert (handler-case (progn (zk-simd:div-masked invalid x y pass) nil)
                       (division-by-zero (condition) (= (zk-simd:condition-lane condition) 1))))
             (assert (handler-case (progn (zk-simd:sqrt-masked invalid x pass) nil)
                       (floating-point-invalid-operation (condition)
                         (= (zk-simd:condition-lane condition) 1))))
             (assert (handler-case (progn (zk-simd:div-masked invalid x y 42) nil)
                       (type-error (condition) (= (type-error-datum condition) 42))))
             t)"#,
    );
}

#[test]
fn simd_array_operations_use_row_major_active_bounds_and_check_before_writes() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let* ((base (make-array 10 :element-type 'single-float :fill-pointer 1
                               :initial-contents '(0.0 1.0 2.0 3.0 4.0 5.0 6.0 7.0 8.0 9.0)))
                  (view (make-array 8 :element-type 'single-float :fill-pointer 1
                               :displaced-to base :displaced-index-offset 1))
                  (matrix (make-array '(2 3) :element-type 'single-float
                               :displaced-to view :displaced-index-offset 1))
                  (value (zk-simd:make-pack 'zk-simd:f32x4 10.0 20.0 30.0 40.0))
                  (pass (zk-simd:splat 'zk-simd:f32x4 -0.0))
                  (empty (zk-simd:mask-first 4 0))
                  (tail (zk-simd:mask-first 4 2))
                  (skip (zk-simd:mask-from-bits 4 #b0101))
                  (huge (ash 1 100)))
             (zk:gc)
             (assert (equalp (zk-simd:pack-lanes (zk-simd:load-pack 'zk-simd:f32x4 matrix 2))
                             #(4.0 5.0 6.0 7.0)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:load-pack 'zk-simd:f32x4 view 1))
                             #(2.0 3.0 4.0 5.0)))
             (assert (eq value (zk-simd:store-pack matrix 2 value)))
             (assert (= (aref base 4) (aref matrix 0 2) 10.0))
             (assert (= (aref base 5) (aref matrix 1 0) 20.0))
             (assert (zk-simd:same-bits-p
                         (zk-simd:load-masked 'zk-simd:f32x4 matrix 4 tail pass)
                         (zk-simd:make-pack 'zk-simd:f32x4 30.0 40.0 -0.0 -0.0)))
             (assert (zk-simd:same-bits-p
                         (zk-simd:load-masked 'zk-simd:f32x4 matrix 4 tail)
                         (zk-simd:make-pack 'zk-simd:f32x4 30.0 40.0 0.0 0.0)))
             (assert (zk-simd:same-bits-p
                         (zk-simd:load-masked 'zk-simd:f32x4 matrix huge empty pass) pass))
             (assert (eq (zk-simd:store-masked matrix huge empty value) value))
             (assert (handler-case
                         (progn (zk-simd:load-masked 'zk-simd:f32x4 matrix huge (zk-simd:mask-from-bits 4 4)) nil)
                       (zk-simd:bounds-error (condition)
                         (and (= (zk-simd:condition-index condition) (+ huge 2))
                              (= (zk-simd:condition-lane condition) 2)))))
             (assert (handler-case (progn (zk-simd:store-masked matrix 4 skip value) nil)
                       (zk-simd:bounds-error (condition)
                         (zk:gc)
                         (and (eq (zk-simd:condition-array condition) matrix)
                              (= (zk-simd:condition-index condition) 6)
                              (= (zk-simd:condition-lane condition) 2)))))
             (assert (= (aref matrix 1 1) 30.0))
             (assert (handler-case (progn (zk-simd:store-pack matrix 4 value) nil)
                       (zk-simd:bounds-error (condition)
                         (= (zk-simd:condition-lane condition) 2))))
             (assert (= (aref matrix 1 1) 30.0))
             (assert (eq value (zk-simd:store-masked matrix 4 tail value)))
             (assert (= (aref base 6) 10.0))
             (assert (= (aref base 7) 20.0))
             (assert (= (aref base 8) 8.0))
             (assert (handler-case
                         (progn (zk-simd:load-masked 'zk-simd:f32x4 (vector 1.0) huge empty) nil)
                       (type-error () t)))
             t)"#,
    );
}

#[test]
fn simd_wide_values_and_full_width_mask_survive_moving_gc() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let* ((wide (zk-simd:splat 'zk-simd:u8x64 255))
                  (mask (zk-simd:mask-from-bits 64 #x8000000000000001))
                  (results (make-hash-table :test 'eq))
                  (other (zk-simd:splat 'zk-simd:u8x64 0)))
             (setf (gethash wide results) mask)
             (setf (gethash mask results) wide)
             (zk:gc)
             (assert (= (zk-simd:lane-count wide) (zk-simd:lane-count mask) 64))
             (assert (eq (gethash wide results) mask))
             (assert (eq (gethash mask results) wide))
             (assert (= (zk-simd:lane wide 63) 255))
             (assert (= (zk-simd:mask-bits mask) #x8000000000000001))
             (assert (= (zk-simd:mask-count mask) 2))
             (assert (= (zk-simd:mask-bits (zk-simd:mask-not mask)) #x7ffffffffffffffe))
             (assert (= (zk-simd:mask-bits (zk-simd:mask-first 64 64)) #xffffffffffffffff))
             (let ((selected (zk-simd:select mask wide other)))
               (assert (= (zk-simd:lane selected 0) (zk-simd:lane selected 63) 255))
               (assert (= (zk-simd:lane selected 1) (zk-simd:lane selected 62) 0)))
             t)"#,
    );
}

#[test]
fn simd_permutations_and_reductions_use_logical_lane_order() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let ((x (zk-simd:make-pack 'zk-simd:u32x8 0 1 2 3 4 5 6 7))
                 (y (zk-simd:make-pack 'zk-simd:u32x8 8 9 10 11 12 13 14 15))
                 (sum (zk-simd:make-pack 'zk-simd:f32x4 67108864.0 1.0 -67108864.0 1.0)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:permute x #(7 0 5 4 3 6 2 1)))
                             #(7 0 5 4 3 6 2 1)))
             (assert (equalp (zk-simd:pack-lanes (zk-simd:shuffle x y #(0 8 7 15 4 4 12 3)))
                             #(0 8 7 15 4 4 12 3)))
             (assert (= (zk-simd:reduce-sum sum) (zk-simd:reduce-sum sum :order :left) 1.0))
             (assert (= (zk-simd:reduce-sum sum :order :pairwise) 0.0))
             (assert (= (zk-simd:reduce-sum (zk-simd:make-pack 'zk-simd:u64x2 #xffffffffffffffff 2)) 1))
             t)"#,
    );
}

#[test]
fn simd_gather_and_scatter_skip_inactive_indices_and_order_duplicate_writes() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let* ((array (make-array 3 :element-type 'single-float :initial-contents '(0.0 1.0 2.0)))
                  (indices (zk-simd:make-pack 'zk-simd:i32x4 1 1 99 0))
                  (value (zk-simd:make-pack 'zk-simd:f32x4 10.0 20.0 30.0 40.0))
                  (active (zk-simd:mask-from-bits 4 #b1011))
                  (all (zk-simd:mask-first 4 4))
                  (pass (zk-simd:splat 'zk-simd:f32x4 -0.0)))
             (assert (zk-simd:same-bits-p
                         (zk-simd:gather 'zk-simd:f32x4 array indices active pass)
                         (zk-simd:make-pack 'zk-simd:f32x4 1.0 1.0 -0.0 0.0)))
             (assert (handler-case (progn (zk-simd:scatter array indices all value) nil)
                       (zk-simd:bounds-error (condition)
                         (and (eq (zk-simd:condition-array condition) array)
                              (= (zk-simd:condition-lane condition) 2)
                              (= (zk-simd:condition-index condition) 99)))))
             (assert (equalp array #(0.0 1.0 2.0)))
             (assert (eq (zk-simd:scatter array indices active value) value))
             (assert (equalp array #(40.0 20.0 2.0)))
             t)"#,
    );
}

#[test]
fn simd_bulk_multiply_add_handles_exact_aliases_and_commits_only_successful_prefix() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let* ((base (make-array 7 :element-type 'single-float
                               :initial-contents '(0.0 1.0 2.0 3.0 4.0 5.0 6.0)))
                  (same (make-array 5 :element-type 'single-float :displaced-to base :displaced-index-offset 1))
                  (out (make-array 5 :element-type 'single-float :displaced-to base :displaced-index-offset 1))
                  (partial (make-array 5 :element-type 'single-float :displaced-to base :displaced-index-offset 2))
                  (ones (make-array 5 :element-type 'single-float :initial-element 1.0)))
             (assert (handler-case (progn (zk-simd:mul-add-into out partial 2.0 ones) nil)
                       (error () t)))
             (assert (equalp base #(0.0 1.0 2.0 3.0 4.0 5.0 6.0)))
             (assert (eq (zk-simd:mul-add-into out same 2.0 ones) out))
             (assert (equalp base #(0.0 3.0 5.0 7.0 9.0 11.0 6.0)))
             t)"#,
    );
    assert_truthy(
        &runtime,
        r#"(let* ((max most-positive-single-float)
                  (half-max (scale-float max -1))
                  (a (make-array 5 :element-type 'single-float :initial-contents (list 0.0 1.0 half-max max 0.0)))
                  (b (make-array 5 :element-type 'single-float :initial-contents (list 0.0 1.0 max 0.0 0.0)))
                  (out (make-array 6 :element-type 'single-float :initial-element 42.0)))
             (assert (handler-case
                         (progn (zk-simd:mul-add-into out a 2.0 b :out-start 1 :a-start 1 :b-start 1 :count 4) nil)
                       (floating-point-overflow (condition)
                         (and (= (zk-simd:condition-lane condition) 1)
                              (= (zk-simd:condition-index condition) 2)
                              (eq (arithmetic-error-operation condition) 'zk-simd:add)))))
             (assert (equalp out #(42.0 3.0 42.0 42.0 42.0 42.0)))
             t)"#,
    );
}

#[test]
fn simd_octet_comparison_masks_follow_lane_order_and_ignore_empty_tails() {
    let runtime = Runtime::start().unwrap();
    assert_truthy(
        &runtime,
        r#"(let* ((base (make-array 18 :element-type '(unsigned-byte 8) :initial-element 0 :fill-pointer 0))
                  (view (make-array 16 :element-type '(unsigned-byte 8) :displaced-to base :displaced-index-offset 1)))
             (dolist (lane '(0 5 15)) (setf (aref view lane) 255))
             (assert (= (zk-simd:mask-bits (zk-simd:octet-eq-mask view 0 255)) #x8021))
             (loop for count from 0 to 16
                   for expected = (logand #x8021 (1- (ash 1 count)))
                   do (assert (= (zk-simd:mask-bits (zk-simd:octet-eq-mask view 0 255 count)) expected)))
             (assert (= (zk-simd:mask-bits (zk-simd:octet-eq-mask view 14 255 2)) #b10))
             (assert (= (zk-simd:mask-bits (zk-simd:octet-eq-mask view (ash 1 100) 255 0)) 0))
             (assert (handler-case (progn (zk-simd:octet-eq-mask view 14 255 3) nil)
                       (zk-simd:bounds-error (condition)
                         (and (eq (zk-simd:condition-array condition) view)
                              (= (zk-simd:condition-index condition) 16)
                              (= (zk-simd:condition-lane condition) 2)))))
             t)"#,
    );
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
#[test]
fn simd_ignores_host_rounding_and_flush_to_zero() {
    let runtime = Runtime::start().unwrap();
    let function = eval(
        &runtime,
        r#"(compile nil '(lambda ()
             (let* ((x (zk-simd:make-pack 'zk-simd:f32x4 1.0 least-positive-normalized-single-float
                                          least-positive-single-float -0.0))
                    (half (zk-simd:splat 'zk-simd:f32x4 0.5))
                    (offset (zk-simd:make-pack 'zk-simd:f32x4 2.98023223876953125e-8 0.0 0.0 -0.0))
                    (product (zk-simd:mul x half))
                    (ordinary (zk-simd:bitcast 'zk-simd:u32x4 (zk-simd:add product offset)))
                    (a (make-array 4 :element-type 'single-float :initial-contents (zk-simd:pack-lanes x)))
                    (b (make-array 4 :element-type 'single-float :initial-contents (zk-simd:pack-lanes offset)))
                    (out (make-array 4 :element-type 'single-float)))
               (zk-simd:mul-add-into out a 0.5 b)
               (let ((bulk (zk-simd:bitcast 'zk-simd:u32x4 (zk-simd:load-pack 'zk-simd:f32x4 out 0))))
                 (if (zk-simd:same-bits-p ordinary bulk)
                     (values (zk-simd:lane bulk 0) (zk-simd:lane bulk 1)
                             (zk-simd:lane bulk 2) (zk-simd:lane bulk 3))
                     nil)))))"#,
    );
    let expected = [
        Some(0x3f000000),
        Some(0x00400000),
        Some(0),
        Some(0x80000000),
    ];
    let default = runtime.call(&function[0], &[]).unwrap();
    assert_eq!(
        default
            .iter()
            .map(|value| value.as_fixnum())
            .collect::<Vec<_>>(),
        expected
    );

    let saved = float_control::read();
    for settings in float_control::test_settings(saved) {
        let restore = float_control::Restore(saved);
        float_control::write(settings);
        let result = runtime.call(&function[0], &[]);
        let after = float_control::read();
        drop(restore);

        assert_eq!(
            after & float_control::CONTROL_MASK,
            settings & float_control::CONTROL_MASK
        );
        let values = result.unwrap();
        assert_eq!(
            values
                .iter()
                .map(|value| value.as_fixnum())
                .collect::<Vec<_>>(),
            expected,
            "floating-point controls {settings:#x}"
        );
    }
}

#[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
mod float_control {
    use std::arch::asm;

    pub struct Restore(pub u64);

    impl Drop for Restore {
        fn drop(&mut self) {
            write(self.0);
        }
    }

    #[cfg(target_arch = "aarch64")]
    pub const CONTROL_MASK: u64 = u64::MAX;

    #[cfg(target_arch = "x86_64")]
    pub const CONTROL_MASK: u64 = !0x3f;

    #[cfg(target_arch = "aarch64")]
    pub fn read() -> u64 {
        let fpcr: u64;
        unsafe { asm!("mrs {fpcr}, fpcr", fpcr = out(reg) fpcr, options(nostack, preserves_flags)) }
        fpcr
    }

    #[cfg(target_arch = "aarch64")]
    pub fn write(fpcr: u64) {
        unsafe { asm!("msr fpcr, {fpcr}", fpcr = in(reg) fpcr, options(nostack, preserves_flags)) }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn read() -> u64 {
        let mut mxcsr = 0_u32;
        unsafe {
            asm!("stmxcsr [{ptr}]", ptr = in(reg) &mut mxcsr, options(nostack, preserves_flags))
        }
        mxcsr.into()
    }

    #[cfg(target_arch = "x86_64")]
    pub fn write(mxcsr: u64) {
        let mxcsr = mxcsr as u32;
        unsafe { asm!("ldmxcsr [{ptr}]", ptr = in(reg) &mxcsr, options(nostack, preserves_flags)) }
    }

    pub fn test_settings(current: u64) -> [u64; 3] {
        #[cfg(target_arch = "aarch64")]
        let (rounding_mode, round_up, flush_to_zero) = (0b11 << 22, 0b01 << 22, 1 << 24);
        #[cfg(target_arch = "x86_64")]
        let (rounding_mode, round_up, flush_to_zero) = (0b11 << 13, 0b10 << 13, 1 << 15);
        let default = current & !(rounding_mode | flush_to_zero);
        [
            default | round_up,
            default | flush_to_zero,
            default | round_up | flush_to_zero,
        ]
    }
}
