#![cfg(feature = "ffi")]

use zkcl::Runtime;

fn runtime() -> Runtime {
    // SAFETY: tests keep the backing storage alive for all external pointer
    // accesses. Failure cases use checked pointers from ZK-FFI:ALLOCATE.
    unsafe { Runtime::builder().allow_foreign_calls() }
        .start()
        .unwrap()
}

#[test]
fn native_scalars_round_trip_at_an_unaligned_byte_offset() {
    let rt = runtime();
    rt.load_str(
        "(defparameter *storage* (zk-ffi:allocate 9))
         (dolist (test '((:int8 -128) (:uint8 255)
                         (:int16 -32768) (:uint16 65535)
                         (:int32 -2147483648) (:uint32 4294967295)
                         (:int64 -9223372036854775808) (:uint64 18446744073709551615)
                         (:float 1.25f0) (:double -123.5d0)))
           (destructuring-bind (type number) test
             (assert (eql (setf (zk-ffi:mem-ref *storage* type 1) number) number))
             (assert (eql (zk-ffi:mem-ref *storage* type 1) number))))
         (assert (= (zk-ffi:type-size :uint64) 8))
         (assert (zerop (mod (zk-ffi:pointer-address *storage*) (zk-ffi:type-alignment :double))))
         (assert (= (zk-ffi:type-size :pointer) (zk-ffi:type-size :size)))
         (let ((sentinel (1- (ash 1 (* 8 (zk-ffi:type-size :pointer))))))
           (assert (= sentinel (zk-ffi:pointer-address (zk-ffi:make-pointer sentinel)))))
         (assert (zk-ffi:pointer-p (zk-ffi:make-pointer 0)))
         (assert (not (zk-ffi:pointer-p 0)))
         (assert (null (zk-ffi:make-pointer 0)))
         (assert (zerop (zk-ffi:pointer-address nil)))
         (let ((raw (zk-ffi:make-pointer (zk-ffi:pointer-address *storage*))))
           (assert (eql (zk-ffi:mem-ref raw :double 1) -123.5d0)))
         (zk-ffi:free *storage*)",
    )
    .unwrap();
}

#[test]
fn octet_transfers_respect_displacement_fill_pointers_and_both_bounds() {
    let rt = runtime();
    rt.load_str(
        r#"
        (let* ((backing (make-array 7 :element-type '(unsigned-byte 8)
                                     :initial-contents '(99 0 128 255 42 98 97)))
               (view (make-array 4 :element-type '(unsigned-byte 8) :fill-pointer 3
                                  :displaced-to backing :displaced-index-offset 1))
               (result (make-array 6 :element-type '(unsigned-byte 8) :initial-element 7)))
          (zk-ffi:with-foreign-memory (buffer 6)
            (assert (= 3 (zk-ffi:write-octets buffer view :offset 1)))
            (assert (= 4 (zk-ffi:write-octets buffer view :end 4 :offset 1)))
            (assert (= 4 (zk-ffi:read-octets buffer result :start 1 :end 5 :offset 1)))
            (assert (equalp result #(7 0 128 255 42 7)))
            (assert (= 1 (zk-ffi:read-octets buffer view :start 3 :end 4 :offset 2)))
            (assert (= 128 (aref backing 4)))
            (assert (= 3 (fill-pointer view)))
            (assert (handler-case (progn (zk-ffi:read-octets buffer result :offset 1) nil)
                      (error () t)))
            (assert (equalp result #(7 0 128 255 42 7)))
            (assert (handler-case (progn (zk-ffi:write-octets buffer view :end 5) nil)
                      (error () t)))
            (assert (= 0 (zk-ffi:mem-ref buffer :uint8)))
            (assert (handler-case (progn (zk-ffi:read-octets buffer #(0)) nil)
                      (error () t)))
            (assert (= 0 (zk-ffi:read-octets nil result :start 6)))
            (assert (= 0 (zk-ffi:write-octets (zk-ffi:inc-pointer buffer 6) view :end 0)))
            (assert (handler-case (progn (zk-ffi:read-octets nil view :end 0 :offset 1) nil)
                      (error () t)))
            (assert (handler-case (progn (zk-ffi:read-octets nil view) nil) (error () t)))))
        (let ((buffer (zk-ffi:allocate 1))
              (bytes (make-array 1 :element-type '(unsigned-byte 8))))
          (zk-ffi:free buffer)
          (assert (handler-case (progn (zk-ffi:write-octets buffer bytes) nil) (error () t))))
        "#,
    )
    .unwrap();
}

#[test]
fn an_allocation_remains_accessible_through_its_aliases_until_freed() {
    let rt = runtime();
    rt.load_str(
        "(defparameter *tail*
             (let ((base (zk-ffi:allocate 8)))
               (setf (zk-ffi:mem-ref base :uint16 6) 65534)
               (zk-ffi:inc-pointer base 6)))",
    )
    .unwrap();
    rt.collect();
    rt.load_str(
        "(assert (= (zk-ffi:mem-ref *tail* :uint16) 65534))
         (defparameter *end* (zk-ffi:inc-pointer *tail* 2))
         (assert (= (zk-ffi:mem-ref *end* :uint16 -2) 65534))
         (assert (zerop (zk-ffi:mem-ref *tail* :uint8 -6)))
         (defparameter *self* (zk-ffi:allocate (zk-ffi:type-size :pointer)))
         (setf (zk-ffi:mem-ref *self* :pointer)
               (zk-ffi:inc-pointer *self* (zk-ffi:type-size :pointer)))
         (defparameter *one-past* (zk-ffi:mem-ref *self* :pointer))",
    )
    .unwrap();
    for op in [
        "(zk-ffi:mem-ref *end* :uint8)",
        "(zk-ffi:mem-ref *one-past* :uint8)",
        "(zk-ffi:mem-ref *tail* :uint32)",
        "(zk-ffi:inc-pointer *end* 1)",
        "(zk-ffi:mem-set *tail* :uint8 -7 23)",
        "(zk-ffi:free *tail*)",
        "(zk-ffi:mem-set *tail* :uint16 0 65536)",
        "(zk-ffi:free (zk-ffi:make-pointer (zk-ffi:pointer-address *tail*)))",
        "(zk-ffi:mem-ref nil :pointer)",
        "(zk-ffi:mem-ref (zk-ffi:allocate 0) :uint8)",
        "(zk-ffi:allocate -1)",
    ] {
        assert!(rt.eval(op).is_err(), "{op}");
    }
    rt.load_str(
        "(assert (= (zk-ffi:mem-ref *tail* :uint16) 65534))
         (defparameter *base* (zk-ffi:inc-pointer *tail* -6))
         (zk-ffi:free *base*)
         (zk-ffi:free *self*)",
    )
    .unwrap();
    for op in [
        "(zk-ffi:pointer-address *tail*)",
        "(zk-ffi:pointer-address *one-past*)",
        "(zk-ffi:mem-ref *tail* :uint8)",
        "(zk-ffi:mem-set *end* :uint8 -1 0)",
        "(zk-ffi:free *base*)",
    ] {
        let error = rt.eval(op).err().expect(op);
        assert!(error.to_string().contains("freed"), "{op}: {error}");
    }
}

#[test]
fn c_string_slots_retain_the_original_utf8_buffer() {
    let rt = runtime();
    rt.load_str(
        "(defparameter *buf* (zk-ffi:allocate 6))
         (defparameter *slots* (zk-ffi:allocate (* 2 (zk-ffi:type-size :pointer))))
         (defparameter *second* (zk-ffi:inc-pointer *slots* (zk-ffi:type-size :pointer)))
         (assert (null (zk-ffi:mem-ref *slots* :cstring)))
         (zk-ffi:write-c-string *buf* \"绿é\")
         (setf (zk-ffi:mem-ref *second* :cstring) *buf*)
         (assert (string= (zk-ffi:mem-ref *second* :cstring) \"绿é\"))
         (defparameter *borrowed* (zk-ffi:mem-ref *second* :pointer))
         (zk-ffi:free *slots*)
         (assert (string= (zk-ffi:read-c-string *borrowed* 3) \"绿\"))
         (assert (eql (char (zk-ffi:read-c-string *buf* 6) 2) (code-char 0)))
         (setf (zk-ffi:mem-ref *buf* :uint8 5) 65)",
    )
    .unwrap();
    for op in [
        "(zk-ffi:read-c-string *buf*)",
        "(zk-ffi:read-c-string *buf* 7)",
        "(zk-ffi:read-c-string *buf* 1)",
        "(zk-ffi:write-c-string *buf* \"绿é!\")",
        "(zk-ffi:write-c-string *buf* (string (code-char 0)))",
    ] {
        assert!(rt.eval(op).is_err(), "{op}");
    }
    rt.load_str(
        "(assert (= (zk-ffi:mem-ref *buf* :uint8 5) 65))
         (zk-ffi:free *buf*)",
    )
    .unwrap();
}

#[test]
fn returned_interior_pointers_refer_to_the_original_bounded_allocation() {
    extern "C" fn advance(p: *mut u8, delta: usize) -> *mut u8 {
        p.wrapping_add(delta)
    }

    let rt = runtime();
    let code = (advance as *const ()).expose_provenance();
    rt.load_str(&format!(
        "(defparameter *advance*
             (zk-ffi:bind-pointer (zk-ffi:make-pointer {code}) :pointer :pointer :size))
         (defparameter *inside*
             (let ((base (zk-ffi:allocate 7)))
               (setf (zk-ffi:mem-ref base :uint8 6) 89)
               (funcall *advance* base 6)))"
    ))
    .unwrap();
    rt.collect();
    rt.load_str(
        "(assert (= (zk-ffi:mem-ref *inside* :uint8) 89))
         (defparameter *after* (funcall *advance* *inside* 1))",
    )
    .unwrap();
    assert!(rt.eval("(zk-ffi:mem-ref *after* :uint8)").is_err());
    rt.load_str("(zk-ffi:free (zk-ffi:inc-pointer *inside* -6))")
        .unwrap();
    let error = rt.eval("(zk-ffi:pointer-address *after*)").err().unwrap();
    assert!(error.to_string().contains("freed"), "{error}");
}

#[test]
fn callback_freeing_an_input_invalidates_its_return_alias() {
    extern "C" fn release_then_return(p: *mut u8, release: extern "C" fn()) -> *mut u8 {
        let interior = p.wrapping_add(2);
        release();
        interior
    }

    let rt = runtime();
    let code = (release_then_return as *const ()).expose_provenance();
    let error = rt
        .eval(&format!(
            "(let ((p (zk-ffi:allocate 8))
                   (invoke (zk-ffi:bind-pointer (zk-ffi:make-pointer {code})
                                               :pointer :pointer :pointer)))
               (funcall invoke p (zk-ffi:make-callback (lambda () (zk-ffi:free p)) :void)))"
        ))
        .err()
        .expect("returning a pointer to storage freed by the callback must fail");
    assert!(error.to_string().contains("freed"), "{error}");
}

#[test]
fn strchr_returns_the_original_utf8_buffer_as_an_owned_interior_pointer() {
    use std::ffi::{c_char, c_int};

    unsafe extern "C" {
        fn strchr(string: *const c_char, character: c_int) -> *mut c_char;
    }

    let rt = runtime();
    let code = (strchr as *const ()).expose_provenance();
    rt.load_str(&format!(
        "(defparameter *hit*
             (funcall (zk-ffi:bind-pointer (zk-ffi:make-pointer {code}) :pointer :cstring :int)
                      \"绿/42\" (char-code #\\/)))"
    ))
    .unwrap();
    rt.collect();
    rt.load_str(
        "(assert (string= (zk-ffi:read-c-string *hit*) \"/42\"))
         (defparameter *text* (zk-ffi:inc-pointer *hit* -3))
         (assert (string= (zk-ffi:read-c-string *text*) \"绿/42\"))",
    )
    .unwrap();
    assert!(rt.eval("(zk-ffi:mem-ref *hit* :uint8 4)").is_err());
    rt.load_str("(zk-ffi:free *text*)").unwrap();
    let error = rt.eval("(zk-ffi:read-c-string *hit*)").err().unwrap();
    assert!(error.to_string().contains("freed"), "{error}");
}

#[test]
fn pointer_field_return_recovers_the_original_allocation() {
    #[repr(C)]
    struct PointerField {
        value: *mut u8,
    }
    extern "C" fn field(value: *mut u8) -> PointerField {
        PointerField { value }
    }

    let rt = runtime();
    let code = (field as *const ()).expose_provenance();
    rt.load_str(&format!(
        "(defparameter *base* (zk-ffi:allocate 4))
         (defparameter *record*
             (funcall (zk-ffi:bind-pointer (zk-ffi:make-pointer {code}) '(:struct :pointer) :pointer)
                      (zk-ffi:inc-pointer *base* 2)))"
    ))
    .unwrap();
    rt.collect();
    rt.load_str(
        "(defparameter *field* (zk-ffi:mem-ref *record* :pointer))
         (setf (zk-ffi:mem-ref *field* :uint16) 1002)
         (assert (= (zk-ffi:mem-ref *base* :uint16 2) 1002))
         (zk-ffi:free *base*)",
    )
    .unwrap();
    for op in [
        "(zk-ffi:pointer-address *field*)",
        "(zk-ffi:mem-ref *record* :pointer)",
    ] {
        let error = rt.eval(op).err().expect(op);
        assert!(error.to_string().contains("freed"), "{op}: {error}");
    }
}

#[test]
fn native_return_prefers_the_most_specific_live_pointer_candidate() {
    extern "C" fn second(_: *mut u8, value: *mut u8) -> *mut u8 {
        value
    }
    let rt = runtime();
    let code = (second as *const ()).expose_provenance();
    rt.load_str(&format!(
        "(defparameter *second-ptr*
             (zk-ffi:bind-pointer (zk-ffi:make-pointer {code}) :pointer :pointer :pointer))
         (defparameter *base* (zk-ffi:allocate 8))
         (defparameter *raw* (zk-ffi:make-pointer (zk-ffi:pointer-address *base*)))
         (defparameter *end-address* (+ 8 (zk-ffi:pointer-address *base*)))
         (defparameter *exact* (funcall *second-ptr* *raw* *base*))
         (defparameter *outside*
             (funcall *second-ptr* *base* (zk-ffi:make-pointer *end-address*)))
         (zk-ffi:free *base*)
         (assert (= (zk-ffi:pointer-address *outside*) *end-address*))
         (let ((empty (zk-ffi:allocate 0)))
           (zk-ffi:free (funcall *second-ptr*
                                 (zk-ffi:make-pointer (zk-ffi:pointer-address empty))
                                 empty)))"
    ))
    .unwrap();
    let error = rt.eval("(zk-ffi:pointer-address *exact*)").err().unwrap();
    assert!(error.to_string().contains("freed"), "{error}");
}

#[test]
fn byte_limits_never_split_a_utf8_character() {
    let rt = runtime();
    rt.load_str(
        r#"
       (let ((s "雪AλZ"))
         (assert (equal '(7 4) (multiple-value-list (zk-ffi:utf8-prefix s 0 nil nil))))
         (assert (equal '(0 1) (multiple-value-list (zk-ffi:utf8-prefix s 1 4 0))))
         (assert (equal '(3 1) (multiple-value-list (zk-ffi:utf8-prefix s 0 4 3))))
         (assert (equal '(1 2) (multiple-value-list (zk-ffi:utf8-prefix s 1 4 2))))
         (assert (equal '(0 4) (multiple-value-list (zk-ffi:utf8-prefix s 4 4 9))))
         (assert (nth-value 1 (ignore-errors (zk-ffi:utf8-prefix s 2 1 nil))))
         (assert (nth-value 1 (ignore-errors (zk-ffi:utf8-prefix s 0 5 nil)))))
       (zk-ffi:with-foreign-memory (p 6)
         (dotimes (i 6) (setf (zk-ffi:mem-ref p :uint8 i) 90))
         (zk-ffi:write-c-string p "abc" 1)
         (assert (= (zk-ffi:mem-ref p :uint8) 0))
         (assert (= (zk-ffi:mem-ref p :uint8 1) 90))
         (zk-ffi:write-c-string p "雪λA" 5)
         (assert (string= "雪" (zk-ffi:read-c-string p)))
         (assert (= 0 (zk-ffi:mem-ref p :uint8 3)))
         (assert (= 90 (zk-ffi:mem-ref p :uint8 4)))
         (assert (nth-value 1 (ignore-errors (zk-ffi:write-c-string p "x" 0))))
         (assert (nth-value 1 (ignore-errors (zk-ffi:write-utf8 p "雪λ" 0 nil 2))))
         (assert (= 90 (zk-ffi:mem-ref p :uint8 4)))
         (assert (= 3 (zk-ffi:write-utf8 p "雪λA" 1 3 1)))
         (assert (string= "λA" (zk-ffi:read-c-string (zk-ffi:inc-pointer p 1) 3)))
         (let ((s (coerce (list #\a (code-char 0) #\b) 'string)))
           (assert (= 3 (zk-ffi:write-utf8 p s 0 nil)))
           (assert (string= s (zk-ffi:read-c-string p 3)))))
    "#,
    )
    .unwrap();
}
