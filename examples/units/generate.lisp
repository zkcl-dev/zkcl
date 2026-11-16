;;;; Turn the declared input into a library source file under ZKCL_OUT_DIR.

(let* ((*read-eval* nil)
       (units (with-open-file (input "units.sexp") (read input)))
       (destination (merge-pathnames "generated-units.lisp" (zk:getenv "ZKCL_OUT_DIR"))))
  (with-open-file (output destination :direction :output :if-exists :supersede)
    (format output "(in-package #:examples.units)~%~%(defparameter *units* '~S)~%" units)))
