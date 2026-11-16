(in-package #:sqlite-example)

(defvar *test-database*)
(defvar *test-rows*)

(defun check-test-row (context count values names)
  (assert (= (zk-ffi:mem-ref context :int) 73))
  (assert (equal (query *test-database* "SELECT 7 AS nested")
                 '(("nested") ("7"))))
  (push (list (read-columns count names) (read-columns count values)) *test-rows*)
  0)

(defun test-row-callback (context count values names)
  (collect-in-sqlite-callback)
  (check-test-row context count values names))

(defun test-reentrant-callbacks ()
  (call-with-database
   (lambda (database)
     (let ((*test-database* database)
           (context (zk-ffi:allocate (zk-ffi:type-size :int))))
       (unwind-protect
           (let ((callback (zk-ffi:make-callback #'test-row-callback
                                                 :int :pointer :int :pointer :pointer)))
             (setf (zk-ffi:mem-ref context :int) 73)
             (collect-in-sqlite-callback)
             (dotimes (round 4)
               (let ((*test-rows* nil))
                 (execute-sql database
                              "SELECT NULL AS \"值\" UNION ALL SELECT '你好' ORDER BY 1"
                              callback context)
                 (assert (equal (reverse *test-rows*)
                                '((("值") (nil)) (("值") ("你好"))))))))
         (zk-ffi:free context))))))

(define-condition callback-failed (error) ())

(defun test-callback-error ()
  (let ((expected (make-condition 'callback-failed)))
    (assert
     (eq expected
         (handler-case
             (progn
               (call-with-database
                (lambda (database)
                  (let ((callback
                          (zk-ffi:make-callback
                           (lambda (context count values names)
                             (declare (ignore context count values names))
                             (collect-in-sqlite-callback)
                             (error expected))
                           :int :pointer :int :pointer :pointer)))
                    (execute-sql database "SELECT 1" callback))))
               nil)
           (callback-failed (condition) condition))))))

(defun test-sqlite-errors ()
  (call-with-database
   (lambda (database)
     (let ((calls 0))
       (assert
        (handler-case
            (let ((callback
                    (zk-ffi:make-callback
                     (lambda (context count values names)
                       (declare (ignore context count values names))
                       (incf calls)
                       1)
                     :int :pointer :int :pointer :pointer)))
              (execute-sql database "SELECT 1 UNION ALL SELECT 2" callback)
              nil)
          (simple-error (condition)
            (search "sqlite3_exec returned 4" (princ-to-string condition)))))
       (assert (= calls 1)))
     (assert
      (handler-case
          (progn (execute-sql database "SELECT * FROM missing_sqlite_test_table") nil)
        (simple-error (condition)
          (search "missing_sqlite_test_table" (princ-to-string condition)))))
     (assert (equal (query database "SELECT 42 AS answer") '(("answer") ("42")))))))
