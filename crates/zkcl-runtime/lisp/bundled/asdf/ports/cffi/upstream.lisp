;;;; Adapt the core of the selected upstream CFFI distribution. Its version
;;;; handling, secondary systems and test operation are evaluated verbatim.
(in-package #:asdf-user)

(unless (find-package :zkcl-cffi-port)
  (make-package :zkcl-cffi-port :use '(:cl)))

(flet ((backend (name)
         (list :file name :pathname
               (zk::port-source-pathname (format nil "cffi/~A.lisp" name)))))
  (with-open-file (input *load-truename*)
    (loop for form = (read input nil input) until (eq form input) do
      (cond
        ((and (consp form) (eq (first form) 'cl:error))
         ;; The only such top-level form in the recognized .asd is its
         ;; implementation guard. ZK-FFI was checked by port selection.
         nil)
        ((and (consp form) (eq (first form) 'asdf:defsystem) (equal (second form) "cffi"))
         (let ((module (first (getf (cddr form) :components))))
           (setf (getf (cddr module) :components)
                 (loop for file in (getf (cddr module) :components)
                       collect file
                       when (equal (second file) "sys-utils") collect (backend "cffi-zkcl")
                       when (equal (second file) "strings") collect (backend "cffi-utf8")
                       when (equal (second file) "features") collect (backend "cffi-aggregate"))))
         (eval form))
        (t (eval form))))))
