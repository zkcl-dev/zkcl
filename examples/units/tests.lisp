(in-package #:examples.units)

(zk-test:deftest exact-conversion ()
  (zk-test:is= 3/2 (convert 90 "s" "min"))
  (zk-test:is= 60 (convert 1 "H" "min"))
  (zk-test:is= 2 (convert (convert 2 "d" "s") "s" "d")))

(zk-test:deftest unknown-unit ()
  (zk-test:signals simple-error (convert 1 "s" "unknown")))

(zk-test:defsuite all exact-conversion unknown-unit)
