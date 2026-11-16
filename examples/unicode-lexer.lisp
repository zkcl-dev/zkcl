;;;; Tokenize a rule as UTF-8 arrives and retain positions in the original bytes.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

(defpackage #:unicode-lexer-example
  (:use #:cl)
  (:local-nicknames (#:lex #:zk-lex) (#:text #:zk-text)))
(in-package #:unicode-lexer-example)

(zk:in-readtable :raw-strings)

(defparameter *rule*
  #>if ifx λ=="é\q"
  )

(zk:in-readtable :standard)

(zk:defpattern identifier ()
  '(:seq (:property :xid-start) (:* (:property :xid-continue))))

(defun identifier-key (spelling)
  "Validate a complete identifier and return its case-insensitive lookup key."
  (lex:lexmatch spelling :full
    ((identifier) (text:normalize (text:case-fold spelling) :nfc))
    (_ nil)))

(defun next-token (scanner)
  ;; The scanner takes the longest token. IF wins ties, and IFX stays an identifier.
  (lex:lexscan scanner
    ((:capture span "if" :as :span) (values :if span))
    ((:capture span (identifier) :as :span) (values :identifier span))
    ((:capture span "=" :as :span) (values :assign span))
    ((:capture span "==" :as :span) (values :equals span))
    ((:seq "\"" (:capture contents (:* (:re "[^\"]")) :as :span) "\"")
     (values :string contents))
    ((:re "\\s+") :space)
    ((:need-more) :need-more)
    ((:eof) :eof)))

(defun chunk-source (bytes &rest cuts)
  "Supply byte chunks with a :NEED-MORE pause at each cut."
  (let ((start 0)
        (ends (append cuts (list (length bytes))))
        (paused nil))
    (lambda ()
      (cond (paused (setf paused nil) (values nil :need-more))
            (ends (let* ((end (pop ends))
                         (chunk (subseq bytes start end)))
                    (setf start end paused ends)
                    (values chunk :chunk)))
            (t (values nil :eof))))))

(defun tokenize (source &optional on-pause)
  (let ((scanner (lex:make-lexscanner (text:utf8-source source)))
        (tokens nil))
    (loop
      (multiple-value-bind (kind span) (next-token scanner)
        (case kind
          (:eof (return (nreverse tokens)))
          (:space nil)
          (:need-more (when on-pause (funcall on-pause (lex:scan-position scanner))))
          (otherwise (push (cons kind span) tokens)))))))

(defun token-row (token)
  (let ((span (cdr token)))
    (multiple-value-bind (origin byte-start byte-end) (text:span-origin-byte-range span)
      (declare (ignore origin))
      (list (car token) (text:span-value span)
            (text:span-start span) (text:span-end span) byte-start byte-end))))

(zk-test:deftest byte-boundaries ()
  (let ((bytes (text:utf8-encode *rule*))
        (expected '((:if "if" 0 2 0 2)
                    (:identifier "ifx" 3 6 3 6)
                    (:identifier "λ" 7 8 7 9)
                    (:equals "==" 8 10 9 11)
                    (:string "é\\q" 11 15 12 17))))
    (zk-test:is-equal expected (mapcar #'token-row (tokenize (chunk-source bytes))))
    (loop for cut from 1 below (length bytes)
          do (zk-test:is-equal expected
                               (mapcar #'token-row (tokenize (chunk-source bytes cut)))))))

(zk-test:deftest unicode-keys ()
  (zk-test:is-equal "strasse" (identifier-key "Straße"))
  (zk-test:is-equal "é" (identifier-key "é"))
  (zk-test:is-eq nil (identifier-key "if!"))
  (zk-test:is= 1 (length (text:segment-text "é" :grapheme))))

(zk-test:deftest source-errors ()
  (let* ((error (zk-test:signals text:unicode-decode-error
                  (tokenize (chunk-source (zk-bits:octets #x41 #xE1 #x80 #x42) 3))))
         (bad (text:unicode-decode-error-span error)))
    (zk-test:is= 1 (text:span-start bad))
    (zk-test:is= 3 (text:span-end bad)))
  (let ((error (zk-test:signals lex:scan-error
                 (tokenize (chunk-source (text:utf8-encode "if @"))))))
    (zk-test:is-eq :miss (lex:scan-error-reason error))
    (zk-test:is= 3 (lex:scan-error-position error))))

(zk-test:defsuite unicode-lexer-example byte-boundaries unicode-keys source-errors)

(let ((report (zk-test:run 'unicode-lexer-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(format t "~%Rule ~A~%" *rule*)
;; The cuts are inside λ and ==, respectively.
(let ((tokens (tokenize (chunk-source (text:utf8-encode *rule*) 8 10)
                        (lambda (position) (format t "  need more at character ~D~%" position)))))
  (dolist (token tokens)
    (destructuring-bind (kind spelling start end byte-start byte-end) (token-row token)
      (format t "  ~A ~S  characters [~D, ~D)  bytes [~D, ~D)~%"
              kind spelling start end byte-start byte-end))))
(format t "  identifier key for Straße is ~S~%" (identifier-key "Straße"))
