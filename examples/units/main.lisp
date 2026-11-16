(in-package #:examples.units)

(defun main ()
  (let ((arguments (zk:command-line-arguments)))
    (if (= (length arguments) 3)
        (destructuring-bind (amount from to) arguments
          (format t "~A ~A~%" (convert (parse-integer amount) from to) to))
        (progn
          (write-line "Usage: units INTEGER FROM-UNIT TO-UNIT" *error-output*)
          (zk:set-exit-code 2)))))
