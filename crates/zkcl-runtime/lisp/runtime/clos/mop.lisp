;; Class construction runs on the caller's VM stack. In particular, validation,
;; metaclass initforms, and user initialization methods can exit nonlocally.
(defvar zk-mop::*provisional-class* nil)
(defvar zk-mop::*changing-forward-class* nil)

(defun zk-mop:ensure-class (name &rest initargs)
  (apply #'zk-mop:ensure-class-using-class (zk-mop::%class-target name) name initargs))

(defgeneric zk-mop:ensure-class-using-class (class name &rest initargs))

(defun zk-mop::%class-initargs (initargs)
  (let ((options (copy-list initargs)))
    (loop while (remf options :metaclass))
    (multiple-value-bind (key parents) (get-properties options '(:direct-superclasses))
      (when key (setf (getf options :direct-superclasses) (zk-mop::%resolve-parents parents))))
    options))

(defmethod zk-mop:ensure-class-using-class ((class null) name &rest initargs
                                            &key (metaclass 'standard-class) &allow-other-keys)
  (zk-mop::%define-named-class name
    (lambda ()
      (apply #'make-instance (zk-mop::%metaclass metaclass) :name name (zk-mop::%class-initargs initargs)))))

(defmethod zk-mop:ensure-class-using-class ((class class) name &rest initargs
                                            &key (metaclass 'standard-class) &allow-other-keys)
  (let ((meta (zk-mop::%metaclass metaclass))
        (options (zk-mop::%class-initargs initargs)))
    (cond ((zk-mop::%compiler-class-p class)
           ;; COMPILE-FILE reserved this identity before the load-time methods ran.
           (let* ((zk-mop::*provisional-class* (cons meta class))
                  (change (not (eq (class-of class) meta)))
                  (storage (when change (zk-mop::%attach-provisional-metaclass class meta))))
             (unwind-protect (apply #'make-instance meta :name name options)
               (when (and change (zk-mop::%compiler-class-p class))
                 (zk-mop::%restore-unpublished-storage class storage)))))
          ((eq (class-of class) meta)
           (apply #'reinitialize-instance class options)
           class)
          (t (error 'program-error)))))

(defmethod zk-mop:ensure-class-using-class ((class zk-mop:forward-referenced-class) name &rest initargs
                                            &key (metaclass 'standard-class) &allow-other-keys)
  (declare (ignore name))
  (let ((meta (zk-mop::%metaclass metaclass))
        (options (zk-mop::%class-initargs initargs))
        (storage (zk-mop::%class-storage class))
        (complete nil))
    (unwind-protect
        (progn (unless (eq (class-of class) meta) (apply #'change-class class meta options))
               (apply #'reinitialize-instance class options)
               (setf complete t)
               class)
      (unless complete (zk-mop::%restore-unpublished-storage class storage)))))

(defmethod change-class ((class zk-mop:forward-referenced-class) (new-metaclass class) &rest initargs)
  (declare (ignore initargs))
  (let ((storage (zk-mop::%class-storage class))
        (zk-mop::*changing-forward-class* class)
        (complete nil))
    (unwind-protect
        (prog1 (call-next-method) (setf complete t))
      (unless complete (zk-mop::%restore-unpublished-storage class storage)))))

(defmethod shared-initialize ((class zk-mop:forward-referenced-class) slot-names &rest initargs
                              &key (name nil name-p))
  (declare (ignore slot-names initargs))
  (call-next-method)
  (when name-p (setf (class-name class) name))
  class)

(defgeneric zk-mop:validate-superclass (class superclass))

(defmethod zk-mop:validate-superclass ((class class) (superclass class))
  (or (eq superclass (find-class t))
      (eq (class-of class) (class-of superclass))
      (and (member (class-of class) (list (find-class 'standard-class) (find-class 'zk-mop:funcallable-standard-class)))
           (member (class-of superclass) (list (find-class 'standard-class) (find-class 'zk-mop:funcallable-standard-class))))
      (typep superclass 'zk-mop:forward-referenced-class)))

(defun zk-mop::%install-accessors (class)
  (dolist (slot (zk-mop:class-direct-slots class))
    (dolist (name (zk-mop:slot-definition-readers slot)) (zk-mop::%install-one-accessor class slot name nil))
    (dolist (name (zk-mop:slot-definition-writers slot)) (zk-mop::%install-one-accessor class slot name t))))


(defgeneric zk-mop:direct-slot-definition-class (class &rest initargs))
(defmethod zk-mop:direct-slot-definition-class ((class class) &rest initargs)
  (declare (ignore initargs))
  (find-class 'zk-mop:standard-direct-slot-definition))

(defgeneric zk-mop:effective-slot-definition-class (class &rest initargs))
(defmethod zk-mop:effective-slot-definition-class ((class class) &rest initargs)
  (declare (ignore initargs))
  (find-class 'zk-mop:standard-effective-slot-definition))

(defmethod shared-initialize ((slot zk-mop:standard-slot-definition) names &rest initargs
                              &key name ((:initargs slot-initargs)) initfunction initform allocation
                                   readers writers type documentation)
  (declare (ignore names name slot-initargs initfunction initform allocation readers writers type documentation))
  (call-next-method)
  (apply #'zk-mop::%initialize-slot slot initargs))

(macrolet ((define-writer (reader initarg)
             `(defmethod (setf ,reader) (value (slot zk-mop:standard-effective-slot-definition))
                (zk-mop::%set-effective-slot-field slot ,initarg value))))
  (define-writer zk-mop:slot-definition-name :name)
  (define-writer zk-mop:slot-definition-type :type)
  (define-writer zk-mop:slot-definition-initargs :initargs)
  (define-writer zk-mop:slot-definition-initform :initform)
  (define-writer zk-mop:slot-definition-initfunction :initfunction)
  (define-writer zk-mop:slot-definition-allocation :allocation))

(defmethod reinitialize-instance ((slot zk-mop:slot-definition) &rest initargs)
  (declare (ignore slot initargs))
  (error 'program-error))

(defun zk-mop::%compute-defaults (class)
  (let ((seen (make-hash-table :test 'eq)) (result nil))
    (dolist (parent (zk-mop:class-precedence-list class))
      (dolist (default (zk-mop:class-direct-default-initargs parent))
        (unless (gethash (first default) seen)
          (setf (gethash (first default) seen) t)
          (push default result))))
    (nreverse result)))

(defmethod zk-mop:compute-default-initargs ((class standard-class))
  (zk-mop::%compute-defaults class))

(defmethod zk-mop:compute-default-initargs ((class zk-mop:funcallable-standard-class))
  (zk-mop::%compute-defaults class))

(defgeneric zk-mop:compute-effective-slot-definition (class name direct-slots))

(defmethod zk-mop:compute-effective-slot-definition ((class class) name direct-slots)
  (let* ((first (car direct-slots))
         (initial (find-if #'zk-mop:slot-definition-initfunction direct-slots))
         (doc (some #'zk-mop:slot-definition-documentation direct-slots))
         (types (remove t (mapcar #'zk-mop:slot-definition-type direct-slots)))
         (initargs (remove-duplicates
                    (mapcan (lambda (slot) (copy-list (zk-mop:slot-definition-initargs slot)))
                            direct-slots) :from-end t))
         (options (list :name name :allocation (zk-mop:slot-definition-allocation first)
                        :initargs initargs :type (cond ((null types) t) ((cdr types) (cons 'and types)) (t (car types))))))
    (when initial
      (setf options (list* :initfunction (zk-mop:slot-definition-initfunction initial)
                           :initform (zk-mop:slot-definition-initform initial) options)))
    (when doc (setf options (list* :documentation doc options)))
    (apply #'make-instance (apply #'zk-mop:effective-slot-definition-class class options) options)))

(defun zk-mop::%compute-standard-slots (class)
  (let ((groups (make-hash-table :test 'eq)) (names nil))
    (dolist (parent (zk-mop:class-precedence-list class))
      (dolist (slot (zk-mop:class-direct-slots parent))
        (let ((name (zk-mop:slot-definition-name slot)))
          (unless (gethash name groups) (push name names))
          (push slot (gethash name groups)))))
    (mapcar (lambda (name) (zk-mop:compute-effective-slot-definition
                            class name (nreverse (gethash name groups))))
            (nreverse names))))

(defmethod zk-mop:compute-slots ((class standard-class))
  (zk-mop::%compute-standard-slots class))

(defmethod zk-mop:compute-slots ((class zk-mop:funcallable-standard-class))
  (zk-mop::%compute-standard-slots class))

(defmethod zk-mop:compute-slots :around ((class standard-class))
  (zk-mop::%assign-slot-locations (call-next-method)))

(defmethod zk-mop:compute-slots :around ((class zk-mop:funcallable-standard-class))
  (zk-mop::%assign-slot-locations (call-next-method)))

(defun zk-mop::%direct-slot (class options)
  (apply #'make-instance (apply #'zk-mop:direct-slot-definition-class class options) options))

(defun zk-mop::%initialize-shared-slots (entries)
  (dolist (entry entries)
    (destructuring-bind (owner name cell initfunction) entry
      (zk-mop::%initialize-class-slot owner name cell (funcall initfunction)))))

(defun zk-mop::%finalize-class (class)
  (zk-mop::%use-precedence class (zk-mop:compute-class-precedence-list class))
  (zk-mop::%initialize-shared-slots
   (zk-mop::%use-layout class (zk-mop:compute-slots class) (zk-mop:compute-default-initargs class))))

(defun zk-mop::%finalize-standard-class (class fallback)
  (cond ((zk-mop::%pending-class-p class) (zk-mop::%finalize-class class))
        ((and (zk-mop::%class-active-p class) (zk-mop:class-finalized-p class))
         (zk-mop::%configure-and-finalize class (zk-mop:class-direct-superclasses class) nil class))
        (t (funcall fallback))))

(defmethod zk-mop:finalize-inheritance ((class standard-class))
  (zk-mop::%finalize-standard-class class (lambda () (call-next-method))))

(defmethod zk-mop:finalize-inheritance ((class zk-mop:funcallable-standard-class))
  (zk-mop::%finalize-standard-class class (lambda () (call-next-method))))

(defun zk-mop::%finish-class-change (shared &optional obsolete-accessors)
  (unwind-protect
      (dolist (entry obsolete-accessors)
        (destructuring-bind (gf method) entry
          (when (eq gf (zk-mop:method-generic-function method)) (remove-method gf method))))
    ;; A hook may leave nonlocally. The new layout already owns its accessors.
    (dolist (entry obsolete-accessors)
      (destructuring-bind (gf method) entry
        (when (eq gf (zk-mop:method-generic-function method)) (zk-mop::%discard-method gf method)))))
  shared)

(defun zk-mop::%configure-and-finalize (class parents options &optional already-entered (old-parents nil notify))
  (multiple-value-bind (token affected) (zk-mop::%configure-class class parents options)
    (multiple-value-call #'zk-mop::%finish-class-change
      (unwind-protect
        (progn
          (dolist (candidate affected)
            (if (eq candidate already-entered)
                (zk-mop::%finalize-class candidate)
                (zk-mop:finalize-inheritance candidate)))
          (when notify (zk-mop::%update-subclass-links class old-parents parents))
          (zk-mop::%publish-class token))
      (zk-mop::%end-class-update token)))))

(defun zk-mop::%initialize-class (initialize class slot-names &rest options
                              &key (name nil name-p) (direct-superclasses nil parents-p)
                                   (direct-slots nil slots-p) (direct-default-initargs nil defaults-p)
                                   (documentation nil doc-p) &allow-other-keys)
  (declare (ignore name direct-default-initargs documentation))
  ;; CHANGE-CLASS initializes the new metaclass slots. ENSURE-CLASS then passes
  ;; the class definition to REINITIALIZE-INSTANCE in a separate protocol call.
  (when (eq class zk-mop::*changing-forward-class*)
    (funcall initialize nil)
    (return-from zk-mop::%initialize-class class))
  (let* ((pending (zk-mop::%pending-class-p class))
         (new (and (not pending) (not (zk-mop::%class-active-p class)))))
    ;; Updating an obsolete metaclass instance can call SHARED-INITIALIZE
    ;; with no added slots while this very class is being finalized.
    (if (or new name-p parents-p slots-p defaults-p doc-p
            (and (null slot-names)
                 (or options (not pending))))
        (let* ((old-parents (zk-mop:class-direct-superclasses class))
               (parents (or (if parents-p direct-superclasses
                               old-parents)
                           (list (find-class (if (typep class 'zk-mop:funcallable-standard-class)
                                                 'zk-mop:funcallable-standard-object 'standard-object))))))
          (dolist (parent parents)
            (unless (zk-mop:validate-superclass class parent)
              (error 'program-error)))
          (funcall initialize new)
          (when slots-p
            (setf options (copy-list options))
            (setf (getf options :direct-slots)
                  (mapcar (lambda (spec) (zk-mop::%direct-slot class spec)) direct-slots)))
          (zk-mop::%initialize-shared-slots
           (zk-mop::%configure-and-finalize class parents options nil (unless new old-parents)))
          (when (or new slots-p) (zk-mop::%install-accessors class)))
        (funcall initialize new)))
  class)

(defmethod shared-initialize ((class standard-class) slot-names &rest options
                              &key name direct-superclasses direct-slots direct-default-initargs documentation)
  (declare (ignore name direct-superclasses direct-slots direct-default-initargs documentation))
  (apply #'zk-mop::%initialize-class (lambda (new)
           (if (and new (null slot-names))
               (apply #'call-next-method class t options)
               (call-next-method))) class slot-names options))

(defmethod shared-initialize ((class zk-mop:funcallable-standard-class) slot-names &rest options
                              &key name direct-superclasses direct-slots direct-default-initargs documentation)
  (declare (ignore name direct-superclasses direct-slots direct-default-initargs documentation))
  (apply #'zk-mop::%initialize-class (lambda (new)
           (if (and new (null slot-names))
               (apply #'call-next-method class t options)
               (call-next-method))) class slot-names options))

(defun zk-mop:standard-instance-access (instance location)
  (zk-mop::%update-instance instance)
  (zk-mop::%local-slot instance location nil))

(defun (setf zk-mop:standard-instance-access) (value instance location)
  (zk-mop::%update-instance instance)
  (zk-mop::%local-slot instance location nil value))

(defun zk-mop:funcallable-standard-instance-access (instance location)
  (zk-mop::%update-instance instance)
  (zk-mop::%local-slot instance location t))

(defun (setf zk-mop:funcallable-standard-instance-access) (value instance location)
  (zk-mop::%update-instance instance)
  (zk-mop::%local-slot instance location t value))
