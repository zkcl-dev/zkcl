(in-package #:embed.native)
(eval-when (:compile-toplevel)
  (setf (get 'answer 'compile-value) 42))
(defmacro compile-answer () (get 'answer 'compile-value 0))
