(in-package #:examples.units)

(defun seconds-per-unit (name)
  (or (cdr (assoc name *units* :test #'string-equal))
      (error "Unknown time unit ~S." name)))

;;! Convert a duration between unit names from the generated table.
;;!
;;! ```lisp test
;;! (assert (= 3/2 (convert 90 "s" "min")))
;;! ```
(defun convert (amount from to)
  (* amount (/ (seconds-per-unit from) (seconds-per-unit to))))
