;;;; Lower CFFI's fixed-signature structure-by-value calls to the native zkcl ABI.

(in-package #:zkcl-cffi-port)

(defun array-abi-type (element dimensions)
  (unless (every (lambda (n) (and (integerp n) (plusp n))) dimensions)
    (error "CFFI inline array dimensions must be positive integers: ~S" dimensions))
  ;; libffi represents inline arrays as structures of repeated elements.
  (cons :struct (make-list (reduce #'* dimensions) :initial-element element)))

(defun aggregate-field-type (type)
  ;; Bare struct names denote inline objects inside another structure. In a C
  ;; function signature CFFI canonicalizes the same bare name to :POINTER.
  (typecase type
    (cffi::foreign-union-type
     (error "zkcl cannot pass the CFFI union ~S by value." (cffi::unparse-type type)))
    (cffi::foreign-struct-type
     (let* ((slots (cffi::slots-in-order type))
            (fields
              (loop for slot in slots
                    for field = (aggregate-field-type (cffi::parse-type (cffi::slot-type slot)))
                    for count = (if (typep slot 'cffi::aggregate-struct-slot)
                                    (cffi::slot-count slot)
                                    1)
                    collect (if (eql count 1) field (array-abi-type field (list count)))))
            (descriptor (cons :struct fields)))
       ;; Packed fields, overlaying slots, and arbitrary tail padding can change
       ;; C's register classification even when the fields individually fit.
       (unless (and fields
                    (= (cffi:foreign-type-size type) (zk-ffi:type-size descriptor))
                    (= (cffi:foreign-type-alignment type) (zk-ffi:type-alignment descriptor))
                    (loop for slot in slots for i from 0
                          always (= (cffi::slot-offset slot) (zk-ffi:field-offset descriptor i))))
         (error "CFFI structure ~S does not have its native C layout." (cffi::unparse-type type)))
       descriptor))
    (cffi::foreign-array-type
     (array-abi-type (aggregate-field-type (cffi::parse-type (cffi::element-type type)))
                     (cffi::dimensions type)))
    (cffi::foreign-type-alias (aggregate-field-type (cffi::actual-type type)))
    (t (cffi-sys::native-type (cffi::canonicalize type)))))

(defun call-abi-type (canonical)
  (if (consp canonical)
      (aggregate-field-type (cffi::parse-type canonical))
      (cffi-sys::native-type canonical)))

(defun expand-aggregate-call (target arguments symbols types result canonical-types pointerp)
  ;; The CFFI hook supplies neither the library nor the calling convention.
  ;; Named functions use the default search. An explicit function pointer
  ;; selects a particular library. Both forms use the system C ABI.
  (let* ((parameters (mapcar #'call-abi-type canonical-types))
         (abi-result (call-abi-type (cffi::canonicalize-foreign-type result)))
         (call (cffi-sys::expand-call target
                 (append (mapcan #'list parameters symbols) (list abi-result))
                 :cdecl :library (unless pointerp :default))))
    (labels ((translate (arguments symbols types parameters)
               (if arguments
                   (funcall (if (consp (car parameters))
                                #'cffi::expand-to-foreign-dyn-indirect
                                #'cffi::expand-to-foreign-dyn)
                            (car arguments) (car symbols)
                            (list (translate (cdr arguments) (cdr symbols)
                                             (cdr types) (cdr parameters)))
                            (cffi::parse-type (car types)))
                   (cffi::expand-from-foreign call (cffi::parse-type result)))))
      (translate arguments symbols types parameters))))

(setf cffi::*foreign-structures-by-value* #'expand-aggregate-call)
