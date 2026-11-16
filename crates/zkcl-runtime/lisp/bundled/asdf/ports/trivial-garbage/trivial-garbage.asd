;;;; zkcl implementation selection for the upstream 20231021 distribution.
(asdf:defsystem "trivial-garbage"
  :description "Portable finalizers, weak hash tables and weak pointers."
  :author "Luis Oliveira <loliveira@common-lisp.net>"
  :license "Public Domain"
  :in-order-to ((asdf:test-op (asdf:test-op "trivial-garbage/tests")))
  :serial t
  :components ((:file "trivial-garbage")
               (:file "zkcl" :pathname #.(zk::port-source-pathname "trivial-garbage/zkcl.lisp"))))

(asdf:defsystem "trivial-garbage/tests"
  :depends-on ("trivial-garbage" "rt")
  :components ((:file "tests"))
  :perform (asdf:test-op (op system)
             (declare (ignore op system))
             (unless (uiop:symbol-call :trivial-garbage-tests :run)
               (error "The trivial-garbage regression suite failed."))))
