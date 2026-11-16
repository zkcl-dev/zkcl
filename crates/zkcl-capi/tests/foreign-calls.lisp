;; Run as source or a compiled file, in a standalone or an embedded runtime.
(defvar *foreign-library* (zk-ffi:open-library (zk:getenv "ZKCL_TEST_LIBRARY")))
(defvar *uint64-identity* (zk-ffi:bind-function *foreign-library* "identity_uint64" :uint64 :uint64))
(defvar *int8-identity* (zk-ffi:bind-function *foreign-library* "identity_int8" :int8 :int8))
(defvar *float-identity* (zk-ffi:bind-function *foreign-library* "identity_float" :float :float))
(defvar *sum-mixed-arguments* (zk-ffi:bind-function *foreign-library* "sum_mixed_arguments" :double :double :float :int16))
(defvar *utf8-byte-length* (zk-ffi:bind-function *foreign-library* "utf8_byte_length" :size :cstring))
(defvar *echo* (zk-ffi:bind-function *foreign-library* "echo_text" :cstring :cstring))
(defvar *invalid-utf8* (zk-ffi:bind-function *foreign-library* "invalid_utf8" :cstring))
(defvar *counter-pointer* (funcall (zk-ffi:bind-function *foreign-library* "counter_address" :pointer)))
(defvar *increment-counter* (zk-ffi:bind-function *foreign-library* "increment_counter" :void :pointer))
(defvar *read-counter* (zk-ffi:bind-function *foreign-library* "read_counter" :int :pointer))
(defvar *null-pointer* (zk-ffi:bind-function *foreign-library* "null_pointer" :pointer))

(defvar *variable* (zk-ffi:find-symbol-pointer *foreign-library* "zkcl_global_number"))
(defvar *promoted* (zk-ffi:bind-variadic-function *foreign-library* "sum_promoted" 1
                                                  :double :int :int8 :float))
(defvar *promoted-by-pointer*
  (zk-ffi:bind-variadic-pointer
   (zk-ffi:find-symbol-pointer *foreign-library* "sum_promoted") 1 :double :int :int :double))
(defvar *long-long-identity*
  (zk-ffi:bind-pointer (zk-ffi:find-symbol-pointer *foreign-library* "identity_uint64")
                       :unsigned-long-long :unsigned-long-long))
(assert (null (zk-ffi:find-symbol-pointer *foreign-library* "absent_zkcl_global")))
(assert (nth-value 1 (ignore-errors (zk-ffi:bind-pointer nil :int))))
(assert (nth-value 1 (ignore-errors
                       (zk-ffi:bind-variadic-function *foreign-library* "sum_promoted" 0 :double :int))))
(assert (nth-value 1 (ignore-errors
                       (zk-ffi:bind-variadic-function *foreign-library* "sum_promoted" 2 :double :int))))

(defvar *retain-int-callback*
  (zk-ffi:bind-function *foreign-library* "retain_int_callback" :void :pointer))
(defvar *call-saved-int-callback*
  (zk-ffi:bind-function *foreign-library* "call_saved_int_callback" :int :int :int))
(defvar *get-negative-int*
  (zk-ffi:bind-function *foreign-library* "get_negative_int" :int))
(defvar *sum-three-ints*
  (zk-ffi:bind-function *foreign-library* "sum_three_ints" :int :int :int :int))
(defvar *multiply-doubles*
  (zk-ffi:bind-function *foreign-library* "multiply_doubles" :double :double :double))

(defvar *replace-counter* (zk-ffi:bind-function *foreign-library* "replace_counter" :void :int))

(defun check-fixed-scalar-calls ()
  (assert (= (funcall *get-negative-int*) -73))
  (let ((previous (funcall *read-counter* *counter-pointer*)))
    (assert (null (multiple-value-list (funcall *replace-counter* -7))))
    (assert (= (funcall *read-counter* *counter-pointer*) -7))
    (funcall *replace-counter* previous))
  (assert (= (funcall *sum-three-ints* 10 20 12) 42))
  (assert (eql (funcall *multiply-doubles* 2d0 3.25d0) 6.5d0))
  (assert (handler-case (progn (funcall *get-negative-int* 0) nil)
            (program-error () t)))
  ;; Numeric-only arguments do not preclude a C callback. The C function
  ;; invokes a pointer installed by an earlier, separate FFI call.
  (let* ((expected (make-condition 'simple-error :format-control "native callback failure"))
         (proc (zk-ffi:make-callback (lambda (a b)
                                      (if (< a 0) (error expected) (+ a b)))
                                    :int :int :int)))
    (unwind-protect
        (progn
          (funcall *retain-int-callback* proc)
          (assert (= (funcall *call-saved-int-callback* 12 30) 42))
          (assert (eq expected (handler-case
                                   (progn (funcall *call-saved-int-callback* -1 2) nil)
                                 (simple-error (condition) condition))))
          (assert (= (funcall *call-saved-int-callback* 40 2) 42)))
      (funcall *retain-int-callback* nil))))

(defun test-foreign-calls ()
  (assert (member :zk-ffi *features*))
  (check-fixed-scalar-calls)
  (assert (functionp *promoted*))
  (assert (eql (funcall *promoted* 8 -3 1.25f0) 6.25d0))
  (assert (eql (funcall *promoted-by-pointer* 8 -3 1.25d0) 6.25d0))
  (assert (= (funcall *long-long-identity* #xffffffffffffffff) #xffffffffffffffff))
  (assert (= (zk-ffi:mem-ref *variable* :int) 37))
  (setf (zk-ffi:mem-ref *variable* :int) 41)
  (assert (= (zk-ffi:mem-ref *variable* :int) 41))
  (setf (zk-ffi:mem-ref *variable* :int) 37)
  (assert (= (funcall *uint64-identity* #xffffffffffffffff) #xffffffffffffffff))
  (assert (= (funcall *int8-identity* -128) -128))
  (assert (eql (funcall *float-identity* 1.5f0) 1.5f0))
  (assert (eql (funcall *sum-mixed-arguments* 1.5d0 2.25f0 -1) 2.75d0))
  (assert (= (funcall *utf8-byte-length* "你好") 6))
  (assert (= (funcall *utf8-byte-length* nil) 0))
  (assert (string= (funcall *echo* "你好") "你好"))
  (assert (null (funcall *echo* nil)))
  (assert (null (funcall *null-pointer*)))
  (assert (= (funcall *read-counter* nil) -1))
  (let ((old (funcall *read-counter* *counter-pointer*)))
    (assert (null (multiple-value-list (funcall *increment-counter* *counter-pointer*))))
    (assert (= (funcall *read-counter* *counter-pointer*) (1+ old))))
  (assert (handler-case (progn (funcall *int8-identity* 128) nil)
            (type-error (c) (and (eql (type-error-datum c) 128)
                                 (equal (type-error-expected-type c) '(signed-byte 8))))))
  (assert (nth-value 1 (ignore-errors (funcall *uint64-identity* -1))))
  (assert (nth-value 1 (ignore-errors (funcall *sum-mixed-arguments* 3))))
  (assert (nth-value 1 (ignore-errors (funcall *float-identity* 1d0))))
  (assert (nth-value 1 (ignore-errors (funcall *utf8-byte-length* (string (code-char 0))))))
  (assert (nth-value 1 (ignore-errors (funcall *invalid-utf8*))))
  (assert (nth-value 1 (ignore-errors (funcall *read-counter* 0))))
  t)

(assert (nth-value 1 (ignore-errors (zk-ffi:bind-function *foreign-library* "unknown_zkcl_function" :void))))
(assert (nth-value 1 (ignore-errors (zk-ffi:bind-function *foreign-library* "read_counter" :int :void))))
(assert (nth-value 1 (ignore-errors (zk-ffi:bind-function *foreign-library* "read_counter" :unknown))))
(setf *foreign-library* nil)
(test-foreign-calls)
