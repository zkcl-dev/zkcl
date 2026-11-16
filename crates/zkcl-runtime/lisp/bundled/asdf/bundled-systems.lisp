(in-package #:cl-user)

(defun zk::bundled-system (name)
  (when (member name '("zk-test" "zk-pbt" "zk-syntax") :test #'string=)
    (or (asdf/system-registry:registered-system name)
        (make-instance 'asdf:require-system :name name :module name))))

(pushnew 'zk::bundled-system asdf:*system-definition-search-functions*)
