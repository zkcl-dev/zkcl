;;;; Import percentage settings using a caller-selected recovery policy.
;;;; The declaration chooses the policy at macro expansion. Restarts let the
;;;; caller supply a replacement or omit the entry while the reader is active.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

(defpackage #:configuration-example
  (:use #:cl))
(in-package #:configuration-example)

(define-condition invalid-percent (error)
  ((input :initarg :input :reader invalid-percent-input))
  (:report (lambda (condition stream)
             (format stream "Expected a percentage from 0 to 100, got ~S."
                     (invalid-percent-input condition)))))

(defun read-percent (input)
  (let ((number (handler-case (parse-integer input)
                  (parse-error () nil))))
    (if (typep number '(integer 0 100))
        number
        (restart-case (error 'invalid-percent :input input)
          (use-value (replacement)
            :report "Use another percentage."
            (check-type replacement (integer 0 100))
            replacement)
          (skip-entry ()
            :report "Omit this setting."
            (values))))))

(zk:define-declaration invalid-percent-policy (spec environment)
  (declare (ignore environment))
  (destructuring-bind (name policy) spec
    (check-type policy (member :strict :default :skip))
    (values :declare (cons name policy))))

(defmacro import-percent (input &environment environment)
  (ecase (or (zk:declaration-information 'invalid-percent-policy environment) :strict)
    (:strict `(read-percent ,input))
    (:default `(handler-bind ((invalid-percent (lambda (condition) (use-value 50 condition))))
                 (read-percent ,input)))
    (:skip `(handler-bind ((invalid-percent
                             (lambda (condition)
                               (invoke-restart (find-restart 'skip-entry condition)))))
              (read-percent ,input)))))

(defun import-presets ()
  (declare (invalid-percent-policy :default))
  (multiple-value-call #'list
    (import-percent "24")
    (import-percent "off")
    (locally (declare (invalid-percent-policy :skip))
      (import-percent "110"))
    (import-percent "0")))

(zk-test:deftest strict-by-default ()
  (let ((condition (zk-test:signals invalid-percent (import-percent "110"))))
    (zk-test:is-equal "110" (invalid-percent-input condition))))

(zk-test:deftest lexical-policies ()
  (zk-test:is-equal '(24 50 0) (import-presets))
  (locally (declare (invalid-percent-policy :skip))
    (zk-test:is-values (import-percent "100") 100)
    (zk-test:is-values (import-percent "off"))))

(zk-test:defsuite configuration-example strict-by-default lexical-policies)

(let ((report (zk-test:run 'configuration-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(format t "~%Import 24, \"off\", 110, and 0~%")
(format t "  use 50 for \"off\", skip 110 -> ~S~%" (import-presets))
