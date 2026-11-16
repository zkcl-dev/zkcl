;;;; Cache grapheme counts for live label objects with a weak-key hash table.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

(defpackage #:weak-cache-example
  (:use #:cl))
(in-package #:weak-cache-example)

(defstruct label
  (text "" :type string :read-only t))

(defun grapheme-count (cache label)
  (multiple-value-bind (cached present-p) (gethash label cache)
    (if present-p
        (values cached :cached)
        (values (setf (gethash label cache)
                      (length (zk-text:segment-text (label-text label) :grapheme)))
                :computed))))

(defun add-temporary-label (cache)
  (let ((temporary (make-label :text "Draft")))
    (grapheme-count cache temporary)
    (values (hash-table-count cache) (zk:make-weak-pointer temporary))))

(defun release-label ()
  (let ((cache (make-hash-table :test #'eq :weakness :key))
        (retained (make-label :text "é")))
    (grapheme-count cache retained)
    (multiple-value-bind (before observer) (add-temporary-label cache)
      ;; TEMPORARY has left scope. Its cache entry does not keep it alive.
      (zk:gc)
      (multiple-value-bind (count origin) (grapheme-count cache retained)
        (values before (hash-table-count cache)
                (nth-value 1 (zk:weak-pointer-value observer)) count origin)))))

(zk-test:deftest cache-follows-label-lifetime ()
  (zk-test:is-values (release-label) 2 1 nil 1 :cached))

(zk-test:defsuite weak-cache-example cache-follows-label-lifetime)

(let ((report (zk-test:run 'weak-cache-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(multiple-value-bind (before after temporary-live-p graphemes origin) (release-label)
  (format t "~%Cache entries before and after collection  ~D -> ~D~%" before after)
  (format t "  Temporary label live ~S~%" temporary-live-p)
  (format t "  Retained label has ~D grapheme (~A)~%" graphemes origin))
