;;;; Call SQLite's C API and read rows through a Lisp callback.
;;;; Set ZKCL_SQLITE_LIBRARY to use a particular SQLite shared library.

#-zk-ffi
(eval-when (:compile-toplevel :load-toplevel :execute)
  (error "The SQLite example requires zkcl-cli built with the ffi feature."))

(defpackage #:sqlite-example
  (:use #:cl))
(in-package #:sqlite-example)

(zk:in-readtable :raw-strings)

(defparameter *sample-query*
  #>SELECT 1 AS n, 'λ' AS "字", NULL AS optional
  #>UNION ALL SELECT 2, '雪', '' ORDER BY n;
  )

(zk:in-readtable :standard)

(defun open-sqlite-library ()
  (let ((override (zk:getenv "ZKCL_SQLITE_LIBRARY")))
    (when override
      (return-from open-sqlite-library (zk-ffi:open-library override))))
  (let ((candidates
          (cond ((member :darwin *features*)
                 '("/usr/lib/libsqlite3.dylib" "libsqlite3.dylib"))
                ((member :unix *features*)
                 '("libsqlite3.so.0" "libsqlite3.so"))
                ((member :windows *features*)
                 ;; The Windows system build uses stdcall on 32-bit x86.
                 (if (member :64-bit *features*)
                     '("sqlite3.dll" "winsqlite3.dll")
                     '("sqlite3.dll"))))))
    (dolist (path candidates)
      (let ((library (ignore-errors (zk-ffi:open-library path))))
        (when library (return-from open-sqlite-library library))))
    (error "Could not load SQLite from ~S. Set ZKCL_SQLITE_LIBRARY to a SQLite shared library."
           candidates)))

(defparameter *sqlite* (open-sqlite-library))

;; After the C symbol name, give the result type followed by the argument types.
(defparameter *sqlite-open*
  (zk-ffi:bind-function *sqlite* "sqlite3_open" :int :cstring :pointer))
(defparameter *sqlite-close*
  (zk-ffi:bind-function *sqlite* "sqlite3_close" :int :pointer))
(defparameter *sqlite-exec*
  (zk-ffi:bind-function *sqlite* "sqlite3_exec"
                        :int :pointer :cstring :pointer :pointer :pointer))
(defparameter *sqlite-errmsg*
  (zk-ffi:bind-function *sqlite* "sqlite3_errmsg" :cstring :pointer))

(defun check-status (status database operation)
  (unless (zerop status)
    (error "~A returned ~D: ~A" operation status
           (if database
               (funcall *sqlite-errmsg* database)
               "SQLite did not return a database handle"))))

(defun call-with-database (function)
  "Open a temporary database and close it when FUNCTION exits."
  (let ((database nil))
    (unwind-protect
        (progn
          (zk-ffi:with-foreign-memory (handle-slot (zk-ffi:type-size :pointer))
            ;; sqlite3_open writes its database pointer to the sqlite3 ** argument.
            (let ((status (funcall *sqlite-open* ":memory:" handle-slot)))
              ;; sqlite3_open can return a handle even on failure.
              (setf database (zk-ffi:mem-ref handle-slot :pointer))
              (check-status status database "sqlite3_open")))
          (funcall function database))
      (when database
        (check-status (funcall *sqlite-close* database) database "sqlite3_close")))))

(defun execute-sql (database sql &optional callback context)
  ;; Use sqlite3_errmsg for errors, so SQLite need not allocate a separate message.
  (check-status (funcall *sqlite-exec* database sql callback context nil)
                database "sqlite3_exec"))

(defun read-columns (count array)
  (let ((stride (zk-ffi:type-size :pointer)))
    (loop for i below count
          for text = (zk-ffi:mem-ref array :pointer (* i stride))
          collect (when text (zk-ffi:read-c-string text)))))

(defun query (database sql)
  "Return the column names followed by the rows, with SQL NULL represented by NIL."
  (let ((columns nil)
        (rows nil))
    (flet ((collect-row (context count values names)
             (declare (ignore context))
             ;; SQLite lends these arrays to the callback for one row. Copy
             ;; the strings into Lisp before returning SQLITE_OK (0).
             (unless columns (setf columns (read-columns count names)))
             (push (read-columns count values) rows)
             0))
      (let ((callback (zk-ffi:make-callback #'collect-row
                                            :int :pointer :int :pointer :pointer)))
        (execute-sql database sql callback)))
    (cons columns (nreverse rows))))

(defun show-query (database)
  (destructuring-bind (columns &rest rows) (query database *sample-query*)
    (write-line "Rows from the SQLite callback")
    (format t "~{~A~^ | ~}~%" columns)
    (dolist (row rows)
      (destructuring-bind (number text optional) row
        (format t "~A | ~A | ~A~%" number text
                (if optional (prin1-to-string optional) "NULL"))))))

(call-with-database #'show-query)
