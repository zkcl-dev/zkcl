;;;; Decode records with an ASCII size prefix and a packed binary body.
;;;; Body layout is a four-bit magic value, four-bit kind, signed priority
;;;; delta, little-endian text size, and that many UTF-8 bytes.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

(defpackage #:binary-frames-example
  (:use #:cl)
  (:local-nicknames (#:lex #:zk-lex) (#:text #:zk-text) (#:bits #:zk-bits)))
(in-package #:binary-frames-example)

(define-condition invalid-frame (error)
  ((reason :initarg :reason :reader frame-error-reason)
   (offset :initarg :offset :reader frame-error-offset))
  (:report (lambda (condition stream)
             (format stream "Invalid frame at byte ~D (~A)"
                     (frame-error-offset condition) (frame-error-reason condition)))))

(defparameter *size-plan*
  (lex:compile-lex-rules
   '((:seq (:capture digits (:repeat 1 3 (:range #x30 #x39))) (:octets #x3A)))
   :domain :bytes :mode :longest-prefix))

(defstruct frame kind priority-delta payload)

(defun decode-body (body)
  (zk:match body
    ((bits:bits (u4be #xD) (u4be kind) (i8be delta) (u16le size)
                (:bytes size payload :as :span))
     (make-frame :kind kind :priority-delta delta :payload payload))
    (_ (error 'invalid-frame :reason :body :offset (text:span-start body)))))

(defun read-size (scanner on-pause)
  (loop
    (multiple-value-bind (status match) (lex:scan-step scanner *size-plan*)
      (ecase status
        (:match (return (parse-integer (text:utf8-decode (lex:lex-capture match 'digits)))))
        (:eof (return nil))
        (:need-more (when on-pause (funcall on-pause :size (lex:scan-position scanner))))
        (:miss (error 'invalid-frame :reason :size :offset (lex:scan-position scanner)))))))

(defun read-frame (scanner &optional on-pause)
  (let ((size (read-size scanner on-pause)))
    (when size
      (loop
        (multiple-value-bind (status body) (lex:scan-take scanner size)
          (ecase status
            (:ok (return (decode-body body)))
            (:need-more (when on-pause (funcall on-pause :body (lex:scan-position scanner))))
            (:truncated (error 'invalid-frame :reason :truncated
                                             :offset (lex:scan-position scanner)))))))))

(defun frame-row (frame)
  (let ((decoded (text:utf8-decode (frame-payload frame) :as :span)))
    (multiple-value-bind (source start end) (text:span-origin-byte-range decoded)
      (declare (ignore source))
      (list (frame-kind frame) (frame-priority-delta frame)
            (text:span-value decoded) start end))))

(defun chunked-frames ()
  ;; 7: D2 FD 03 00 E9 9B AA    4: D1 00 00 00
  (let ((steps (list (bits:octets #x37) :need-more
                     (bits:octets #x3A #xD2 #xFD 3 0 #xE9) :need-more
                     (bits:octets #x9B #xAA #x34 #x3A #xD1 0 0 0))))
    (lex:make-lexscanner
     (lambda ()
       (let ((step (pop steps)))
         (cond ((eq step :need-more) (values nil :need-more))
               (step (values step :chunk))
               (t (values nil :eof)))))
     :domain :bytes)))

(zk-test:deftest chunked-read ()
  (let ((scanner (chunked-frames))
        (pauses nil))
    (flet ((record-pause (phase position) (push (list phase position) pauses)))
      (zk-test:is-equal '(2 -3 "雪" 6 9) (frame-row (read-frame scanner #'record-pause)))
      (zk-test:is-equal '(1 0 "" 15 15) (frame-row (read-frame scanner #'record-pause)))
      (zk-test:is-eq nil (read-frame scanner #'record-pause))
      (zk-test:is-equal '((:size 0) (:body 2)) (nreverse pauses)))))

(zk-test:deftest payload-view ()
  (let* ((body (text:snapshot (bits:octets #xD2 #xFD 3 0 #xE9 #x9B #xAA)))
         (payload (frame-payload (decode-body body))))
    (zk-test:is-eq (text:span-source body) (text:span-source payload))
    (zk-test:is= 4 (text:span-start payload))))

(zk-test:deftest malformed-frames ()
  (flet ((read-bytes (&rest bytes)
           (read-frame (lex:make-lexscanner (apply #'bits:octets bytes) :domain :bytes))))
    (let ((condition (zk-test:signals invalid-frame (read-bytes #x37 #x3A #xD2))))
      (zk-test:is-eq :truncated (frame-error-reason condition))
      (zk-test:is= 2 (frame-error-offset condition)))
    ;; This complete body claims one text byte but contains none.
    (let ((condition (zk-test:signals invalid-frame
                       (read-bytes #x34 #x3A #xD2 0 1 0))))
      (zk-test:is-eq :body (frame-error-reason condition)))
    (let ((condition (zk-test:signals invalid-frame (read-bytes #x37))))
      (zk-test:is-eq :size (frame-error-reason condition))
      (zk-test:is= 0 (frame-error-offset condition)))))

(zk-test:defsuite binary-frames-example chunked-read payload-view malformed-frames)

(let ((report (zk-test:run 'binary-frames-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(format t "~%Read two frames from a chunked byte source~%")
(flet ((show-pause (phase position)
         (format t "  waiting for ~A at byte ~D~%" phase position)))
  (let ((scanner (chunked-frames)))
    (loop for frame = (read-frame scanner #'show-pause)
          while frame
          do (destructuring-bind (kind delta text start end) (frame-row frame)
               (format t "  kind ~D, priority delta ~D, text ~S from bytes [~D, ~D)~%"
                       kind delta text start end)))))
