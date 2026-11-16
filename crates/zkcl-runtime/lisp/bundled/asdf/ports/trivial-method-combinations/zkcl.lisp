(in-package #:trivial-method-combinations)

(defun method-combination-name (method-combination)
  "Returns the name of the method combination."
  (check-type method-combination method-combination)
  (zk-mop:method-combination-name method-combination))

(defun method-combination-arglist (method-combination)
  "Returns the argument list of the method combination."
  (check-type method-combination method-combination)
  (zk-mop:method-combination-arglist method-combination))
