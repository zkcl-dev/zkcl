;;;; Prefix each line of a job log through the Common Lisp stream interface.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-gray)
  (require :zk-test))

(defpackage #:gray-streams-example
  (:use #:cl)
  (:local-nicknames (#:gray #:zk-gray)))
(in-package #:gray-streams-example)

(defclass prefixed-stream (gray:fundamental-character-output-stream)
  ((sink :initarg :sink :reader stream-sink)
   (prefix :initarg :prefix :reader stream-prefix)
   (column :initform 0 :accessor stream-column)))

(defmethod gray:stream-line-column ((stream prefixed-stream))
  (stream-column stream))

(defmethod gray:stream-write-char ((stream prefixed-stream) character)
  ;; Blank lines stay blank. Count only the characters written by the caller.
  (let ((sink (stream-sink stream)))
    (if (char= character #\Newline)
        (setf (stream-column stream) 0)
        (progn
          (when (zerop (stream-column stream))
            (write-string (stream-prefix stream) sink))
          (incf (stream-column stream))))
    (write-char character sink)))

(defun write-job-log (stream)
  (format stream "Starting ~A" "index")
  (fresh-line stream)
  (fresh-line stream)
  (format stream "Processed ~D files~%" 3)
  (terpri stream)
  (write-line "Done" stream))

(defun capture-job-log ()
  (let ((sink (make-string-output-stream)))
    (with-open-stream (log (make-instance 'prefixed-stream :prefix "[index] " :sink sink))
      (write-job-log log))
    ;; LOG borrows SINK. Closing the wrapper leaves the sink available.
    (get-output-stream-string sink)))

(zk-test:deftest ordinary-output-operations ()
  (zk-test:is-equal (format nil "[index] Starting index~%[index] Processed 3 files~%~%[index] Done~%")
                    (capture-job-log)))

(zk-test:deftest wrapper-lifetime ()
  (let* ((sink (make-string-output-stream))
         (log (make-instance 'prefixed-stream :prefix "job " :sink sink)))
    (zk-test:is (output-stream-p log))
    (close log)
    (zk-test:is (open-stream-p sink))
    (zk-test:signals stream-error (write-char #\x log))))

(zk-test:defsuite gray-streams-example ordinary-output-operations wrapper-lifetime)

(let ((report (zk-test:run 'gray-streams-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(terpri)
(write-string (capture-job-log))
