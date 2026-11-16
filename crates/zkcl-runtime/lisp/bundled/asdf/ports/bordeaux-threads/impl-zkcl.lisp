;;;; A zkcl image has one Lisp execution thread. Each independent Runtime
;;;; has its own objects, including the main thread and its locks.
(in-package #:bt2)

(defun %current-thread () (bt:current-thread))
(defun %all-threads () (bt:all-threads))
(defun %thread-name (thread) (bt:thread-name thread))
(defun %thread-alive-p (thread) (bt:thread-alive-p thread))
(defun %thread-yield () (zk-async:yield) (values))

(defun %make-thread (function name)
  (declare (ignore function name))
  (signal-not-implemented 'make-thread))

(defun %join-thread (thread)
  (declare (ignore thread))
  (signal-not-implemented 'join-thread))

(defun %destroy-thread (thread)
  (declare (ignore thread))
  (signal-not-implemented 'destroy-thread))

(defun %interrupt-thread (thread function)
  (declare (ignore thread function))
  (signal-not-implemented 'interrupt-thread))

(defun %lock-holder ()
  (if (zk::%child-task-p) (zk-async:current-task) :root))

(defun %acquire-semaphore-with-timeout (semaphore wait timeout marker)
  (or (zk::%try-acquire-semaphore semaphore marker)
      (and wait
           (if timeout
               (zk-async:with-timeout (timeout :on-timeout nil)
                 (zk::%acquire-semaphore semaphore marker))
               (zk::%acquire-semaphore semaphore marker)))))

(defstruct (native-lock (:constructor %new-lock (name semaphore)) (:predicate nil))
  name semaphore owner)

(defun %make-lock (name)
  (%new-lock name (zk-async:make-semaphore 1)))

(defun %take-lock (lock wait timeout marker)
  (when (%acquire-semaphore-with-timeout (native-lock-semaphore lock) wait timeout marker)
    (setf (native-lock-owner lock) (%lock-holder))
    t))

(defun %drop-lock-permit (lock)
  (setf (native-lock-owner lock) nil)
  (zk-async:release-semaphore (native-lock-semaphore lock)))

(defun %acquire-lock (lock wait timeout)
  (let ((marker (cons nil nil)) (kept nil))
    (unwind-protect
         (setq kept (%take-lock lock wait timeout marker))
      (when (and (car marker) (not kept))
        (%drop-lock-permit lock)))))

(defun %release-lock (lock)
  (zk-async:protect-from-cancel
    (unless (eq (native-lock-owner lock) (%lock-holder))
      (bt-error "Lock ~S is not held by the calling task" (native-lock-name lock)))
    (%drop-lock-permit lock))
  (values))

(defstruct (native-recursive-lock
            (:constructor %new-recursive-lock (name semaphore))
            (:predicate nil))
  name semaphore owner (depth 0))

(defun %make-recursive-lock (name)
  (%new-recursive-lock name (zk-async:make-semaphore 1)))

(defun %take-recursive-lock (lock wait timeout marker)
  (let ((self (%lock-holder)))
    (cond
      ((eq (native-recursive-lock-owner lock) self)
       (zk-async:protect-from-cancel
         (incf (native-recursive-lock-depth lock))
         (setf (car marker) :nested)
         t))
      ((%acquire-semaphore-with-timeout (native-recursive-lock-semaphore lock) wait timeout marker)
       (setf (native-recursive-lock-owner lock) self
             (native-recursive-lock-depth lock) 1)
       t))))

(defun %drop-recursive-permit (lock)
  (setf (native-recursive-lock-owner lock) nil
        (native-recursive-lock-depth lock) 0)
  (zk-async:release-semaphore (native-recursive-lock-semaphore lock)))

(defun %undo-recursive-acquisition (lock marker)
  (case (car marker)
    (:nested (decf (native-recursive-lock-depth lock)))
    ((t) (%drop-recursive-permit lock))))

(defun %acquire-recursive-lock (lock wait timeout)
  (let ((marker (cons nil nil)) (kept nil))
    (unwind-protect
         (setq kept (%take-recursive-lock lock wait timeout marker))
      (unless kept
        (%undo-recursive-acquisition lock marker)))))

(defun %release-recursive-lock (lock)
  (zk-async:protect-from-cancel
    (unless (eq (native-recursive-lock-owner lock) (%lock-holder))
      (bt-error "Recursive lock ~S is not held by the calling task" (native-recursive-lock-name lock)))
    (when (zerop (decf (native-recursive-lock-depth lock)))
      (%drop-recursive-permit lock)))
  (values))

;; The upstream WITH-LOCK-HELD macros use these hooks when available.
;; Establish cleanup before acquisition. The semaphore records the permit
;; as part of the handoff, including when cancellation is already pending.
(defmacro %with-lock ((place timeout) &body body)
  (let ((lock (gensym "LOCK")) (marker (gensym "PERMIT")))
    `(let ((,lock ,place) (,marker (cons nil nil)))
       (unwind-protect
            (when (%take-lock ,lock t ,timeout ,marker)
              (locally ,@body))
         (when (car ,marker)
           (%drop-lock-permit ,lock))))))

(defmacro %with-recursive-lock ((place timeout) &body body)
  (let ((lock (gensym "LOCK")) (marker (gensym "PERMIT")))
    `(let ((,lock ,place) (,marker (cons nil nil)))
       (unwind-protect
            (when (%take-recursive-lock ,lock t ,timeout ,marker)
              (locally ,@body))
         (%undo-recursive-acquisition ,lock ,marker)))))

(defstruct (condition-variable (:constructor %make-condition-variable (name))
                               (:predicate nil))
  name waiters)

(defun %condition-wait (variable lock timeout)
  (check-type variable condition-variable)
  (let ((event (zk-async:make-event)) (released nil))
    (unwind-protect
         (progn
           (zk-async:protect-from-cancel
             (setf (condition-variable-waiters variable)
                   (nconc (condition-variable-waiters variable) (list event)))
             (%release-lock lock)
             (setq released t))
           (if timeout
               (zk-async:with-timeout (timeout :on-timeout nil)
                 (zk-async:wait-event event))
               (zk-async:wait-event event)))
      (setf (condition-variable-waiters variable)
            (delete event (condition-variable-waiters variable) :test #'eq :count 1))
      (when released
        (zk-async:protect-from-cancel (%acquire-lock lock t nil))))))

(defun %condition-notify (variable)
  (check-type variable condition-variable)
  (let ((event (pop (condition-variable-waiters variable))))
    (when event
      (zk-async:set-event event)))
  (values))

(defun %condition-broadcast (variable)
  (check-type variable condition-variable)
  (let ((waiters (condition-variable-waiters variable)))
    (setf (condition-variable-waiters variable) nil)
    (dolist (event waiters)
      (zk-async:set-event event)))
  (values))

(defconstant +word-bits+ #+64-bit 64 #+32-bit 32)
(deftype %atomic-integer-value () (list 'unsigned-byte +word-bits+))
(defstruct (atomic-integer (:constructor %wrap-atomic-integer (cell)) (:copier nil))
  (cell nil :read-only t))

(defun make-atomic-integer (&key (value 0))
  (%wrap-atomic-integer (zk::%make-atomic-word value)))

(defun atomic-integer-value (integer)
  (zk::%atomic-word-value (atomic-integer-cell integer)))

(defmethod print-object ((integer atomic-integer) stream)
  (print-unreadable-object (integer stream :type t :identity t)
    (format stream "~D" (atomic-integer-value integer))))

(defun (setf atomic-integer-value) (value integer)
  (zk::%set-atomic-word (atomic-integer-cell integer) value))

(defun atomic-integer-compare-and-swap (integer old new)
  (zk::%atomic-word-compare-exchange (atomic-integer-cell integer) old new))

(defun update-atomic-integer (integer delta)
  (let ((cell (atomic-integer-cell integer)))
    (loop for old = (zk::%atomic-word-value cell)
          for new = (ldb (byte +word-bits+ 0) (+ old delta))
          when (zk::%atomic-word-compare-exchange cell old new) return new)))

(defun atomic-integer-incf (integer &optional (delta 1))
  (check-type delta %atomic-integer-value)
  (update-atomic-integer integer delta))

(defun atomic-integer-decf (integer &optional (delta 1))
  (check-type delta %atomic-integer-value)
  (update-atomic-integer integer (- delta)))

(dolist (op '(make-thread join-thread destroy-thread interrupt-thread
              signal-in-thread warn-in-thread error-in-thread
              with-timeout))
  (mark-not-implemented op))
