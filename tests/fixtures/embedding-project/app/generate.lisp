(let ((offset (with-open-file (input "input.txt") (read input))))
  (assert (zk:getenv "ZKCL_BUILD_TARGET"))
  (with-open-file (output (merge-pathnames "generated.lisp"
                            (pathname (zk:getenv "ZKCL_OUT_DIR")))
                    :direction :output :if-exists :supersede :if-does-not-exist :create)
    (format output "(defparameter cl-user::*embedded-generated* ~D)~%" offset)))
