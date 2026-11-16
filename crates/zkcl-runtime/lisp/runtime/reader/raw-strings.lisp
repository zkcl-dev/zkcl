(in-package #:cl-user)

(eval-when (:compile-toplevel :load-toplevel :execute)
  (unless (find-package "ZK")
    (make-package "ZK" :use nil)))

(eval-when (:compile-toplevel :load-toplevel :execute)
  (export '(zk::in-readtable zk::make-raw-strings-readtable
            zk::call-with-raw-strings)
          "ZK"))

(defun zk::%raw-string-continuation-p (stream)
  ;; A different # dispatch may start the next form. Save a position because
  ;; the Common Lisp stream protocol guarantees only one character of pushback.
  (let ((position (file-position stream)))
    (unless position
      (error 'stream-error :stream stream))
    (loop for character = (read-char stream nil nil t)
          while (or (eql character #\Space) (eql character #\Tab))
          finally
             (return
               (cond ((and (eql character #\#)
                           (eql (read-char stream nil nil t) #\>))
                      t)
                     (t
                      (unless (file-position stream position)
                        (error 'stream-error :stream stream))
                      nil))))))

(defun zk::%read-raw-string (stream sub-character argument)
  (declare (ignore sub-character))
  (when (and argument (not *read-suppress*))
    (error 'reader-error :stream stream))
  (let ((text
          (with-output-to-string (out)
            (loop
              (multiple-value-bind (line eof-p) (read-line stream nil nil t)
                (unless line (return))
                (let ((end (length line)))
                  (when (and (not eof-p) (plusp end)
                             (char= (char line (1- end)) #\Return))
                    (decf end))
                  (write-string line out :end end))
                (when (or eof-p (not (zk::%raw-string-continuation-p stream)))
                  (return))
                (terpri out))))))
    (unless *read-suppress* text)))

(defun zk:make-raw-strings-readtable ()
  "Return a fresh standard readtable extended with #> raw string lines."
  (let ((table (copy-readtable nil)))
    (set-dispatch-macro-character #\# #\> #'zk::%read-raw-string table)
    table))

(defmacro zk:in-readtable (name)
  "Select :RAW-STRINGS or :STANDARD for subsequently read forms."
  (let ((constructor (ecase name
                        (:raw-strings '(zk:make-raw-strings-readtable))
                        (:standard '(copy-readtable nil)))))
    `(eval-when (:compile-toplevel :load-toplevel :execute)
       (setf *readtable* ,constructor))))

(defun zk:call-with-raw-strings (thunk)
  "Call THUNK with a fresh #> readtable, for example as an ASDF :AROUND-COMPILE hook."
  (let ((*readtable* (zk:make-raw-strings-readtable)))
    (funcall thunk)))
