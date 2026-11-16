(asdf:defsystem "trivial-gray-streams"
  :description "Compatibility layer for Gray Streams"
  :license "MIT"
  :version "2.1"
  :serial t
  :components ((:file "package" :pathname #.(zk::port-source-pathname "trivial-gray-streams/package.lisp"))
               (:file "streams")
               (:file "zkcl" :pathname #.(zk::port-source-pathname "trivial-gray-streams/zkcl.lisp"))))
