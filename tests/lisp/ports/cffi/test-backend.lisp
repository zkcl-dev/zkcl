;;;; Run after zkcl-cffi-port:register with Alexandria and Babel in ASDF.
(asdf:load-system "cffi")

(cffi:defcallback zkcl-test-add :int ((a :int) (b :int)) (+ a b))

(let ((callback (cffi:callback zkcl-test-add)))
  (assert (cffi:pointerp callback))
  (assert (= 42 (cffi:foreign-funcall-pointer callback () :int 15 :int 27 :int))))

#+zkcl
(progn
  (cffi:defctype zkcl-callback-int :int)
  (defun install-zkcl-test-offset (offset)
    (cffi:defcallback zkcl-test-offset zkcl-callback-int ((x :int)) (+ x offset)))
  (install-zkcl-test-offset 1)
  (let ((original (cffi:pointer-address (cffi:callback zkcl-test-offset))))
    (flet ((invoke () (cffi:foreign-funcall-pointer (cffi:make-pointer original) ()
                        :int 5 :int)))
      (assert (= 6 (invoke)))
      (install-zkcl-test-offset 37)
      (assert (= original (cffi:pointer-address (cffi:get-callback 'zkcl-test-offset))))
      (assert (= 42 (invoke)))
      (assert (handler-case
                  (progn (cffi:defcallback zkcl-test-offset :double ((x :int)) (float x 1d0))
                         nil)
                (error () t)))
      (assert (handler-case
                  (progn (cffi:defcallback zkcl-test-offset :int ((x :short)) x) nil)
                (error () t)))
      (assert (= 42 (invoke)))
      (assert (= 3 (cffi:foreign-funcall-pointer (cffi:callback zkcl-test-add) ()
                     :int 1 :int 2 :int))))))

(let ((null (cffi:null-pointer)))
  (assert (and null (cffi:pointerp null) (cffi:null-pointer-p null)))
  (assert (not (cffi:pointerp nil)))
  (cffi:with-foreign-object (slot :pointer)
    (setf (cffi:mem-ref slot :pointer) null)
    (assert (cffi:null-pointer-p (cffi:mem-ref slot :pointer)))))

(let ((address (let ((p (cffi:foreign-alloc :uint32 :initial-element #x12345678)))
                 (cffi:pointer-address p))))
  (let ((p (cffi:make-pointer address)))
    (unwind-protect (assert (= #x12345678 (cffi:mem-ref p :uint32)))
      (cffi:foreign-free p)))
  #+zkcl (assert (null (gethash address cffi-sys::*allocations*))))

(let ((v (cffi:make-shareable-byte-vector 3)))
  (setf (aref v 1) 45)
  (catch 'done
    (cffi:with-pointer-to-vector-data (p v)
      (assert (= (cffi:mem-ref p :uint8 1) 45))
      (setf (cffi:mem-ref p :uint8 2) 91)
      (throw 'done nil)))
  (assert (= (aref v 2) 91)))

#+unix
(let ((snprintf (cffi:foreign-symbol-pointer "snprintf")))
  (assert snprintf)
  (assert (string= "21 2.5"
                   (cffi:with-foreign-pointer-as-string (p 40)
                     (assert (= 6 (cffi:foreign-funcall-pointer-varargs
                                   snprintf () (:pointer p :size 40 :string "%d %.1f")
                                   :int 21 :double 2.5d0 :int)))))))

#+(and zkcl unix)
(progn
  (defun zkcl-test-abs (value)
    (cffi:foreign-funcall "abs" :int value :int))
  (assert (= 1 (zkcl-test-abs -1)))
  (let* ((signature '("abs" :default nil :int :int))
         (cell (gethash signature cffi-sys::*named-bindings*))
         (binding (car cell)))
    (assert (functionp binding))
    (dotimes (i 100)
      (assert (= i (zkcl-test-abs (- i)))))
    (assert (eq binding (car (gethash signature cffi-sys::*named-bindings*))))
    (cffi-sys::invalidate-named-bindings)
    (assert (null (car cell)))
    (assert (= 42 (zkcl-test-abs -42)))
    (assert (not (eq binding (car (gethash signature cffi-sys::*named-bindings*)))))))

#+zkcl
(cffi:defcstruct (zkcl-test-record :class zkcl-record-type)
  (grid (:array :short 2 2))
  (weights :double :count 2))

#+zkcl
(defmethod cffi:translate-into-foreign-memory (value (type zkcl-record-type) pointer)
  (let ((weights (cffi:foreign-slot-pointer pointer '(:struct zkcl-test-record) 'weights)))
    (setf (cffi:mem-aref pointer :short 3) (first value)
          (cffi:mem-aref weights :double 1) (second value))))

#+zkcl
(defmethod cffi:translate-from-foreign (pointer (type zkcl-record-type))
  (let ((weights (cffi:foreign-slot-pointer pointer '(:struct zkcl-test-record) 'weights)))
    (list (cffi:mem-aref pointer :short 3) (cffi:mem-aref weights :double 1))))

#+zkcl
(let* ((abi '(:struct (:struct :short :short :short :short) (:struct :double :double)))
       (last-weight (+ (zk-ffi:field-offset abi 1) (zk-ffi:type-size :double)))
       (callback (zk-ffi:make-callback
                  (lambda (p) (incf (zk-ffi:mem-ref p :double last-weight) 1.5d0) p)
                  abi abi)))
  (assert (equal '(42 4d0)
                 (cffi:foreign-funcall-pointer callback ()
                   (:struct zkcl-test-record) '(42 2.5d0) (:struct zkcl-test-record)))))

(format t "~&CFFI backend checks passed.~%")
