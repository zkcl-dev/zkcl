;;;; Modern Structural Pattern Matching showcase for ZK.
;;;; Run from the repository root with:
;;;;   ./target/release/zkcl run examples/pattern-matching.lisp

(defpackage #:pattern-matching-showcase
  (:use #:cl))
(in-package #:pattern-matching-showcase)

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

;;; ---------------------------------------------------------------------------
;;; 1. Typed closed variants (defvariant), exhaustive zk:match & constant fold
;;; ---------------------------------------------------------------------------

(zk:defvariant (expr (:predicate expr-p) (:conc-name expr-))
  (lit (value integer))
  (sym (name symbol))
  (add (left expr) (right expr))
  (neg (inner expr)))

(defun parse-expr (form)
  "Parse an integer, symbol, (+ a b), or (- x) form into EXPR."
  (zk:ematch form
    ((type integer value) (lit value))
    ((type symbol name) (sym name))
    ((list '+ left right) (add (parse-expr left) (parse-expr right)))
    ((list '- inner) (neg (parse-expr inner)))))

(defun simplify-add (left right)
  (declare (type expr left right))
  (zk:match (:inputs left right)
    (((lit 0) rhs) rhs)
    ((lhs (lit 0)) lhs)
    (((lit a) (lit b)) (lit (+ a b)))
    ((lhs rhs) :when (equalp lhs rhs) (add lhs rhs))
    (_ (add left right))))

(defun simplify-expr (e)
  "Exhaustively normalize an EXPR using declared closed variant coverage."
  (declare (type expr e))
  (zk:match e
    ((lit _) e)
    ((sym _) e)
    ((neg (neg inner)) (simplify-expr inner))
    ((neg (lit n)) (lit (- n)))
    ((neg inner)
     (let ((s (simplify-expr inner)))
       (zk:match (the expr s)
         ((lit n) (lit (- n)))
         ((neg x) x)
         ((or (sym _) (add _ _)) (neg s)))))
    ((add left right)
     (simplify-add (simplify-expr left) (simplify-expr right)))))

(defun format-expr (e)
  (declare (type expr e))
  (zk:match e
    ((lit v) v)
    ((sym s) s)
    ((neg inner) (list '- (format-expr inner)))
    ((add lhs rhs) (list '+ (format-expr lhs) (format-expr rhs)))))

(zk-test:deftest typed-adt-and-exhaustive-match ()
  (zk-test:is (expr-p (lit 3)))
  (zk-test:is= 3 (expr-value (lit 3)))
  (zk-test:signals type-error (lit :not-an-integer))
  (zk-test:signals type-error (neg 42))
  (zk-test:is-equal 'x (format-expr (simplify-expr (parse-expr '(+ 0 (+ x 0))))))
  (zk-test:is= 10 (format-expr (simplify-expr (parse-expr '(+ (+ 1 2) (+ 3 4))))))
  (zk-test:is-equal 'y (format-expr (simplify-expr (parse-expr '(- (- y))))))
  (zk-test:is= -5 (format-expr (simplify-expr (parse-expr '(- 5)))))
  (let ((err (zk-test:signals zk:match-error (parse-expr '(+ x)))))
    (zk-test:is-equal '((+ x)) (zk:match-error-values err))))

;;; ---------------------------------------------------------------------------
;;; 2. Pattern abstraction (defpattern), compile-time :const, and dynamic :=
;;; ---------------------------------------------------------------------------

(defconstant +default-port+ 8080)

(zk:defpattern net-endpoint (host port)
  `(zk:keys (:host (type string ,host))
            (:port (zk:range 1 65535 ,port))))

(defun classify-route (route expected-host)
  (zk:match route
    ((net-endpoint (:= expected-host :test #'string-equal) (:const +default-port+))
     :default-local)
    ((net-endpoint host port)
     (list :custom host port))
    (_ :invalid-route)))

(zk-test:deftest custom-patterns-and-equality ()
  (multiple-value-bind (expanded expanded-p)
      (zk:pattern-expand-1 '(net-endpoint h p))
    (zk-test:is expanded-p)
    (zk-test:is-eq 'zk:keys (car expanded)))
  (let ((default-table (make-hash-table))
        (custom-table (make-hash-table))
        (bad-port (make-hash-table)))
    (setf (gethash :host default-table) "LOCALHOST"
          (gethash :port default-table) 8080
          (gethash :host custom-table) "api.internal"
          (gethash :port custom-table) 9000
          (gethash :host bad-port) "api.internal"
          (gethash :port bad-port) 70000)
    (zk-test:is-eq :default-local (classify-route default-table "localhost"))
    (zk-test:is-equal '(:custom "api.internal" 9000) (classify-route custom-table "localhost"))
    (zk-test:is-eq :invalid-route (classify-route bad-port "localhost"))))

;;; ---------------------------------------------------------------------------
;;; 3. Zero-allocation views (zk:view) and hash-table lookups (zk:keys)
;;; ---------------------------------------------------------------------------

(defun decode-hex-pair (n)
  (zk:match n
    ((zk:view (lambda (v) (floor v 16))
              (:values (zk:range 0 15 hi) (zk:range 0 15 lo)))
     (values hi lo))
    (_ (values nil nil))))

(zk-test:deftest views-and-keys ()
  (zk-test:is-values (decode-hex-pair #xAF) 10 15)
  (zk-test:is-values (decode-hex-pair 300) nil nil)
  (zk-test:is= 16
               (zk:match (lit -4)
                 ((and (type lit) (zk:view expr-value v)) (* v v))
                 (_ 0)))
  (let ((config (make-hash-table :test 'equal)))
    (setf (gethash "debug" config) nil
          (gethash "workers" config) 4)
    (zk-test:is-equal '(:present-nil 4)
                      (zk:match config
                        ((zk:keys ("debug" (:values nil t)) ("workers" n))
                         (list :present-nil n))
                        (_ :missing)))
    (zk-test:is-eq :absent
                   (zk:match config
                     ((zk:keys ("missing" (:values _ nil))) :absent)
                     (_ :present)))))

;;; ---------------------------------------------------------------------------
;;; 4. Single-pattern conditionals (with :when) and let-match / let-match*
;;; ---------------------------------------------------------------------------

(defun checked-quota (table)
  (zk:if-match (val t) (:values (gethash "quota" table))
    :when (and (integerp val) (not (minusp val)))
    (values val :ok)
    (values nil :unavailable)))

(zk-test:deftest conditionals-and-let-match ()
  (let ((table (make-hash-table :test 'equal)))
    (zk-test:is-values (checked-quota table) nil :unavailable)
    (setf (gethash "quota" table) -1)
    (zk-test:is-values (checked-quota table) nil :unavailable)
    (setf (gethash "quota" table) 25)
    (zk-test:is-values (checked-quota table) 25 :ok))
  (zk-test:is= 14
               (zk:when-match (lit n) (lit 7)
                 :when (plusp n)
                 (* n 2)))
  (zk-test:is-eq :rejected
               (zk:unless-match (lit n) (lit -3)
                 :when (plusp n)
                 :rejected))
  (let ((outer 100))
    (zk-test:is-equal '(1 100)
                      (zk:let-match (((lit outer) (lit 1))
                                     ((lit inner) (lit outer)))
                        (list outer inner)))
    (zk-test:is-equal '(1 1)
                      (zk:let-match* (((lit outer) (lit 1))
                                      ((lit inner) (lit outer)))
                        (list outer inner)))))

;;; ---------------------------------------------------------------------------
;;; 5. Intervals (zk:range), vectors with (:rest ...), and zk:treelist
;;; ---------------------------------------------------------------------------

(defun status-class (code)
  (declare (type (integer 100 599) code))
  (zk:match code
    ((zk:range 100 199) :informational)
    ((zk:range 200 299) :success)
    ((zk:range 300 399) :redirection)
    ((zk:range 400 499) :client-error)
    ((zk:range 500 599) :server-error)))

(zk-test:deftest ranges-vectors-and-treelists ()
  (zk-test:is-eq :success (status-class 204))
  (zk-test:is-eq :server-error (status-class 503))
  (let ((buf (make-array 5 :fill-pointer 4 :initial-contents '(:head 10 20 :tail :ignored))))
    (zk-test:is-equalp '(:head #(10 20) :tail)
                       (zk:match buf
                         ((vector head (:rest mid) tail) (list head mid tail))
                         (_ nil))))
  (let ((seq (zk-treelist:treelist :start :a :b :end)))
    (zk-test:is-equal '(:start (:a :b) :end)
                      (zk:match seq
                        ((zk:treelist first (:rest mid) last)
                         (list first (coerce mid 'list) last))
                        (_ nil)))))

;;; ---------------------------------------------------------------------------
;;; 6. Continuable zk:cmatch and CLOS defmethod integration
;;; ---------------------------------------------------------------------------

(defgeneric describe-node (node))

(defmethod describe-node ((node expr))
  (zk:match (the expr node)
    ((lit 0) "zero")
    ((lit n) (format nil "int(~D)" n))
    ((sym name) (format nil "sym(~A)" name))
    ((neg inner) (format nil "(- ~A)" (describe-node inner)))
    ((add lhs rhs) (format nil "(+ ~A ~A)" (describe-node lhs) (describe-node rhs)))))

(zk-test:deftest continuable-match-and-clos ()
  (zk-test:is-equal "(+ sym(X) (- zero))"
                    (describe-node (add (sym 'x) (neg (lit 0)))))
  (let ((handled 0))
    (zk-test:is-equal '(10 nil 30)
                      (handler-bind ((zk:match-error
                                       (lambda (c)
                                         (declare (ignore c))
                                         (incf handled)
                                         (continue))))
                        (mapcar (lambda (node)
                                  (zk:cmatch node ((lit v) v)))
                                (list (lit 10) (sym 'skip) (lit 30)))))
    (zk-test:is= 1 handled)))

;;; ---------------------------------------------------------------------------
;;; Run the showcase test suite, then print live expansion & diagnostics.
;;; ---------------------------------------------------------------------------

(zk-test:defsuite pattern-matching-showcase
  typed-adt-and-exhaustive-match
  custom-patterns-and-equality
  views-and-keys
  conditionals-and-let-match
  ranges-vectors-and-treelists
  continuable-match-and-clos)

(let ((report (zk-test:run 'pattern-matching-showcase)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(format t "~&~%=== Live Pattern Matching & Diagnostics Showcase ===~%")

(dolist (form '((+ 0 (+ x 0)) (+ (+ 1 2) (+ 3 4)) (- (- y)) (+ x 2)))
  (format t "Simplify ~S => ~S~%" form (format-expr (simplify-expr (parse-expr form)))))

(format t "Expand (net-endpoint h p) => ~S~%"
        (zk:pattern-expand-1 '(net-endpoint h p)))

(handler-case (macroexpand-1 '(zk:match (the expr e) ((lit _) :lit) ((sym _) :sym)))
  (error (c)
    (format t "Exhaustiveness witness => ~A~%" c)))

(handler-case (parse-expr '(+ x))
  (zk:match-error (c)
    (format t "Runtime match-error values => ~S, patterns => ~S~%"
            (zk:match-error-values c) (zk:match-error-patterns c))))
