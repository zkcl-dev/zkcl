;;;; zkcl's system definition for the upstream CFFI core, Quicklisp 2026-01-01.
;;;; The upstream .asd selects from a closed set of Lisp implementations.

(asdf:defsystem "cffi"
  :description "Common Foreign Function Interface with the zkcl backend"
  :license "MIT"
  :pathname #.zkcl-cffi-port::*cffi-directory*
  :depends-on ("trivial-features" "alexandria" "babel" (:feature :darwin "uiop"))
  :in-order-to ((asdf:test-op (asdf:test-op "cffi-tests")))
  :components
  ((:module "src"
    :serial t
    :components
    ((:file "package")
     (:file "sys-utils")
     (:file "cffi-zkcl" :pathname #.(merge-pathnames "cffi-zkcl.lisp" zkcl-cffi-port::*port-directory*))
     (:file "utils")
     (:file "darwin-frameworks" :if-feature :darwin)
     (:file "libraries")
     (:file "early-types")
     (:file "types")
     (:file "enum")
     (:file "strings")
     (:file "cffi-utf8" :pathname #.(merge-pathnames "cffi-utf8.lisp" zkcl-cffi-port::*port-directory*))
     (:file "structures")
     (:file "functions")
     (:file "foreign-vars")
     (:file "features")
     (:file "cffi-aggregate" :pathname #.(merge-pathnames "cffi-aggregate.lisp" zkcl-cffi-port::*port-directory*))))))

;; Like the upstream Git distribution, an unversioned checkout satisfies
;; version constraints from systems depending on CFFI.
(defmethod asdf:version-satisfies ((system (eql (asdf:find-system "cffi"))) version)
  (or (null (asdf:component-version system)) (call-next-method)))
