(in-package #:bordeaux-threads)

;; The v1 defaults handle acquisition, release, and unsupported waits.
;; Give the exported lock types an identity for callers' type declarations.
(defstruct (lock (:constructor make-lock (&optional name))) name)
(defstruct (recursive-lock (:constructor make-recursive-lock (&optional name))) name)

;; This handle identifies the only execution thread of this Runtime. It is
;; also the native key for the upstream API v2 wrapper. DEFVAR preserves the
;; handle (and any callers' identity-keyed state) across a forced ASDF reload.
(defstruct (thread (:constructor %make-main-thread ()) (:copier nil) (:predicate threadp))
  (name "Main thread" :read-only t))
(defvar *main-thread* (%make-main-thread))

(defun current-thread () *main-thread*)
(defun all-threads () (list *main-thread*))
(defun thread-alive-p (thread) (eq thread *main-thread*))
