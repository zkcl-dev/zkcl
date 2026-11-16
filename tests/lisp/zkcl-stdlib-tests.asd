(asdf:defsystem "zkcl-stdlib-tests"
  :license "Apache-2.0"
  :depends-on ("zk-test" "zk-pbt")
  :serial t
  :components
  ((:module "stdlib"
    :components
    ((:file "package")
     (:file "numbers")
     (:file "reader-printer")
     (:file "equality-and-tables")
     (:file "sequences-and-arrays")
     (:file "pathnames")
     (:file "types-and-eval")
     #+zkcl (:file "zkcl-extensions")
     (:file "suites"))))
  :perform (asdf:test-op (o c)
             (declare (ignore o c))
             (uiop:symbol-call :zk-test :check
                               (uiop:find-symbol* :stdlib-all :zkcl-stdlib-tests))))
