;;;; zkcl implementation of the upstream CFFI-SYS interface.

(in-package #:cffi-sys)

(pushnew 'no-stdcall *features*)

(defun canonicalize-symbol-name-case (name) (string-upcase name))
(defun native-namestring (pathname) (namestring pathname))

(defstruct (null-foreign-pointer (:constructor %make-null-pointer) (:copier nil)))
(defvar *null-pointer* (%make-null-pointer))

(defun null-pointer () *null-pointer*)
(defun pointerp (value)
  (and value (or (eq value *null-pointer*) (zk-ffi:pointer-p value))))
(deftype foreign-pointer () '(satisfies pointerp))

;; CFFI distinguishes a C null pointer from NIL. ZK-FFI represents it as NIL.
(defun native-pointer (pointer)
  (cond ((eq pointer *null-pointer*) nil)
        ((null pointer) (error 'type-error :datum pointer :expected-type 'foreign-pointer))
        (t pointer)))

(defun pointer-address (pointer) (zk-ffi:pointer-address (native-pointer pointer)))
(defun make-pointer (address) (or (zk-ffi:make-pointer address) *null-pointer*))
(defun null-pointer-p (pointer) (zerop (pointer-address pointer)))
(defun pointer-eq (left right) (= (pointer-address left) (pointer-address right)))
(defun inc-pointer (pointer offset)
  (or (zk-ffi:inc-pointer (native-pointer pointer) offset) *null-pointer*))

;; FOREIGN-ALLOC has explicit lifetime. C may keep just the numeric address,
;; so retain the owning zkcl pointer until FOREIGN-FREE receives that address.
(defvar *allocations* (make-hash-table :test #'eql))

(defun %foreign-alloc (size)
  (let ((base (zk-ffi:allocate size)))
    (setf (gethash (pointer-address base) *allocations*) base)))

(defun foreign-free (pointer)
  (unless (null-pointer-p pointer)
    (let* ((address (pointer-address pointer))
           (base (gethash address *allocations*)))
      (unless base (error "No CFFI allocation starts at address ~X." address))
      (zk-ffi:free base)
      (remhash address *allocations*)))
  nil)

(defmacro with-foreign-pointer ((var size &optional size-var) &body body)
  (unless size-var (setf size-var (gensym "SIZE")))
  `(let* ((,size-var ,size)
          (,var (zk-ffi:allocate ,size-var)))
     (unwind-protect (locally ,@body)
       (zk-ffi:free ,var))))

(eval-when (:compile-toplevel :load-toplevel :execute)
  (defun native-type (type)
    ;; Upstream defines :INT8 in terms of :CHAR. Plain C char can be unsigned.
    (if (eq type :char) :signed-char type)))

(defun %foreign-type-size (type) (zk-ffi:type-size type))
(defun %foreign-type-alignment (type) (zk-ffi:type-alignment type))

(defun %mem-ref (pointer type &optional (offset 0))
  (let ((value (zk-ffi:mem-ref (native-pointer pointer) (native-type type) offset)))
    (if (eq type :pointer) (or value *null-pointer*) value)))

(defun %mem-set (value pointer type &optional (offset 0))
  (zk-ffi:mem-set (native-pointer pointer) (native-type type) offset
                  (if (eq type :pointer) (native-pointer value) value))
  value)

(defun make-shareable-byte-vector (size)
  (make-array size :element-type '(unsigned-byte 8) :initial-element 0))

;; The VM does not expose movable array storage to C. CFFI specifies an octet
;; vector and permits a temporary buffer. Copy out even on a non-local exit.
(defmacro with-pointer-to-vector-data ((pointer vector) &body body)
  (let ((source (gensym "VECTOR")) (size (gensym "SIZE")))
    `(let* ((,source ,vector)
            (,size (length ,source)))
       (with-foreign-pointer (,pointer ,size)
         (zk-ffi:write-octets ,pointer ,source :end ,size)
         (unwind-protect (locally ,@body)
           (zk-ffi:read-octets ,pointer ,source :end ,size))))))

(defvar *open-libraries* nil)
;; Only signatures appear in FASLs. Resolved bindings belong to this instance
;; and lose their cache references whenever the native symbol search changes.
(defvar *named-bindings* (make-hash-table :test #'equal))

(defun resolve-named-binding (site signature bind)
  (let ((cell (gethash signature *named-bindings*)))
    (unless (car cell)
      (setf cell (list (funcall bind)))
      (setf (gethash signature *named-bindings*) cell))
    (setf (car site) cell)
    (car cell)))

(defun invalidate-named-bindings ()
  ;; One cell per resolved signature. Compiled call sites can retain an empty
  ;; cell after a library closes, so clear its native owner before dropping it.
  (maphash (lambda (key cell) (declare (ignore key)) (setf (car cell) nil))
           *named-bindings*)
  (clrhash *named-bindings*))

(defun %load-foreign-library (name path)
  (declare (ignore name))
  (let ((handle (zk-ffi:open-library path)))
    (invalidate-named-bindings)
    (push handle *open-libraries*)
    handle))

(defun %close-foreign-library (handle)
  ;; Bound functions and pointers independently retain their native owners.
  (invalidate-named-bindings)
  (setf *open-libraries* (delete handle *open-libraries* :test #'eq))
  t)

(defun %foreign-symbol-pointer (name library)
  (if (eq library :default)
      (or (some (lambda (handle) (zk-ffi:find-symbol-pointer handle name))
                *open-libraries*)
          (zk-ffi:find-symbol-pointer :default name)
          (zk-ffi:find-symbol-pointer :host name))
      (and library (zk-ffi:find-symbol-pointer library name))))

(defun required-symbol-pointer (name library)
  (or (cffi:foreign-symbol-pointer name :library library)
      (error "Undefined C symbol ~S in library ~S." name library)))

(eval-when (:compile-toplevel :load-toplevel :execute)
  (defun check-convention (convention)
    (unless (member convention '(nil :cdecl))
      (error "Unsupported C calling convention ~S." convention)))

  (defun to-native-form (form type)
    (if (eq type :pointer) `(native-pointer ,form) form))

  (defun from-native-form (form type)
    (if (eq type :pointer) `(or ,form *null-pointer*) form))

  (defun expand-call (target args convention &key library fixed-count)
    (check-convention convention)
    (let ((result :void) types arguments)
      (loop while args
            do (let ((type (native-type (pop args))))
                 (if args
                     (progn (push type types) (push (to-native-form (pop args) type) arguments))
                     (setf result type))))
      (setf types (nreverse types))
      (let* ((pointer (if library `(required-symbol-pointer ,target ',library) target))
             ;; Native ABI descriptions may be lists for by-value structures.
             (signature (mapcar (lambda (type) (list 'quote type)) (cons result types)))
             (binding (if fixed-count
                          `(zk-ffi:bind-variadic-pointer ,pointer ,fixed-count ,@signature)
                          `(zk-ffi:bind-pointer ,pointer ,@signature))))
        (when (and library (stringp target))
          (let ((site (gensym "SITE"))
                (key `'(,target ,library ,fixed-count ,result ,@types)))
            (setf binding
                  `(let ((,site (load-time-value (list nil))))
                     (or (caar ,site)
                         (resolve-named-binding ,site ,key (lambda () ,binding)))))))
        (from-native-form `(funcall ,binding ,@(nreverse arguments)) result)))))

(defmacro %foreign-funcall (name args &key (library :default) convention)
  (expand-call name args convention :library library))

(defmacro %foreign-funcall-pointer (pointer args &key convention)
  (expand-call pointer args convention))

(defmacro %foreign-funcall-varargs (name args varargs &key (library :default) convention)
  (expand-call name (append args varargs) convention
               :library library :fixed-count (/ (length args) 2)))

(defmacro %foreign-funcall-pointer-varargs (pointer args varargs &key convention)
  (expand-call pointer (append args varargs) convention :fixed-count (/ (length args) 2)))

(defstruct (named-callback (:copier nil)) signature function pointer)
(defvar *callbacks* (make-hash-table :test #'eq))

(defun register-named-callback (name signature function make-dispatcher)
  (let ((entry (gethash name *callbacks*)))
    (cond (entry
           (unless (equal signature (named-callback-signature entry))
             (error "Callback ~S already has signature ~S and cannot be redefined with ~S."
                    name (named-callback-signature entry) signature))
           (setf (named-callback-function entry) function))
          (t
           (setf entry (make-named-callback :signature signature :function function))
           (setf (named-callback-pointer entry)
                 (apply #'zk-ffi:make-callback (funcall make-dispatcher entry) signature))
           (setf (gethash name *callbacks*) entry)))
    (named-callback-pointer entry)))

(defmacro %defcallback (name result argument-names argument-types body &key convention)
  (check-convention convention)
  (let ((entry (gensym "ENTRY"))
        (pointer-args (loop for arg in argument-names
                            for type in argument-types
                            when (eq type :pointer)
                              collect (list arg (from-native-form arg type)))))
    `(register-named-callback
      ',name ',(mapcar #'native-type (cons result argument-types))
      (lambda ,argument-names (let ,pointer-args ,(to-native-form body result)))
      ;; Native callers can retain the address for the lifetime of the runtime.
      ;; Its fixed-arity dispatcher reads the current target directly from its entry.
      (lambda (,entry)
        (lambda ,argument-names
          (funcall (named-callback-function ,entry) ,@argument-names))))))

(defun %callback (name)
  (let ((entry (gethash name *callbacks*)))
    (if entry (named-callback-pointer entry) (error "Undefined C callback ~S." name))))
