;; Regression cases for the public matching interfaces.
(defpackage #:match-examples (:use #:cl))
(in-package #:match-examples)

;; defvariant
(zk:defvariant reply (pending) (ok value) (err reason))
(assert (typep (pending) 'reply))
(assert (typep (ok 7) 'ok))
(assert (not (typep (ok 7) 'err)))
(assert (= 7 (ok-value (ok 7))))
(assert (eq :timeout (err-reason (err :timeout))))

;; defvariant options & typed fields
(zk:defvariant (typed-box (:predicate typed-box-p) (:conc-name tb-))
  (empty-box)
  (full-box (count (integer 0 10)) (label string)))
(assert (typed-box-p (empty-box)))
(assert (typed-box-p (full-box 3 "ok")))
(assert (not (typed-box-p :nope)))
(assert (= 3 (tb-count (full-box 3 "ok"))))
(assert (string= "ok" (tb-label (full-box 3 "ok"))))
(assert (eq :caught
            (handler-case (full-box -1 "bad")
              (type-error () :caught))))

;; defpattern & keys
(zk:defpattern singleton (p) `(cons ,p nil))
(assert (equal (zk:match '(7) ((singleton x) x) (_ :other))
               '7))

(let ((ht (make-hash-table)))
  (setf (gethash :a ht) 10
        (gethash :b ht) nil)
  (assert (equal '(10 nil t)
                 (zk:match ht
                   ((zk:keys (:a a) (:b (:values b present-p)))
                    (list a b present-p))
                   (_ :miss))))
  (assert (eq :miss
              (zk:match ht
                ((zk:keys (:missing _)) :hit)
                (_ :miss)))))

;; pattern-expand-1
(assert (equal (multiple-value-list (zk:pattern-expand-1 '(singleton x)))
               '((cons x nil) t)))

;; view (symbol, lambda, and :values)
(assert (equal (zk:match (ok 7)
  ((and (type ok) (zk:view ok-value n)) (* n n))
  (_ :other))
               '49))
(assert (equal (zk:match (ok 5)
  ((zk:view (lambda (r) (when (typep r 'ok) (ok-value r))) (and (type integer) n)) (+ n 10))
  (_ :other))
               '15))
(assert (equal (zk:match 27
  ((zk:view (lambda (n) (floor n 10)) (:values 2 rem)) rem)
  (_ :other))
               '7))

;; match
(defconstant +limit+ 8)
(assert (equal (zk:match (:inputs :idle (ok 8))
  ((:idle (ok (:const +limit+))) :at-limit)
  ((:idle (ok n))                (list :start n))
  (_ :stay))
               ':at-limit))

;; ematch
(assert (equal (handler-case (zk:ematch (pending) ((ok x) x))
  (zk:match-error () :expected-ok))
               ':expected-ok))

;; cmatch
(assert (equal (let ((calls 0))
  (list (handler-bind ((zk:match-error #'continue))
          (zk:cmatch (:values (progn (incf calls) (values 503 :offline)))
            ((code :online) code)))
        calls))
               '(nil 1)))

;; if-match / when-match / unless-match (including :when guards)
(assert (equal (zk:if-match (ok x) (ok nil) (list :hit x) :miss)
               '(:hit nil)))
(assert (equal (zk:if-match (ok x) (ok -2) :when (plusp x) x :non-positive)
               ':non-positive))
(assert (equal (zk:when-match (err why) (err :timeout) (list :retry why))
               '(:retry :timeout)))
(assert (null (zk:when-match (ok x) (ok 0) :when (plusp x) x)))
(assert (equal (zk:unless-match (ok _) (pending) :poll-again)
               ':poll-again))
(assert (eq :rejected
            (zk:unless-match (ok x) (ok 0) :when (plusp x) :rejected)))

;; let-match
(assert (equal (let ((x 10))
  (zk:let-match (((ok x) (ok 2))
                 ((ok y) (ok x)))
    (list x y)))
               '(2 10)))

;; let-match*
(assert (equal (let ((x 10))
  (zk:let-match* (((ok x) (ok 2))
                  ((ok y) (ok x)))
    (list x y)))
               '(2 2)))

;; standard defun / defmethod / lambda composed with match / ematch / cmatch
(assert (equal (mapcar (lambda (left right)
                         (zk:match (:inputs left right)
                           (((ok a) (ok b)) (+ a b))
                           (_ :skip)))
                       (list (ok 2) (pending))
                       (list (ok 3) (ok 4)))
               '(5 :skip)))

(assert (equal (mapcar (lambda (r)
                         (zk:ematch r
                           ((ok n) (* n n))))
                       (list (ok 2) (ok 3)))
               '(4 9)))

(assert (equal (handler-bind ((zk:match-error #'continue))
                 (mapcar (lambda (r)
                           (zk:cmatch r ((ok x) x)))
                         (list (ok 1) (pending) (ok 3))))
               '(1 nil 3)))

(defun transition (state event &key (scale 1))
  (zk:match (:inputs state event)
    ((:idle (ok n)) (list :start (* scale n)))
    (_ :stay)))
(assert (equal (transition :idle (ok 4) :scale 3)
               '(:start 12)))

(defun unwrap (r)
  (zk:ematch r
    ((ok x) x)))
(assert (equal (handler-case (unwrap (pending))
                 (zk:match-error () :no-value))
               ':no-value))

(defun accept-event (state event)
  (zk:cmatch (:inputs state event)
    ((:waiting (ok n)) n)))
(assert (equal (handler-bind ((zk:match-error #'continue))
                 (accept-event :idle (ok 7)))
               'nil))

(defgeneric render-reply (r))
(defmethod render-reply ((r reply)) :reply)
(defmethod render-reply ((r err))
  (zk:match r
    ((err why) :when (stringp why) why)
    (_ (call-next-method))))
(assert (equal (list (render-reply (err "disk")) (render-reply (err 7)))
               '("disk" :reply)))

(defgeneric sum-replies (a b))
(defmethod sum-replies ((a ok) (b ok))
  (zk:ematch (:inputs a b)
    (((ok (type integer x)) (ok (type integer y))) (+ x y))))
(assert (equal (sum-replies (ok 2) (ok 5))
               '7))

(defgeneric dispatch-event (operation r))
(defmethod dispatch-event ((operation (eql :retry)) (r err))
  (zk:cmatch (:inputs operation r)
    ((:retry (err :timeout)) :queued)))
(assert (equal (handler-bind ((zk:match-error #'continue))
                 (dispatch-event :retry (err :disk)))
               'nil))

;; match-error
(assert (equal (handler-case (zk:ematch :offline (:online 1))
  (zk:match-error (c) (typep c 'zk:match-error)))
               't))

;; match-error-values
(assert (equal (flet ((miss (producer)
         (handler-case
             (zk:ematch (:values (funcall producer)) ((:ready) 1))
           (zk:match-error (c) (zk:match-error-values c)))))
  (list (miss (lambda () (values))) (miss (lambda () (values nil)))))
               '(nil (nil))))

;; match-error-patterns
(assert (equal (handler-case
    (zk:ematch (pending) ((ok n) n) ((err why) why))
  (zk:match-error (c) (zk:match-error-patterns c)))
               '((ok n) (err why))))
