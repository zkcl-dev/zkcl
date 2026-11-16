;;;; Bordeaux Threads 0.9.4 rejects single-threaded hosts in its .asd.
;;;; Its API v1 frontend supplies the documented single-thread defaults.
(asdf:defsystem "bordeaux-threads"
  :version (:read-file-form "version.sexp")
  :license "MIT"
  :depends-on ("alexandria" "global-vars" "trivial-features" "trivial-garbage")
  :components
  ((:static-file "version.sexp")
   (:module "api-v1" :pathname "apiv1/" :serial t
    :components ((:file "pkgdcl")
                 (:file "bordeaux-threads")
                 (:file "types-v1" :pathname #.(zk::port-source-pathname
                                                 "bordeaux-threads/types-v1.lisp"))
                 (:file "default-implementations")))
   (:module "api-v2" :pathname "apiv2/" :depends-on ("api-v1") :serial t
    :components ((:file "pkgdcl")
                 (:file "bordeaux-threads")
                 (:file "timeout-interrupt")
                 (:file "impl-zkcl" :pathname #.(zk::port-source-pathname
                                                 "bordeaux-threads/impl-zkcl.lisp"))
                 (:file "api-locks")
                 (:file "api-threads")
                 (:file "api-semaphores")
                 (:file "api-condition-variables"))))
  :in-order-to ((asdf:test-op (asdf:test-op "bordeaux-threads/test"))))

(asdf:defsystem "bordeaux-threads/test"
  :version (:read-file-form "version.sexp")
  :depends-on ("bordeaux-threads" "fiveam")
  :pathname "test/"
  :serial t
  :components ((:file "tests-v1") (:file "pkgdcl")
               (:file "not-implemented") (:file "tests-v2"))
  :perform (asdf:test-op (o c)
             (uiop:symbol-call :5am :run! :bordeaux-threads)
             (uiop:symbol-call :5am :run! :bordeaux-threads-2)))
