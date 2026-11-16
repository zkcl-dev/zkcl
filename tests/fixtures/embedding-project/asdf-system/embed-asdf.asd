(asdf:defsystem "embed-asdf"
  :version "1.0.0"
  :depends-on ("embed-native")
  :serial t
  :components ((:file "package") (:module "code" :pathname "" :components ((:file "api")))))
