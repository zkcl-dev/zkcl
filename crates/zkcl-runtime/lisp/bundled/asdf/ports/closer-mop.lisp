;; Closer to MOP selects its native package with reader conditionals. The
;; upstream distribution predates ZKCL. Adapt the two platform imports only
;; while ASDF compiles or source-loads that system's package declaration.
(defpackage #:zkcl-closer-mop-port (:use #:common-lisp))
(in-package #:zkcl-closer-mop-port)

(defun closer-component-p (component name)
  (and (string= name (asdf:component-name component))
       (equal "closer-mop" (asdf:component-name (asdf:component-system component)))))

(defun native-import (option)
  (if (and (consp option) (eq (first option) :import-from) (symbolp (second option))
           (member (symbol-name (second option)) '("CLASSP" "DIRECT-SLOT-DEFINITION") :test #'string=)
           (not (find-package (second option))))
      (list* :import-from :zk-mop (rest option))
      option))

(defmethod asdf/lisp-action:call-with-around-compile-hook :around
    ((component asdf:cl-source-file) thunk)
  (declare (ignore thunk))
  (if (closer-component-p component "closer-mop-packages")
      (let* ((outer *macroexpand-hook*)
             (*macroexpand-hook*
               (lambda (expander form environment)
                 (funcall outer expander
                   (if (and (consp form) (eq (first form) 'cl:defpackage)
                            (string= (second form) "CLOSER-MOP"))
                       (list* (first form) (second form) (mapcar #'native-import (cddr form)))
                       form)
                   environment))))
        (call-next-method))
      (call-next-method)))

(defun ensure-method (gf expression &key (method-class (zk-mop:generic-function-method-class gf))
                           qualifiers (lambda-list (second expression))
                           (specializers (loop for p in lambda-list until (member p lambda-list-keywords)
                                               collect (find-class t))))
  (multiple-value-bind (body initargs)
      (zk-mop:make-method-lambda gf (zk-mop:class-prototype method-class) expression nil)
    (let ((method (apply #'make-instance method-class :qualifiers qualifiers
                         :lambda-list lambda-list :specializers specializers
                         :function (compile nil body) initargs)))
      (add-method gf method)
      method)))

(defun effective-function (gf form options)
  (multiple-value-bind (entry uses-methods)
      (zk-mop::%compile-effective (zk-mop::%effective-option-form gf form options))
    (lambda (&rest arguments)
      (multiple-value-bind (methods standard) (zk-mop::%standard-effective-methods gf arguments uses-methods)
        (funcall entry arguments gf
                 (if standard methods (zk-mop::%applicable-methods gf arguments))
                 #'zk-mop::%check-next-methods entry)))))

(defun install-zkcl-mop-definitions ()
  ;; A registry entry named "closer-mop" can be an aggregator or a native
  ;; implementation. Only complete the public interface actually declared by
  ;; the selected frontend. Preserve definitions supplied by the distribution.
  (let ((package (find-package "CLOSER-MOP")))
    (when package
      (flet ((public-symbol (name)
               (multiple-value-bind (symbol status) (find-symbol name package)
                 (when (eq status :external) symbol))))
        (let ((ensure (public-symbol "ENSURE-METHOD"))
              (fix (public-symbol "FIX-SLOT-INITARGS"))
              (effective (public-symbol "COMPUTE-EFFECTIVE-METHOD-FUNCTION"))
              (warn (public-symbol "WARN-ON-DEFMETHOD-WITHOUT-GENERIC-FUNCTION")))
          (when (and ensure fix effective warn)
            (when (or (not (fboundp ensure)) (eq (fdefinition ensure) #'ensure-method))
              ;; The upstream unsupported branch defines FIX-SLOT-INITARGS
              ;; with an empty body. zkcl supplies the original initarg list.
              (setf (fdefinition ensure) #'ensure-method
                    (fdefinition fix) #'identity)
              (unless (fboundp effective)
                (eval `(defmethod ,effective ((gf standard-generic-function) form options)
                         (effective-function gf form options))))
              (eval `(define-symbol-macro ,warn nil)))
            (pushnew :closer-mop *features*)))))))

(defmethod asdf:perform :after ((operation asdf:load-op) (component asdf:system))
  (when (equal "closer-mop" (asdf:component-name component)) (install-zkcl-mop-definitions)))

(defmethod asdf:perform :after ((operation asdf:load-source-op) (component asdf:cl-source-file))
  (when (closer-component-p component "closer-mop-shared") (install-zkcl-mop-definitions)))
