(in-package #:embed.asdf)
(defparameter *resource*
  (with-open-file (input (asdf:system-relative-pathname "embed-asdf" "data.txt"))
    (read-line input)))
(defun answer () (native:compile-answer))
