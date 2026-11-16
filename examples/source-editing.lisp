;;;; Repair an incomplete Lisp buffer and wrap one form while retaining trivia.
;;;; Every range and selection is in UTF-8 bytes in its owning snapshot.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-syntax)
  (require :zk-test))

(defpackage #:source-editing-example
  (:use #:cl)
  (:local-nicknames (#:source #:zk-syntax)))
(in-package #:source-editing-example)

(defparameter *draft* (format nil "(emit 雪 ; keep this comment~%"))

(defun close-list-at-end (snapshot)
  (let ((end (source:byte-offset snapshot (length (source:text snapshot)))))
    (source:make-edit snapshot (list (list end end ")")) (1+ end) (1+ end))))

(defun node-at-character (snapshot index)
  (source:node-at snapshot (source:byte-offset snapshot index)))

(defun wrap-form-at (snapshot index)
  (multiple-value-bind (start end) (source:range (node-at-character snapshot index))
    (source:structural-edit snapshot start end :wrap)))

(zk-test:deftest source-coordinates ()
  (let* ((snapshot (source:parse-source *draft*))
         (name (node-at-character snapshot 6)))
    (zk-test:is (getf (first (source:diagnostics snapshot)) :needs-more-input))
    (zk-test:is-values (source:range name) 6 9)
    (zk-test:is= 7 (source:character-index snapshot 9))
    (zk-test:is-equal "雪" (source:node-text name))))

(zk-test:deftest snapshot-bound-edits ()
  (let* ((workspace (source:make-workspace))
         (draft (source:set-document workspace "example.lisp" *draft*))
         (saved-name (node-at-character draft 6))
         (close (close-list-at-end draft))
         (repaired (source:apply-edit-to-document workspace "example.lisp" close))
         (wrap (wrap-form-at repaired 6)))
    (zk-test:is-eq nil (source:diagnostics repaired))
    (zk-test:is-equal (format nil "(emit (雪) ; keep this comment~%)")
                      (source:apply-edit wrap))
    (zk-test:is (source:snapshot-equal-p repaired (source:snapshot workspace "example.lisp")))
    (let ((result (source:apply-edit-to-document workspace "example.lisp" wrap)))
      (zk-test:is-eq nil (source:diagnostics result))
      (zk-test:is-equal "雪" (source:node-text saved-name))
      (zk-test:is (source:snapshot-equal-p draft (source:node-snapshot saved-name)))
      (let ((condition (zk-test:signals source:edit-error
                         (source:apply-edit-to-document workspace "example.lisp" close))))
        (zk-test:is-eq :stale-snapshot (source:edit-error-reason condition)))
      (zk-test:is (source:snapshot-equal-p result (source:snapshot workspace "example.lisp"))))))

(zk-test:defsuite source-editing-example source-coordinates snapshot-bound-edits)

(let ((report (zk-test:run 'source-editing-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(let* ((workspace (source:make-workspace))
       (draft (source:set-document workspace "example.lisp" *draft*))
       (diagnostic (first (source:diagnostics draft)))
       (repaired (source:apply-edit-to-document workspace "example.lisp" (close-list-at-end draft)))
       (wrap (wrap-form-at repaired 6)))
  (format t "~%Draft~%~A" (source:text draft))
  (format t "  ~A at bytes [~D, ~D)~%" (getf diagnostic :message)
          (getf diagnostic :start) (getf diagnostic :end))
  (multiple-value-bind (anchor head) (source:edit-selection wrap)
    (format t "~%Wrapped form, selection [~D, ~D)~%~A~%"
            anchor head (source:apply-edit wrap))))
