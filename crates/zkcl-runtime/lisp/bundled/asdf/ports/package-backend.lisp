;;;; The pinned upstream definition contains one DEFSYSTEM. Its first Lisp
;;;; component defines the package extended by the native backend.
(let* ((definition (uiop:read-file-form *load-truename*))
       (name (asdf:coerce-name (second definition)))
       (options (cddr definition))
       (components (getf options :components))
       (package-file (second (first components))))
  (setf (getf options :components)
        (append components
                (list (list :file "zkcl" :depends-on (list package-file)
                            :pathname (zk::port-source-pathname
                                       (format nil "~A/zkcl.lisp" name))))))
  (eval definition))
