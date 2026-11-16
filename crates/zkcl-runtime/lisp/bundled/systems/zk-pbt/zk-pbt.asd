(asdf:defsystem "zk-pbt"
  :license "Apache-2.0"
  :depends-on ("zk-test")
  :serial t
  :components ((:file "generators") (:file "corpus") (:file "runner") (:file "machines")))
