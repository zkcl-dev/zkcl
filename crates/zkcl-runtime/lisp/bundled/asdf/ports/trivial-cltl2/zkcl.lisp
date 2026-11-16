(in-package #:trivial-cltl2)

(setf (macro-function 'define-declaration) (macro-function 'zk:define-declaration)
      (fdefinition 'declaration-information) (fdefinition 'zk:declaration-information))
