(defpackage #:zk-ffi
  (:use #:cl)
  (:export #:open-library #:find-symbol-pointer
           #:bind-function #:bind-pointer #:bind-variadic-function #:bind-variadic-pointer
           #:make-callback #:allocate #:free #:with-foreign-memory
           #:inc-pointer #:pointer-p #:pointer-address #:make-pointer
           #:type-size #:type-alignment #:field-offset #:mem-ref #:mem-set
           #:read-octets #:write-octets
           #:read-c-string #:write-c-string #:utf8-prefix #:write-utf8))
(in-package #:zk-ffi)

(defun (setf mem-ref) (value pointer type &optional (byte-offset 0))
  (mem-set pointer type byte-offset value))

(defmacro with-foreign-memory ((name byte-count) &body body)
  "Bind NAME to zeroed native storage and release it on leaving BODY."
  `(let ((,name (allocate ,byte-count)))
     (unwind-protect (progn ,@body) (free ,name))))

(defun read-octets (pointer vector &key (start 0) end (offset 0))
  "Copy from POINTER at byte OFFSET into VECTOR between START and END.
END defaults to LENGTH. Return the number of bytes copied."
  (%read-octets pointer vector start end offset))

(defun write-octets (pointer vector &key (start 0) end (offset 0))
  "Copy VECTOR between START and END to POINTER at byte OFFSET.
END defaults to LENGTH. Return the number of bytes copied."
  (%write-octets pointer vector start end offset))

(defun make-callback (function result-type &rest parameter-types)
  "Create a C function pointer for a synchronous callback to this runtime.
Call it on the owning thread during a foreign call or an explicit host scope.
Retain the pointer as long as native code can invoke it."
  (let ((target (coerce function 'function)))
    (macrolet ((capture-error (parameters call)
                 `(lambda ,parameters
                    (handler-case (values ,call nil)
                      (error (condition) (values nil condition))))))
      (apply #'%make-callback
             (case (length parameter-types)
               (0 (capture-error () (funcall target)))
               (1 (capture-error (a) (funcall target a)))
               (2 (capture-error (a b) (funcall target a b)))
               (3 (capture-error (a b c) (funcall target a b c)))
               (4 (capture-error (a b c d) (funcall target a b c d)))
               (otherwise (capture-error (&rest arguments) (apply target arguments))))
             result-type parameter-types))))

(provide :zk-ffi)
(in-package #:cl-user)
