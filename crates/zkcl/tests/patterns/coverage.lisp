(in-package #:match-examples)
(defun code (r)
  (declare (type reply r))
  (zk:match r
    ((ok 0) :zero)
    ((pending) :pending)
    ((ok n) n)
    ((err reason) reason)))
(assert (equal (mapcar #'code (list (ok 0) (pending) (ok 4) (err :io)))
               '(:zero :pending 4 :io)))
(assert (eq :green (zk:match (the (member :red :green) :green)
                     (:red nil) (:green :green))))
(assert (equal '(2 3) (zk:match '(1 2 3) ((list* _ tail) tail) (_ nil))))
(assert (= 5 (zk:match #(2 3) ((vector a b) (+ a b)) (_ 0))))
(assert (= 5 (zk:match (ok (err 5)) ((ok (err n)) n) (_ 0))))
(assert (= 5 (zk:match (ok 5) ((or (err n) (ok n)) n) (_ 0))))
;; Left-to-right view execution order and non-wiping guarded rows.
(let ((reads 0))
  (flet ((counted-car (v) (incf reads) (car v)))
    (assert (eq :second
      (zk:match (cons 1 2)
        ((and (cons 1 _) (zk:view counted-car 1)) :when nil :first)
        ((and (cons 1 _) (zk:view counted-car 1)) :second)
        (_ :miss))))
    (assert (= reads 2))))
(flet ((pick (k allow-b)
         (zk:match (the (member :a :b) k)
           (:a 1)
           (:b :when allow-b 2)
           (:b 3))))
  (assert (equal '(1 2 3) (list (pick :a t) (pick :b t) (pick :b nil)))))
(assert (= 9 (funcall (lambda (&key (n 4)) (zk:match (:inputs) (() (* n n)))) :n 3)))
(assert (eq :empty (zk:match (:values (values)) ((nil) :empty) (_ :other))))

;; An ordinary DEFUN supplies lexical type evidence to the macro expander.
(defun declared-reply (r)
  (declare (type reply r))
  (zk:match r ((ok x) x) ((err why) why) ((pending) nil)))
(assert (= 19 (declared-reply (ok 19))))

;; Constructor fields use definition-site identities. A lexical function
;; binding cannot change their meaning; an explicit VIEW does see it.
(flet ((ok-value (x) (declare (ignore x)) 99))
  (assert (= 7 (zk:match (ok 7) ((ok n) n))))
  (assert (= 99 (zk:ematch (ok 7) ((zk:view ok-value n) n)))))

(zk:defpattern nested (&whole whole p &environment env)
  (declare (ignore whole))
  (zk:pattern-expand-1 (list 'singleton p) env))
(assert (= 42 (zk:ematch (list 42) ((nested n) n))))

;; Restarts must not swallow nonlocal control transfers from view functions.
(assert (= 31 (catch 'exit
                (flet ((leave (v) (throw 'exit v)))
                  (zk:ematch 31 ((zk:view leave _) 0))))))

;; Only the parallel binding evaluates its second input after a first miss.
(let ((evaluations 0))
  (handler-case (zk:let-match (((ok _) (pending)) (z (incf evaluations))) z)
    (zk:match-error () nil))
  (handler-case (zk:let-match* (((ok _) (pending)) (z (incf evaluations))) z)
    (zk:match-error () nil))
  (assert (= evaluations 1)))

;; Shorthand expansion and direct expansion agree in expression and file code.
(defun bool-pair (a b)
  (declare (type boolean a b))
  (zk:match (:inputs a b)
    ((nil nil) 0) ((nil t) 1) ((t nil) 2) ((t t) 3)))
(assert (equal (list (bool-pair nil nil) (bool-pair nil t)
                     (bool-pair t nil) (bool-pair t t)) '(0 1 2 3)))

;; Each ordered branch body is emitted once via DAG join points. The nested
;; literal switch shares its tag and field lookup.
(defun classify (r)
  (declare (type reply r))
  (zk:match r
    ((ok 0) :zero) ((ok 1) :one) ((ok n) n)
    ((pending) :pending) ((err why) why)))
(assert (equal (mapcar #'classify (list (ok 0) (ok 1) (ok 9) (pending) (err :e)))
               '(:zero :one 9 :pending :e)))

(flet ((zk:match (v) (+ v 1)))
  (assert (= 4 (zk:match 3))))
(defgeneric weighted-reply (r &key scale))
(defmethod weighted-reply ((r reply) &key scale)
  (declare (ignore r scale)) 1)
(defmethod weighted-reply :around ((r ok) &key (scale 1))
  (declare (type ok r))
  (zk:match r
    ((ok n) (+ (* n scale) (call-next-method)))))
(assert (= 13 (weighted-reply (ok 4) :scale 3)))

;; The matrix witness must take field types into account.
(defun boolean-record (r)
  (declare (type (cons boolean boolean) r))
  (zk:match r
    ((cons nil nil) 0) ((cons nil t) 1) ((cons t nil) 2) ((cons t t) 3)))
(assert (= 2 (boolean-record (cons t nil))))

(let ((diagnostics nil))
  (handler-bind ((warning (lambda (w) (push (princ-to-string w) diagnostics))))
    (macroexpand-1 '(zk:match r (pending :all) ((ok _) :dead))))
  (assert (some (lambda (text) (search "binds every value" text)) diagnostics)))

;; Loading a plugin after compilation preserves the open CLOS branch.
(defclass open-event () ())
(defclass click-event (open-event) ())
(defun event-kind (event)
  (declare (type open-event event))
  (zk:match event
    ((type click-event) :click)
    ((type open-event) :future)))
(defclass plugin-event (open-event) ())
(assert (eq :future (event-kind (make-instance 'plugin-event))))

;; Catch performance regressions that would still return the same values.
(defun count-operator (tree operator)
  (if (consp tree)
      (+ (if (eq (car tree) operator) 1 0)
         (count-operator (car tree) operator)
         (count-operator (cdr tree) operator))
      0))
(let ((tree (macroexpand-1
             '(zk:match (the reply r)
                ((ok 0) :zero) ((ok 1) :one) ((ok n) n)
                ((pending) :waiting) ((err _) :failed)))))
  (assert (= 1 (count-operator tree 'zk::%pattern-tag)))
  (assert (= 1 (count-operator tree 'zk::%pattern-field))))

;; Disjoint OR shares a single emitted arm body via DAG join points.
(let ((tree (macroexpand-1
             '(zk:match (the reply r)
                ((or (ok n) (err n)) (shared-arm-marker n))
                ((pending) :waiting)))))
  (assert (= 1 (count-operator tree 'shared-arm-marker))))

;; OR shares its bindings, but a successful arm returning a NIL field still
;; wins. The failed first arm cannot leak a binding into the body.
(let ((reads 0))
  (flet ((tick (v) (incf reads) v))
    (assert (equal '(:won nil)
      (zk:match '(nil . 4)
        ((or (cons x :other) (cons (zk:view tick x) _)) (list :won x))
        (_ :miss))))
    (assert (= 1 reads))))
;; An explicitly discarded view still runs once, even inside an empty AND.
(let ((calls 0))
  (flet ((bump (v) (incf calls) v))
    (assert (eq :hit (zk:ematch :x ((zk:view bump (and)) :hit))))
    (assert (= calls 1))))

;; Recover exactly the producer's values on a miss, even when the row is
;; shorter (or longer). A known-arity success needs no temporary values list.
(dolist (case '(((values) nil)
                ((values nil) (nil))
                ((values :a nil :c) (:a nil :c))))
  (let ((form `(handler-case (zk:ematch (:values ,(first case)) ((:no _) :wrong))
                 (zk:match-error (c) (zk:match-error-values c)))))
    (assert (equal (second case) (eval form)))))
(let ((table (make-hash-table)))
  (setf (gethash :k table) nil)
  (assert (equal '(nil t)
    (handler-case (zk:ematch (:values (gethash :k table)) ((:absent) :wrong))
      (zk:match-error (c) (zk:match-error-values c))))))
(assert (zerop (count-operator (macroexpand-1 '(zk:ematch (:values (values x y)) ((:ok a) a)))
                               'multiple-value-list)))
(assert (zerop (count-operator (macroexpand-1 '(zk:ematch (:values (funcall f)) ((:ok a) a) (_ nil)))
                               'multiple-value-list)))
(let ((order nil))
  (assert (equal '(10 20)
    (multiple-value-list
     (zk:match (:values (values (progn (push :a order) (values 10 11))
                                (progn (push :b order) 20)
                                (progn (push :c order) 30)))
       ((a b) (values a b))))))
  (assert (equal order '(:c :b :a))))

(defvar *coverage-expansions* 0)
(zk:defpattern counting-pattern (pattern)
  (incf *coverage-expansions*)
  pattern)

(defun coverage-failure (form)
  (handler-bind ((warning #'muffle-warning))
    (handler-case (progn (macroexpand-1 form) nil)
      (error (condition) (princ-to-string condition)))))

;; Build the same proof twice from fresh syntax. The second expansion still
;; runs the pattern expander, allocates its own bindings, and emits its body.
(let* ((zk-pattern-compiler::*coverage-proofs* (zk-pattern-compiler::make-coverage-proofs))
       (*coverage-expansions* 0)
       (domain (list 'integer 0 7))
       (rows (loop for key below 8 collect (list (list key t) :selected))))
  (flet ((form (tail body)
           `(zk:match (:inputs (the ,domain key) (the boolean flag))
              ,@rows (,tail ,body))))
    (let* ((cold (macroexpand-1 (form '((counting-pattern _) nil) :cold)))
           (hot (macroexpand-1 (form '((counting-pattern _) nil) :edited))))
      (assert (= 1 (zk-pattern-compiler::coverage-proofs-next zk-pattern-compiler::*coverage-proofs*)))
      (assert (= 2 *coverage-expansions*))
      (assert (not (eq (caar (second cold)) (caar (second hot)))))
      (assert (eq :cold (eval `(let ((key 7) (flag nil)) ,cold))))
      (assert (eq :edited (eval `(let ((key 7) (flag nil)) ,hot)))))

    ;; Changing the domain or final row invalidates the proof and leaves the
    ;; same uncovered witness as the uncached path.
    (let ((missing-row (form '(0 nil) :other)))
      (let ((failure (coverage-failure missing-row)))
        (assert (search "missing pattern" failure))
        (let ((zk-pattern-compiler::*coverage-proofs* nil))
          (assert (equal failure (coverage-failure missing-row))))
        (let ((zk-pattern-compiler::*coverage-proofs* (zk-pattern-compiler::make-coverage-proofs)))
          (macroexpand-1 (cons 'zk:ematch (cdr missing-row)))
          (assert (equal failure (coverage-failure missing-row))))))
    (let ((proof (form '(_ nil) :other)))
      (setf (third domain) 8)
      (let ((failure (coverage-failure proof)))
        (assert (search "(8 T)" failure))
        (let ((zk-pattern-compiler::*coverage-proofs* nil))
          (assert (equal failure (coverage-failure proof))))))
    (setf (third domain) 7)
    (assert (search "guard" (coverage-failure
                              (append (butlast (form '(_ nil) :other))
                                      '(((_ nil) :when nil :other))))))

    ;; Even when the dead source rows normalize identically, each expansion
    ;; signals the warning with that call site's source pattern.
    (dolist (dead '((0 t) ((quote 0) t)))
      (let ((warnings nil))
        (handler-bind ((warning (lambda (w) (push (princ-to-string w) warnings)
                                           (muffle-warning w))))
          (macroexpand-1 (append (form '(_ nil) :other) (list (list dead :unreachable)))))
        (assert (= 1 (length warnings)))
        (assert (search (format nil "Unreachable pattern ~S" dead) (first warnings)))))
    ;; The warning runs before the final coverage query. Use the expander
    ;; directly so the handler and expander share the dynamically built type.
    (let ((failure
            (handler-bind ((warning (lambda (w) (setf (third domain) 8) (muffle-warning w))))
              (handler-case
                  (progn (funcall (macro-function 'zk:match)
                                  (append (form '(_ nil) :other) '(((0 t) :unreachable))) nil)
                         nil)
                (error (condition) (princ-to-string condition))))))
      (assert (search "(8 T)" failure)))
    (assert (= 1 (zk-pattern-compiler::coverage-proofs-next zk-pattern-compiler::*coverage-proofs*)))))

;; Uninterned names with identical spelling denote different EQL objects.
(let* ((zk-pattern-compiler::*coverage-proofs* (zk-pattern-compiler::make-coverage-proofs))
       (covered (make-symbol "SAME-KEY"))
       (other (make-symbol "SAME-KEY"))
       (domain `(member ,covered 1 2 3 4 5 6 7))
       (rows (loop for key from 1 to 7 collect (list (list key t) :selected))))
  (flet ((form (symbol)
           `(zk:match (:inputs (the ,domain key) (the boolean flag))
              (((quote ,symbol) t) :symbol) ,@rows ((_ nil) :other))))
    (let* ((good (form covered)) (bad (form other)))
      (assert (macroexpand-1 good))
      (assert (= 1 (zk-pattern-compiler::coverage-proofs-next zk-pattern-compiler::*coverage-proofs*)))
      (let ((failure (coverage-failure bad)))
        (assert (search "missing pattern" failure))
        (assert (search "SAME-KEY" failure))))))

;; Cache construction cannot walk an unbounded source type specifier, even if
;; it contains only supported atoms. An oversized key leaves the solver alone.
(assert (null (zk::%pattern-coverage-key (make-list 2048 :initial-element 0))))
(let ((circular (list nil)))
  (setf (cdr circular) circular)
  (assert (null (zk::%pattern-coverage-key circular))))
