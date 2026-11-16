;;;; Test definitions register at load time. Only runs execute test bodies.
(defpackage #:zk-test
  (:use #:cl)
  (:export #:deftest #:defsuite #:is #:is= #:is-eq #:is-eql #:is-equal #:is-equalp
           #:is-values #:signals #:signals-not #:finishes #:fail
           #:assertion-failure #:assertion-form #:assertion-file #:assertion-details
           #:list-tests #:run #:check #:print-report #:write-json-report #:write-junit-report
           #:report #:report-status #:report-data #:check-failed #:failure-report
           #:*run-options* #:*report-hook* #:*discover-only* #:*discovered-checks*))
(in-package #:zk-test)

(defstruct (report (:constructor make-report (status data))) status data)

(define-condition check-failed (error)
  ((report :initarg :report :reader failure-report))
  (:report (lambda (c s)
             (let ((r (failure-report c)))
               (format s "Test ~S: ~S" (getf (report-data r) :name) (report-status r))))))

(define-condition assertion-failure (error)
  ((form :initarg :form :reader assertion-form)
   (file :initarg :file :reader assertion-file)
   (details :initarg :details :initform nil :reader assertion-details))
  (:report (lambda (c s)
             (let ((details (assertion-details c)))
               (if details
                   (format s "Assertion ~S failed in ~A: ~S"
                           (assertion-form c) (assertion-file c) details)
                   (format s "Assertion ~S failed in ~A"
                           (assertion-form c) (assertion-file c)))))))

(defvar *test-registry* (make-hash-table :test 'eq))
(defvar *run-options* nil)
(defvar *report-hook* nil)
(defvar *discover-only* nil)
(defvar *discovered-checks* nil)
(defvar *value-formatter* nil)

(defun format-display-value (value)
  (if *value-formatter*
      (funcall *value-formatter* value)
      value))

(defstruct (entry (:constructor make-entry (name kind definition metadata)))
  name kind definition metadata)

(defun register-test-entry (name kind definition &optional metadata)
  (unless (and (symbolp name) (symbol-package name))
    (error "A registered test needs an interned name: ~S" name))
  (let ((old (gethash name *test-registry*)))
    (when (and old (not (eq (entry-kind old) kind)))
      (error "~S is already registered as ~S" name (entry-kind old)))
    (setf (gethash name *test-registry*) (make-entry name kind definition metadata)))
  name)

(defun entry-info (name kind)
  (let ((entry (gethash name *test-registry*)))
    (and entry (eq (entry-kind entry) kind) (entry-metadata entry))))

(eval-when (:compile-toplevel :load-toplevel :execute)
  (defun current-test-file ()
    (let ((p (or *compile-file-truename* *load-truename*)))
      (and p (namestring p))))

  (defparameter *binary-assertion-operators*
    '(= /= < <= > >= eq eql equal equalp
      string= string/= string< string<= string> string>= string-equal string-not-equal
      char= char/= char< char<= char> char>= char-equal char-not-equal
      typep subtypep))

  (defun binary-assertion-form-p (expression)
    (and (consp expression)
         (member (car expression) *binary-assertion-operators* :test #'eq)
         (consp (cdr expression))
         (consp (cddr expression))
         (null (cdddr expression))))

  (defun split-body-declarations (body)
    (let ((doc nil)
          (decls nil))
      (when (and (stringp (car body)) (cdr body))
        (setq doc (pop body)))
      (loop while (and (consp (car body)) (eq (caar body) 'declare))
            do (push (pop body) decls))
      (values (nreverse decls) body doc))))

(defun fail-assertion (form file &optional details)
  (error 'assertion-failure :form form :file file :details details))

(defun fail (&optional datum &rest arguments)
  (let ((message (when datum
                   (if arguments
                       (apply #'format nil datum arguments)
                       (princ-to-string datum)))))
    (fail-assertion '(fail) (current-test-file) (when message (list :message message)))))

(defmacro is (expression)
  (let ((file (current-test-file)))
    (if (binary-assertion-form-p expression)
        (let ((op (car expression))
              (lhs (second expression))
              (rhs (third expression))
              (lv (gensym "LHS"))
              (rv (gensym "RHS")))
          `(let ((,lv ,lhs)
                 (,rv ,rhs))
             (or (,op ,lv ,rv)
                 (fail-assertion ',expression ,file
                                 (list :operator ',op :left ,lv :right ,rv)))))
        `(or ,expression
             (fail-assertion ',expression ,file)))))

(defmacro define-comparison-assertion (macro-name predicate)
  `(defmacro ,macro-name (expected actual)
     (let ((ev (gensym "EXPECTED"))
           (av (gensym "ACTUAL"))
           (file (current-test-file)))
       `(let ((,ev ,expected)
              (,av ,actual))
          (or (,',predicate ,ev ,av)
              (fail-assertion '(,',macro-name ,expected ,actual) ,file
                              (list :operator ',',predicate :expected ,ev :actual ,av)))))))

(define-comparison-assertion is= =)
(define-comparison-assertion is-eq eq)
(define-comparison-assertion is-eql eql)
(define-comparison-assertion is-equal equal)
(define-comparison-assertion is-equalp equalp)

(defmacro is-values (form &rest expected)
  (let ((actual (gensym "VALUES"))
        (exp (gensym "EXPECTED"))
        (file (current-test-file)))
    `(let ((,actual (multiple-value-list ,form))
           (,exp (list ,@expected)))
       (or (equal ,actual ,exp)
           (fail-assertion '(is-values ,form ,@expected) ,file
                           (list :expected ,exp :actual ,actual))))))

(defmacro signals (condition-type &body body)
  (let ((blk (gensym "SIGNALS"))
        (file (current-test-file)))
      `(block ,blk
         (handler-bind ((,condition-type (lambda (c) (return-from ,blk c))))
           ,@body)
         (fail-assertion '(signals ,condition-type ,@body) ,file
                         (list :expected-condition ',condition-type)))))

(defmacro signals-not (condition-type &body body)
  (let ((file (current-test-file)))
    `(handler-case (progn ,@body)
       (,condition-type (c)
         (fail-assertion '(signals-not ,condition-type ,@body) ,file
                         (list :unexpected-condition c))))))

(defmacro finishes (&body body)
  `(signals-not error ,@body))

(defmacro deftest (name () &body body)
  (multiple-value-bind (decls forms doc) (split-body-declarations body)
    `(register-test-entry ',name :test
       (lambda ()
         ,@(when doc (list doc))
         ,@decls
         (block ,name ,@forms)))))

(defmacro defsuite (name &rest members)
  `(register-test-entry ',name :suite ',members))

(defun matches-filter-p (name filter)
  (cond
    ((null filter) t)
    ((symbolp filter) (eq name filter))
    ((functionp filter) (not (null (funcall filter name))))
    ((stringp filter)
     (let* ((pkg (symbol-package name))
            (single (if pkg
                        (format nil "~A:~A" (package-name pkg) (symbol-name name))
                        (symbol-name name)))
            (double (and pkg
                         (format nil "~A::~A" (package-name pkg) (symbol-name name)))))
       (or (not (null (search filter single :test #'char-equal)))
           (and double (not (null (search filter double :test #'char-equal)))))))
    (t (error "Invalid test filter ~S" filter))))

(defun collect-runnable-tests (root &optional filter)
  (let ((seen (make-hash-table :test 'eq)) (leaves nil))
    (labels ((visit (name ancestors)
               (when (member name ancestors) (error "Suite cycle through ~S" name))
               (unless (gethash name seen)
                 (let ((entry (gethash name *test-registry*)))
                   (unless entry (error "Unknown test or suite ~S" name))
                   (if (eq :suite (entry-kind entry))
                       (dolist (child (entry-definition entry)) (visit child (cons name ancestors)))
                       (when (matches-filter-p name filter)
                         (push entry leaves)))
                   (setf (gethash name seen) t)))))
      (visit root nil))
    (nreverse leaves)))

(defun list-tests (&optional (selection nil supplied) filter)
  "Return runnable names without executing a test. A suite preserves member order."
  (if (and supplied selection)
      (mapcar #'entry-name (collect-runnable-tests selection filter))
      (sort (loop for entry being the hash-values of *test-registry*
                  when (and (not (eq :suite (entry-kind entry)))
                            (matches-filter-p (entry-name entry) filter))
                    collect (entry-name entry))
            (lambda (a b)
              (let ((pa (package-name (symbol-package a))) (pb (package-name (symbol-package b))))
                (if (string= pa pb) (string< (symbol-name a) (symbol-name b)) (string< pa pb)))))))

(defun split-keyword-option (plist target-key)
  (let ((found nil)
        (found-p nil)
        (rest nil))
    (loop for tail on plist by #'cddr do
      (when (null (cdr tail))
        (error "Odd keyword argument list ~S" plist))
      (let ((k (car tail))
            (v (cadr tail)))
        (if (eq k target-key)
            (unless found-p (setq found v found-p t))
            (progn (push k rest) (push v rest)))))
    (values found (nreverse rest) found-p)))

(defun elapsed-ms-since (start)
  (let ((delta (- (get-internal-real-time) start)))
    (round (* 1000 delta) (max 1 internal-time-units-per-second))))

(defun with-elapsed-ms (report elapsed-ms)
  (let ((data (report-data report)))
    (if (getf data :elapsed-ms)
        report
        (make-report (report-status report) (append data (list :elapsed-ms elapsed-ms))))))

(defun run-test-entry (entry options)
  (let ((name (entry-name entry))
        (kind (entry-kind entry))
        (function (entry-definition entry))
        (start (get-internal-real-time)))
    (with-elapsed-ms
        (handler-case
            (if (eq kind :property)
                (apply function options)
                (progn (funcall function) (make-report :passed (list :name name))))
          (check-failed (c) (failure-report c))
          (assertion-failure (c)
            (make-report :falsified (list :name name :form (assertion-form c)
                                          :file (assertion-file c)
                                          :details (assertion-details c))))
          (error (c) (make-report :error (list :name name :condition c))))
      (elapsed-ms-since start))))

(defun status-severity (status)
  (case status
    (:passed 0)
    (:insufficient 1)
    (:falsified 2)
    (:unstable 3)
    (:error 4)
    (otherwise 5)))

(defun worst-suite-status (results)
  (let ((worst :passed))
    (dolist (r results worst)
      (let ((status (report-status r)))
        (when (> (status-severity status) (status-severity worst))
          (setq worst status))))))

(defun run (name &rest options)
  (let ((start (get-internal-real-time)))
    (handler-case
        (multiple-value-bind (filter clean-options)
            (split-keyword-option (append options *run-options*) :filter)
          (let* ((leaves (collect-runnable-tests name filter))
                 (results (loop for entry in leaves collect
                            (run-test-entry entry clean-options))))
            (if (eq :suite (entry-kind (gethash name *test-registry*)))
                (if results
                    (make-report (worst-suite-status results)
                                 (list :name name :results results
                                       :elapsed-ms (elapsed-ms-since start)))
                    (make-report :insufficient
                                 (list :name name :results nil
                                       :elapsed-ms (elapsed-ms-since start)
                                       :reason (if filter :filtered :empty-suite))))
                (or (first results)
                    (make-report :insufficient
                                 (list :name name :reason :filtered
                                       :elapsed-ms (elapsed-ms-since start)))))))
      (error (c) (make-report :error (list :name name :condition c
                                           :elapsed-ms (elapsed-ms-since start)))))))

(defun print-coverage-entries (entries random-count)
  (let ((sorted (sort (copy-list entries)
                      (lambda (a b)
                        (if (= (cdr a) (cdr b))
                            (string< (princ-to-string (car a)) (princ-to-string (car b)))
                            (> (cdr a) (cdr b)))))))
    (dolist (entry sorted)
      (let ((pct (if (plusp random-count)
                     (round (* 100 (cdr entry)) random-count)
                     0)))
        (format t "    ~5D  ~3D%  ~S~%" (cdr entry) pct (car entry))))))

(defun print-coverage-table (coverage random-count)
  (when coverage
    (let ((flat nil)
          (groups (make-hash-table :test 'equal))
          (group-order nil))
      (dolist (entry coverage)
        (if (consp (car entry))
            (let ((grp (caar entry))
                  (item (cons (cdar entry) (cdr entry))))
              (unless (gethash grp groups)
                (push grp group-order))
              (push item (gethash grp groups)))
            (push entry flat)))
      (when flat
        (format t "  coverage:~%")
        (print-coverage-entries flat random-count))
      (dolist (grp (sort group-order #'string< :key #'princ-to-string))
        (format t "  tabulate ~S:~%" grp)
        (print-coverage-entries (gethash grp groups) random-count)))))

(defun print-report (report)
  (let ((data (report-data report)) (*print-circle* t) (*print-level* 8) (*print-length* 32))
    (format t "~&~A ~S~%" (report-status report) (getf data :name))
    (let ((results (getf data :results)))
      (when results
        (let ((passed (count :passed results :key #'report-status)))
          (format t "  summary: ~D/~D passed~%" passed (length results)))))
    (when (getf data :seed)
      (if (getf data :counts)
          (format t "  seed=~D counts=~S~%" (getf data :seed) (getf data :counts))
          (format t "  seed=~D~%" (getf data :seed))))
    (print-coverage-table (getf data :coverage) (or (getf (getf data :counts) :random) 0))
    (when (eq (getf data :coverage-status) :not-run)
      (format t "  coverage: not run (0 random trials)~%"))
    (when (getf data :goal)
      (if (getf data :observed-count)
          (format t "  goal: ~S (observed ~D, required ~D)~%"
                  (getf data :goal) (getf data :observed-count) (getf data :required-count))
          (format t "  goal: ~S~%" (getf data :goal))))
    (when (getf data :reason) (format t "  reason: ~S~%" (getf data :reason)))
    (when (getf data :stop-reason) (format t "  stop-reason: ~S~%" (getf data :stop-reason)))
    (when (getf data :targets) (format t "  targets: ~S~%" (getf data :targets)))
    (when (getf data :saved-regression) (format t "  saved: ~A~%" (getf data :saved-regression)))
    (when (getf data :source)
      (if (getf data :index)
          (format t "  source: ~S (index ~D)~%" (getf data :source) (getf data :index))
          (format t "  source: ~S~%" (getf data :source))))
    (when (getf data :file) (format t "  file: ~A~%" (getf data :file)))
    (when (getf data :form) (format t "  assertion: ~S~%" (getf data :form)))
    (when (getf data :details) (format t "  details: ~S~%" (getf data :details)))
    (when (getf data :condition) (format t "  ~A~%" (getf data :condition)))
    (when (getf data :prior) (format t "  prior: ~A~%" (getf data :prior)))
    (cond
      ((eq (report-status report) :unstable)
       (format t "  initial: ~S, replay: ~S~%" (getf data :failure) (getf data :observed))
       (when (or (getf data :original-available) (getf data :original))
         (format t "  input ~S~%" (format-display-value (getf data :original)))))
      ((getf data :failure)
       (if (or (getf data :reduced-available) (getf data :persistable) (getf data :reduced))
           (let ((disp-reduced (format-display-value (getf data :reduced)))
                 (disp-original (format-display-value (getf data :original))))
             (format t "  ~S, input ~S~%" (getf data :failure) disp-reduced)
             (when (and (getf data :original-available)
                        (not (equal (getf data :original) (getf data :reduced))))
               (format t "  original: ~S~%" disp-original)))
           (format t "  ~S (unprintable/non-persistable input)~%" (getf data :failure)))))
    (when (getf data :context)
      (format t "  context: ~A~%" (getf data :context)))
    (dolist (n (getf data :notes))
      (format t "  note: ~A~%" n))
    (when (getf data :shrinks)
      (format t "  shrinks: ~D (~S)~%" (getf data :shrinks) (getf data :shrink-stop)))
    (let ((results (getf data :results)))
      (mapc #'print-report results)
      (when results
        (let ((failed (remove :passed results :key #'report-status)))
          (when failed
            (format t "  failed: ~{~S~^, ~}~%"
                    (mapcar (lambda (r) (getf (report-data r) :name)) failed))))))))

(defun safe-prin1 (value)
  (let ((*print-circle* t) (*print-level* 8) (*print-length* 32) (*print-pretty* nil))
    (prin1-to-string value)))

(defun qualified-symbol-string (name)
  (if (symbolp name)
      (let ((pkg (symbol-package name)))
        (if pkg
            (format nil "~A:~A" (package-name pkg) (symbol-name name))
            (symbol-name name)))
      (princ-to-string name)))

(defun write-json-string (text stream)
  (write-char #\" stream)
  (loop for c across text
        for code = (char-code c) do
          (case c
            (#\" (write-string "\\\"" stream))
            (#\\ (write-string "\\\\" stream))
            (#\Newline (write-string "\\n" stream))
            (#\Return (write-string "\\r" stream))
            (#\Tab (write-string "\\t" stream))
            (otherwise
             (if (< code 32)
                 (format stream "\\u~4,'0X" code)
                 (write-char c stream)))))
  (write-char #\" stream))

(defun write-json-counts (counts stream)
  (write-char #\{ stream)
  (let ((first t))
    (loop for (k v) on counts by #'cddr do
      (unless first (write-string ", " stream))
      (setq first nil)
      (write-json-string (string-downcase (symbol-name k)) stream)
      (format stream ": ~D" v)))
  (write-char #\} stream))

(defun write-json-coverage (coverage stream)
  (write-char #\[ stream)
  (let ((first t))
    (dolist (entry coverage)
      (unless first (write-string ", " stream))
      (setq first nil)
      (write-string "{\"label\": " stream)
      (write-json-string (safe-prin1 (car entry)) stream)
      (format stream ", \"count\": ~D}" (cdr entry))))
  (write-char #\] stream))

(defun write-json-report (report &optional (stream *standard-output*))
  (let ((data (report-data report))
        (first t))
    (flet ((field (key writer)
             (unless first (write-string ", " stream))
             (setq first nil)
             (write-json-string key stream)
             (write-string ": " stream)
             (funcall writer)))
      (write-char #\{ stream)
      (field "status" (lambda () (write-json-string (string-downcase (symbol-name (report-status report))) stream)))
      (field "name" (lambda () (write-json-string (qualified-symbol-string (getf data :name)) stream)))
      (when (getf data :elapsed-ms)
        (field "elapsed_ms" (lambda () (format stream "~D" (getf data :elapsed-ms)))))
      (when (getf data :seed)
        (field "seed" (lambda () (format stream "~D" (getf data :seed)))))
      (when (getf data :counts)
        (field "counts" (lambda () (write-json-counts (getf data :counts) stream))))
      (when (getf data :coverage)
        (field "coverage" (lambda () (write-json-coverage (getf data :coverage) stream))))
      (when (getf data :coverage-status)
        (field "coverage_status"
               (lambda () (write-json-string (string-downcase (symbol-name (getf data :coverage-status))) stream))))
      (when (getf data :source)
        (field "source" (lambda () (write-json-string (string-downcase (symbol-name (getf data :source))) stream))))
      (when (getf data :reason)
        (field "reason" (lambda () (write-json-string (string-downcase (symbol-name (getf data :reason))) stream))))
      (when (getf data :stop-reason)
        (field "stop_reason" (lambda () (write-json-string (string-downcase (symbol-name (getf data :stop-reason))) stream))))
      (when (getf data :targets)
        (field "targets"
               (lambda ()
                 (write-char #\{ stream)
                 (loop for entry in (getf data :targets) for t-first = t then nil do
                   (unless t-first (write-string ", " stream))
                   (write-json-string (safe-prin1 (car entry)) stream)
                   (if (integerp (cdr entry))
                       (format stream ": ~D" (cdr entry))
                       (format stream ": ~F" (float (cdr entry) 1.0d0))))
                 (write-char #\} stream))))
      (when (getf data :saved-regression)
        (field "saved_regression" (lambda () (write-json-string (namestring (getf data :saved-regression)) stream))))
      (when (getf data :file)
        (field "file" (lambda () (write-json-string (princ-to-string (getf data :file)) stream))))
      (when (getf data :form)
        (field "assertion" (lambda () (write-json-string (safe-prin1 (getf data :form)) stream))))
      (when (getf data :details)
        (field "details" (lambda () (write-json-string (safe-prin1 (getf data :details)) stream))))
      (when (getf data :condition)
        (field "condition" (lambda () (write-json-string (princ-to-string (getf data :condition)) stream))))
      (when (getf data :failure)
        (field "failure" (lambda () (write-json-string (safe-prin1 (getf data :failure)) stream))))
      (when (getf data :original-available)
        (field "original" (lambda () (write-json-string (safe-prin1 (format-display-value (getf data :original))) stream))))
      (when (or (getf data :reduced-available) (getf data :persistable) (getf data :reduced))
        (field "reduced" (lambda () (write-json-string (safe-prin1 (format-display-value (getf data :reduced))) stream))))
      (when (getf data :observed-count)
        (field "observed_count" (lambda () (format stream "~D" (getf data :observed-count)))))
      (when (getf data :required-count)
        (field "required_count" (lambda () (format stream "~D" (getf data :required-count)))))
      (when (getf data :shrinks)
        (field "shrinks" (lambda () (format stream "~D" (getf data :shrinks)))))
      (when (getf data :shrink-stop)
        (field "shrink_stop"
               (lambda () (write-json-string (string-downcase (symbol-name (getf data :shrink-stop))) stream))))
      (when (getf data :context)
        (field "context" (lambda () (write-json-string (princ-to-string (getf data :context)) stream))))
      (when (getf data :notes)
        (field "notes"
               (lambda ()
                 (write-char #\[ stream)
                 (loop for note in (getf data :notes) for note-first = t then nil do
                   (unless note-first (write-string ", " stream))
                   (write-json-string (princ-to-string note) stream))
                 (write-char #\] stream))))
      (let ((results (getf data :results)))
        (when results
          (field "results"
                 (lambda ()
                   (write-char #\[ stream)
                   (loop for child in results for child-first = t then nil do
                     (unless child-first (write-string ", " stream))
                     (write-json-report child stream))
                   (write-char #\] stream)))))
      (write-char #\} stream))
    report))

(defun write-xml-escaped (text stream)
  (loop for c across text do
    (case c
      (#\& (write-string "&amp;" stream))
      (#\< (write-string "&lt;" stream))
      (#\> (write-string "&gt;" stream))
      (#\" (write-string "&quot;" stream))
      (#\' (write-string "&apos;" stream))
      (otherwise (write-char c stream)))))

(defun report-failure-summary (report)
  (with-output-to-string (*standard-output*)
    (print-report report)))

(defun write-junit-testcase (report stream &optional suite-name)
  (let* ((data (report-data report))
         (status (report-status report))
         (name (qualified-symbol-string (getf data :name)))
         (classname (if suite-name (qualified-symbol-string suite-name) "zk-test"))
         (elapsed-sec (/ (float (or (getf data :elapsed-ms) 0) 1.0d0) 1000.0d0)))
    (format stream "  <testcase name=\"")
    (write-xml-escaped name stream)
    (format stream "\" classname=\"")
    (write-xml-escaped classname stream)
    (format stream "\" time=\"~,3F\">" elapsed-sec)
    (case status
      (:passed nil)
      (:insufficient
       (format stream "<skipped message=\"")
       (write-xml-escaped (safe-prin1 (or (getf data :reason) :insufficient)) stream)
       (format stream "\"/>"))
      (:error
       (format stream "<error type=\"error\" message=\"")
       (write-xml-escaped (princ-to-string (or (getf data :condition) "error")) stream)
       (format stream "\">")
       (write-xml-escaped (report-failure-summary report) stream)
       (format stream "</error>"))
      (otherwise
       (format stream "<failure type=\"")
       (write-xml-escaped (string-downcase (symbol-name status)) stream)
       (format stream "\" message=\"")
       (write-xml-escaped (safe-prin1 (or (getf data :failure) (getf data :form) status)) stream)
       (format stream "\">")
       (write-xml-escaped (report-failure-summary report) stream)
       (format stream "</failure>")))
    (format stream "</testcase>~%")))

(defun write-junit-report (report &optional (stream *standard-output*))
  (let* ((data (report-data report))
         (leaves (or (getf data :results) (list report)))
         (suite-name (getf data :name))
         (total (length leaves))
         (failures (count-if (lambda (r) (member (report-status r) '(:falsified :unstable))) leaves))
         (errors (count :error leaves :key #'report-status))
         (skipped (count :insufficient leaves :key #'report-status))
         (elapsed-sec (/ (float (or (getf data :elapsed-ms) 0) 1.0d0) 1000.0d0)))
    (format stream "<?xml version=\"1.0\" encoding=\"UTF-8\"?>~%")
    (format stream "<testsuite name=\"")
    (write-xml-escaped (qualified-symbol-string suite-name) stream)
    (format stream "\" tests=\"~D\" failures=\"~D\" errors=\"~D\" skipped=\"~D\" time=\"~,3F\">~%"
            total failures errors skipped elapsed-sec)
    (dolist (leaf leaves)
      (write-junit-testcase leaf stream suite-name))
    (format stream "</testsuite>~%")
    report))

(defun check (name &rest options)
  (when *discover-only*
    (multiple-value-bind (filter clean-options)
        (split-keyword-option (append options *run-options*) :filter)
      (declare (ignore clean-options))
      (collect-runnable-tests name filter)
      (push (list name options) *discovered-checks*)
      (return-from check (values t (make-report :passed (list :name name))))))
  (let ((report (apply #'run name options)))
    (when *report-hook* (funcall *report-hook* report))
    (if (eq (report-status report) :passed) (values t report)
        (error 'check-failed :report report))))

(provide "zk-test")
(provide "ZK-TEST")
