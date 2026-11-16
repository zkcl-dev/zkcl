(export '(zk::define-declaration zk::declaration-information) :zk)

(defmacro zk:define-declaration (name lambda-list &body body)
  "Define a lexical declaration handler. Return :DECLARE and (KEY . VALUE)
to make VALUE available to DECLARATION-INFORMATION in that scope."
  (let ((spec (gensym "SPEC")) (env (gensym "ENV"))
        (kind (gensym "KIND")) (data (gensym "DATA")))
    `(eval-when (:compile-toplevel :load-toplevel :execute)
       (proclaim '(declaration ,name))
       (zk::%register-declaration-handler ',name
         (lambda (,spec ,env)
           (multiple-value-bind (,kind ,data)
               (funcall (function (lambda ,lambda-list ,@body)) ,spec ,env)
             (unless (eq ,kind :declare)
               (error "Declaration ~S returned ~S; expected :DECLARE." ',name ,kind))
             ,data))))))
