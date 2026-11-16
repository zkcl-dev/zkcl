;;;; Register the port and configure the original systems in ASDF before loading.

(asdf:load-system "sqlite")

(let ((database (sqlite:connect ":memory:")))
  (unwind-protect
       (progn
         (sqlite:execute-non-query database "create table samples (n integer, s text)")
         (sqlite:execute-non-query database "insert into samples values (?, ?)"
                                   4294967297 "中文 SQLite")
         (assert (equal (sqlite:execute-to-list database "select n, s from samples")
                        '((4294967297 "中文 SQLite")))))
    (sqlite:disconnect database)))

(format t "~&cl-sqlite create/insert/select/close passed.~%")

;; SQLite retains this function pointer between separate SQL statements.
#+zkcl
(progn
  (defun install-zkcl-sqlite-scalar (value)
    (cffi:defcallback zkcl-scalar :void ((context :pointer) (argc :int) (argv :pointer))
      (declare (ignore argc argv))
      (cffi:foreign-funcall "sqlite3_result_int" :pointer context :int value :void)))
  (install-zkcl-sqlite-scalar 17)
  (let ((database (sqlite:connect ":memory:"))
        (null (cffi:null-pointer)))
    (unwind-protect
         (progn
           (assert (zerop (cffi:foreign-funcall "sqlite3_create_function"
                            :pointer (sqlite::handle database) :string "zkcl_scalar"
                            :int 0 :int 1 :pointer null :pointer (cffi:callback zkcl-scalar)
                            :pointer null :pointer null :int)))
           (assert (= 17 (sqlite:execute-single database "select zkcl_scalar()")))
           (install-zkcl-sqlite-scalar 42)
           (assert (= 42 (sqlite:execute-single database "select zkcl_scalar()"))))
      (sqlite:disconnect database)))
  (format t "~&SQLite retained callback saw the replacement definition.~%"))
