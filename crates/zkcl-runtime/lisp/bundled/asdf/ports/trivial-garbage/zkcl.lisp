;;;; Implementation-dependent operations for the upstream trivial-garbage API.
(in-package #:trivial-garbage)

(defun gc (&key full verbose)
  "Collect the full zkcl heap, then call finalizers discovered at this boundary."
  (declare (ignore full verbose))
  (zk:gc))

(defun make-weak-pointer (object)
  (assert object)
  (zk:make-weak-pointer object))

(defun weak-pointer-p (object)
  (zk:weak-pointer-p object))

(defun weak-pointer-value (pointer)
  (values (zk:weak-pointer-value pointer)))

(defun weakness-keyword-arg (weakness)
  (declare (ignore weakness))
  :weakness)

(defun weakness-keyword-opt (weakness errorp)
  (declare (ignore errorp))
  (ecase weakness
    (:key :key)
    (:value :value)
    (:key-or-value :key-or-value)
    (:key-and-value :key-and-value)))

(defun hash-table-weakness (table)
  (zk:hash-table-weakness table))

(defun finalize (object function)
  "Call FUNCTION with no arguments after OBJECT becomes unreachable. Return OBJECT."
  (zk:finalize object function))

(defun cancel-finalization (object)
  "Remove all finalizers registered on OBJECT."
  (zk:cancel-finalization object))
