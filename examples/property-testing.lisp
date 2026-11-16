;;;; Modern Property-Based Testing showcase for zk-pbt and zk-test.
;;;; Run from the repository root with:
;;;;   ./target/release/zkcl run examples/property-testing.lisp

(defpackage #:pbt-showcase
  (:use #:cl))
(in-package #:pbt-showcase)

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-pbt))

;;; ---------------------------------------------------------------------------
;;; 1. Run-length encoding: 4 input sources, let-gen, with-corners,
;;;    statistical coverage (:confidence), and collect-range tabulation.
;;; ---------------------------------------------------------------------------

(defun rle-encode (items)
  "Encode adjacent equal items as (ITEM . COUNT) pairs."
  (let ((runs nil))
    (dolist (item items (nreverse runs))
      (if (and runs (eql (caar runs) item))
          (incf (cdar runs))
          (push (cons item 1) runs)))))

(defun rle-decode (runs)
  "Expand (ITEM . COUNT) pairs back into a flat list."
  (loop for (item . count) in runs
        nconc (make-list count :initial-element item)))

(zk-test:deftest rle-known-cases ()
  (zk-test:is-equal nil (rle-encode nil))
  (zk-test:is-equal '((:a . 3) (:b . 1) (:a . 2))
                    (rle-encode '(:a :a :a :b :a :a)))
  (zk-test:is-equal '(:x :x :y) (rle-decode '((:x . 2) (:y . 1)))))

(defun rle-run-list-generator ()
  "Generate lists skewed toward adjacent duplicates alongside arbitrary lists."
  (zk-pbt:with-corners '(nil (:a) (:a :a :a))
    (zk-pbt:one-of
     (zk-pbt:list-of (zk-pbt:elements :a :b :c) :max 12)
     (zk-pbt:let-gen ((pairs (zk-pbt:list-of
                              (zk-pbt:cons-of (zk-pbt:elements :a :b :c)
                                              (zk-pbt:integers :min 1 :max 4))
                              :min 1 :max 5)))
       (rle-decode pairs)))))

(zk-pbt:defproperty rle-round-trip ((items (rle-run-list-generator)))
  "Encoding and decoding must round-trip and avoid zero/adjacent runs."
  (:trials 60)
  (:examples (nil) ((:a :a :b)))
  (:enumerate (zk-pbt:tuple (zk-pbt:list-of (zk-pbt:elements :a :b) :min 0 :max 3)))
  (:cover :has-runs :at-least 1/4 :confidence 95/100)
  (let* ((encoded (rle-encode items))
         (decoded (rle-decode encoded))
         (has-multi (some (lambda (pair) (> (cdr pair) 1)) encoded)))
    (zk-pbt:classify :has-runs has-multi)
    (zk-pbt:collect-range :list-length (length items) :step 4)
    (and (every (lambda (pair) (plusp (cdr pair))) encoded)
         (loop for rest on encoded
               while (cdr rest)
               never (eql (caar rest) (caadr rest)))
         (equal items decoded))))

;;; ---------------------------------------------------------------------------
;;; 2. Struct generators (defstruct-gen), CL type generators (type-gen),
;;;    differential sequence edits (mutated-pair), and inline for-all.
;;; ---------------------------------------------------------------------------

(defstruct span lo hi)

(zk-pbt:defstruct-gen raw-span-gen make-span
  (:lo (zk-pbt:integers :min -10 :max 10))
  (:hi (zk-pbt:integers :min -10 :max 10)))

(defun ordered-span-gen ()
  (zk-pbt:let-gen ((base (raw-span-gen)))
    (if (<= (span-lo base) (span-hi base))
        base
        (make-span :lo (span-hi base) :hi (span-lo base)))))

(defun merge-spans (spans)
  "Sort and merge overlapping or touching closed intervals."
  (when spans
    (let* ((sorted (sort (copy-list spans) #'< :key #'span-lo))
           (acc (list (make-span :lo (span-lo (first sorted))
                                 :hi (span-hi (first sorted))))))
      (dolist (cur (rest sorted) (nreverse acc))
        (let ((top (first acc)))
          (if (<= (span-lo cur) (span-hi top))
              (setf (span-hi top) (max (span-hi top) (span-hi cur)))
              (push (make-span :lo (span-lo cur) :hi (span-hi cur)) acc)))))))

(zk-pbt:defproperty merged-spans-disjoint-and-ordered
    ((spans (zk-pbt:list-of (ordered-span-gen) :max 8)))
  (:trials 50)
  (let ((merged (merge-spans spans)))
    (and (every (lambda (s) (<= (span-lo s) (span-hi s))) merged)
         (loop for rest on merged
               while (cdr rest)
               always (< (span-hi (car rest)) (span-lo (cadr rest))))
         (every (lambda (orig)
                  (some (lambda (m) (<= (span-lo m) (span-lo orig) (span-hi orig) (span-hi m)))
                        merged))
                spans))))

(zk-test:deftest type-and-differential-checks ()
  (zk-pbt:for-all ((payload (zk-pbt:type-gen '(or null (vector (unsigned-byte 8) 4)))))
    (:name typed-packet-check)
    (:trials 30)
    (or (null payload)
        (and (vectorp payload)
             (= 4 (length payload))
             (every (lambda (b) (<= 0 b 255)) payload))))
  (zk-pbt:for-all ((pair (zk-pbt:mutated-pair
                          (zk-pbt:printable-strings :min 1 :max 6)
                          :max-edits 3
                          :element-gen (zk-pbt:alphanumeric-characters))))
    (:name differential-string-edit)
    (:trials 30)
    (and (stringp (first pair))
         (stringp (second pair))
         (<= (abs (- (length (first pair)) (length (second pair)))) 3))))

;;; ---------------------------------------------------------------------------
;;; 3. Higher-order function generators (functions) and fusion laws.
;;; ---------------------------------------------------------------------------

(zk-pbt:defproperty mapcar-fusion
    ((f (zk-pbt:functions (zk-pbt:integers :min -10 :max 10) :arity 1 :max-entries 4))
     (g (zk-pbt:functions (zk-pbt:integers :min -10 :max 10) :arity 1 :max-entries 4))
     (xs (zk-pbt:list-of (zk-pbt:integers :min -5 :max 5) :max 8)))
  (:trials 50)
  (equal (mapcar f (mapcar g xs))
         (mapcar (lambda (x) (funcall f (funcall g x))) xs)))

;;; ---------------------------------------------------------------------------
;;; 4. Recursive AST constant folding: size-bounded-enumerator,
;;;    swarm-one-of, and targeted search (target).
;;; ---------------------------------------------------------------------------

(defun eval-expr (expr env)
  (cond
    ((integerp expr) expr)
    ((symbolp expr) (or (cdr (assoc expr env)) 0))
    ((eq (first expr) '-) (- (eval-expr (second expr) env)))
    ((eq (first expr) '+) (+ (eval-expr (second expr) env)
                             (eval-expr (third expr) env)))
    ((eq (first expr) '*) (* (eval-expr (second expr) env)
                             (eval-expr (third expr) env)))
    (t (error "Unknown AST node ~S" expr))))

(defun fold-expr (expr)
  (cond
    ((or (integerp expr) (symbolp expr)) expr)
    ((eq (first expr) '-)
     (let ((a (fold-expr (second expr))))
       (if (integerp a) (- a) (list '- a))))
    ((eq (first expr) '+)
     (let ((a (fold-expr (second expr)))
           (b (fold-expr (third expr))))
       (cond ((and (integerp a) (integerp b)) (+ a b))
             ((eql a 0) b)
             ((eql b 0) a)
             (t (list '+ a b)))))
    ((eq (first expr) '*)
     (let ((a (fold-expr (second expr)))
           (b (fold-expr (third expr))))
       (cond ((and (integerp a) (integerp b)) (* a b))
             ((or (eql a 0) (eql b 0)) 0)
             ((eql a 1) b)
             ((eql b 1) a)
             (t (list '* a b)))))
    (t expr)))

(defun expr-depth (expr)
  (if (atom expr)
      0
      (1+ (reduce #'max (rest expr) :key #'expr-depth :initial-value 0))))

(defun expr-step (child)
  (zk-pbt:swarm-one-of
   (zk-pbt:map-gen (lambda (a) (list '- a)) child)
   (zk-pbt:map-gen (lambda (a b) (list '+ a b)) child child)
   (zk-pbt:map-gen (lambda (a b) (list '* a b)) child child)))

(defun expr-enum-step (child)
  (zk-pbt:one-of
   (zk-pbt:map-gen (lambda (a) (list '- a)) child)
   (zk-pbt:map-gen (lambda (a b) (list '+ a b)) child child)))

(zk-pbt:defproperty ast-fold-preserves-semantics
    ((expr (zk-pbt:recursive (zk-pbt:one-of (zk-pbt:integers :min -3 :max 3)
                                            (zk-pbt:elements 'x 'y))
                             #'expr-step
                             :max-depth 4))
     (x-val (zk-pbt:integers :min -5 :max 5))
     (y-val (zk-pbt:integers :min -5 :max 5)))
  (:trials 60)
  (:enumerate (zk-pbt:tuple
               (zk-pbt:size-bounded-enumerator (zk-pbt:elements 0 1 'x) #'expr-enum-step :max-depth 1)
               (zk-pbt:elements 2)
               (zk-pbt:elements -3)))
  (let ((env (list (cons 'x x-val) (cons 'y y-val))))
    (zk-pbt:target (expr-depth expr) :ast-depth)
    (zk-pbt:collect-range :ast-depth (expr-depth expr) :step 1)
    (= (eval-expr expr env)
       (eval-expr (fold-expr expr) env))))

;;; ---------------------------------------------------------------------------
;;; 5. Stateful model checking with Bundle handles (:produces / deref / :post).
;;; ---------------------------------------------------------------------------

(zk-pbt:defmachine ledger-machine
  (:model () nil)
  (:open () (list :accounts nil))
  (:invariant (model sut)
    (and (= (length model) (length (getf sut :accounts)))
         (every (lambda (entry)
                  (let* ((ref (car entry))
                         (expected-bal (cdr entry))
                         (box (zk-pbt:deref ref)))
                    (and (>= expected-bal 0)
                         (= expected-bal (car box)))))
                model)))
  (:command :open-account ()
    (:produces :accounts)
    (:weight 2)
    (:step (model)
      (let ((ref (list :ref :accounts (1+ (length model)))))
        (append model (list (cons ref 0)))))
    (:act (sut)
      (let ((box (list 0)))
        (push box (getf sut :accounts))
        box))
    (:post (before after actual)
      (declare (ignore actual))
      (= (length after) (1+ (length before)))))
  (:command :deposit (acct amount)
    (:weight 4)
    (:args (model)
      (zk-pbt:tuple (zk-pbt:bundle-elements :accounts)
                    (zk-pbt:integers :min 1 :max 20)))
    (:step (model)
      (values (mapcar (lambda (kv)
                        (if (equal (car kv) acct)
                            (cons (car kv) (+ (cdr kv) amount))
                            kv))
                      model)
              (list (+ (cdr (assoc acct model :test #'equal)) amount))))
    (:act (sut)
      (let ((box (zk-pbt:deref acct)))
        (incf (car box) amount))))
  (:command :withdraw (acct amount)
    (:weight 3)
    (:args (model)
      (zk-pbt:tuple (zk-pbt:bundle-elements :accounts)
                    (zk-pbt:integers :min 1 :max 20)))
    (:pre (model)
      (let ((entry (assoc acct model :test #'equal)))
        (and entry (>= (cdr entry) amount))))
    (:step (model)
      (values (mapcar (lambda (kv)
                        (if (equal (car kv) acct)
                            (cons (car kv) (- (cdr kv) amount))
                            kv))
                      model)
              (list (- (cdr (assoc acct model :test #'equal)) amount))))
    (:act (sut)
      (let ((box (zk-pbt:deref acct)))
        (decf (car box) amount)))))

(zk-pbt:defproperty ledger-matches-model
    ((ops (zk-pbt:commands 'ledger-machine :max 12)))
  (:trials 40)
  (zk-pbt:run-commands 'ledger-machine ops))

;;; ---------------------------------------------------------------------------
;;; 6. Parallel linearizability testing.
;;; ---------------------------------------------------------------------------

(zk-pbt:defmachine atomic-counter
  (:model () 0)
  (:open () (list 0))
  (:command :inc ()
    (:step (m) (values (1+ m) (list (1+ m))))
    (:act (sut) (incf (car sut)))))

(zk-pbt:defproperty atomic-counter-is-linearizable
    ((plan (zk-pbt:parallel-commands 'atomic-counter :prefix-max 2 :branch-count 2 :branch-max 3)))
  (:trials 30)
  (zk-pbt:run-parallel-commands 'atomic-counter plan))

;;; ---------------------------------------------------------------------------
;;; 7. Monolith-style multi-object differential modeling (Transient CoW).
;;; ---------------------------------------------------------------------------

(defstruct (cow-box (:constructor make-cow-box (vec owner)))
  vec owner)

(defun cow-box-clone (box)
  (let* ((src (cow-box-vec box))
         (n (length src))
         (dst (make-array n :adjustable t :fill-pointer n)))
    (dotimes (i n dst) (setf (aref dst i) (aref src i)))))

(defun cow-box-own (box)
  (when (eq (cow-box-owner box) :shared)
    (setf (cow-box-vec box) (cow-box-clone box)
          (cow-box-owner box) :unique))
  box)

(zk-pbt:defmodel cow-seq
  (:abstract :mseq :check (lambda (ref cand) (equal (car ref) (coerce (cow-box-vec cand) 'list))))
  (:abstract :pseq :check (lambda (ref cand) (equal ref (coerce cand 'list))))
  (:op :make ()
    (:weight 2)
    (:returns (:abstract :mseq))
    (:ref (lambda () (list nil)))
    (:cand (lambda () (make-cow-box (make-array 0 :adjustable t :fill-pointer 0) :unique))))
  (:op :push ((s (:ref :mseq)) (x (zk-pbt:integers :min -20 :max 20)))
    (:weight 4)
    (:returns :value)
    (:ref (lambda (s x) (setf (car s) (append (car s) (list x))) (length (car s))))
    (:cand (lambda (s x)
             (cow-box-own s)
             (vector-push-extend x (cow-box-vec s))
             (length (cow-box-vec s)))))
  (:op :snapshot ((s (:ref :mseq)))
    (:weight 3)
    (:returns (:abstract :pseq))
    (:ref (lambda (s) (copy-list (car s))))
    (:cand (lambda (s)
             (setf (cow-box-owner s) :shared)
             (cow-box-vec s))))
  (:op :pget ((p (:ref :pseq))
              (i (:from (p) (zk-pbt:integers :min 0 :max (max 0 (1- (length p)))))))
    (:weight 2)
    (:returns :value)
    (:ref (lambda (p i) (if (< i (length p)) (nth i p) (error "index out of bounds"))))
    (:cand (lambda (p i) (if (< i (length p)) (aref p i) (error "index out of bounds")))))
  (:op :split ((s (:ref :mseq))
               (i (:from (s) (zk-pbt:integers :min 0 :max (length (car s))))))
    (:weight 2)
    (:returns (:values (:abstract :mseq) (:abstract :mseq)))
    (:ref (lambda (s i) (values (list (subseq (car s) 0 i)) (list (subseq (car s) i)))))
    (:cand (lambda (s i)
             (let* ((v (cow-box-vec s))
                    (left (make-array i :adjustable t :fill-pointer i))
                    (rlen (- (length v) i))
                    (right (make-array rlen :adjustable t :fill-pointer rlen)))
               (dotimes (k i) (setf (aref left k) (aref v k)))
               (dotimes (k rlen) (setf (aref right k) (aref v (+ i k))))
               (values (make-cow-box left :unique) (make-cow-box right :unique)))))))

(zk-pbt:defproperty cow-sequence-matches-model
    ((sc (zk-pbt:model-scenarios 'cow-seq :min 2 :max 12)))
  (:trials 40)
  (zk-pbt:run-model 'cow-seq sc))

;;; ---------------------------------------------------------------------------
;;; Run the passing showcase suite, then print live shrink demonstrations.
;;; ---------------------------------------------------------------------------

(zk-test:defsuite pbt-showcase
  rle-known-cases
  rle-round-trip
  merged-spans-disjoint-and-ordered
  type-and-differential-checks
  mapcar-fusion
  ast-fold-preserves-semantics
  ledger-matches-model
  atomic-counter-is-linearizable
  cow-sequence-matches-model)

(let ((report (zk-test:run 'pbt-showcase :seed 42)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(format t "~&~%=== Live Shrinking & Diagnostics Showcase ===~%")

;; A. Higher-order function counterexample with recorded :CALLS and NOTE.
(let ((fn-report
        (zk-pbt:for-all ((f (zk-pbt:functions (zk-pbt:integers :min 0 :max 20)
                                              :arity 1 :max-entries 4)))
          (:name demo-function-calls)
          (:seed 42)
          (:trials 60)
          (:check nil)
          (let ((a (funcall f :a))
                (b (funcall f :b)))
            (zk-pbt:note "f(:a)=~D f(:b)=~D sum=~D" a b (+ a b))
            (< (+ a b) 12)))))
  (zk-test:print-report fn-report))

;; B. Tree-lifting shrinking on a buggy AST optimizer ((* x 0) -> x).
(labels ((buggy-fold (expr)
           (cond
             ((atom expr) expr)
             ((eq (first expr) '-) (list '- (buggy-fold (second expr))))
             ((eq (first expr) '+) (list '+ (buggy-fold (second expr)) (buggy-fold (third expr))))
             ((eq (first expr) '*)
              (let ((a (buggy-fold (second expr)))
                    (b (buggy-fold (third expr))))
                (if (eql b 0) a (list '* a b))))
             (t expr))))
  (let ((ast-report
          (zk-pbt:for-all
              ((expr (zk-pbt:resize 60
                       (zk-pbt:recursive (zk-pbt:elements 0 1 'x)
                                         #'expr-step
                                         :max-depth 4 :leaf-weight 1 :branch-weight 5))))
            (:name demo-buggy-ast-fold)
            (:seed 42)
            (:trials 40)
            (:check nil)
            (= (eval-expr expr '((x . 3)))
               (eval-expr (buggy-fold expr) '((x . 3)))))))
    (zk-test:print-report ast-report)))

;; C. Parallel linearizability violation shrunk to minimal interleaved race.
(zk-pbt:defmachine racy-counter
  (:model () 0)
  (:open () (list :val 0 :snap nil))
  (:command :read-snap ()
    (:step (m) (values m (list m)))
    (:act (sut) (setf (getf sut :snap) (getf sut :val))))
  (:command :write-from-snap ()
    (:step (m) (values (1+ m) (list (1+ m))))
    (:act (sut)
      (let ((next (1+ (or (getf sut :snap) (getf sut :val)))))
        (setf (getf sut :val) next)
        next))))

(let ((race-report
        (zk-pbt:for-all
            ((plan (zk-pbt:parallel-commands 'racy-counter
                                             :prefix-max 1 :branch-count 2 :branch-max 3
                                             :with-schedule t)))
          (:name demo-racy-linearizability)
          (:seed 42)
          (:trials 60)
          (:check nil)
          (zk-pbt:run-parallel-commands 'racy-counter plan))))
  (zk-test:print-report race-report))

;; D. Multi-object differential shrinking: broken CoW corrupts historical snapshot.
(zk-pbt:defmodel buggy-cow-seq
  (:abstract :mseq :check (lambda (ref cand) (equal (car ref) (coerce (cow-box-vec cand) 'list))))
  (:abstract :pseq :check (lambda (ref cand) (equal ref (coerce cand 'list))))
  (:op :make ()
    (:weight 2)
    (:returns (:abstract :mseq))
    (:ref (lambda () (list nil)))
    (:cand (lambda () (make-cow-box (make-array 0 :adjustable t :fill-pointer 0) :unique))))
  (:op :push ((s (:ref :mseq)) (x (zk-pbt:integers :min -20 :max 20)))
    (:weight 4)
    (:returns :value)
    (:ref (lambda (s x) (setf (car s) (append (car s) (list x))) (length (car s))))
    (:cand (lambda (s x)
             (cow-box-own s)
             (vector-push-extend x (cow-box-vec s))
             (length (cow-box-vec s)))))
  (:op :snapshot ((s (:ref :mseq)))
    (:weight 3)
    (:returns (:abstract :pseq))
    (:ref (lambda (s) (copy-list (car s))))
    (:cand (lambda (s)
             ;; BUG: exports underlying vector without marking :shared
             (cow-box-vec s)))))

(let ((cow-report
        (zk-pbt:for-all
            ((sc (zk-pbt:model-scenarios 'buggy-cow-seq :min 2 :max 12)))
          (:name demo-buggy-cow-model)
          (:seed 11)
          (:trials 60)
          (:check nil)
          (zk-pbt:run-model 'buggy-cow-seq sc))))
  (zk-test:print-report cow-report))

;; E. Herlihy & Wing call/return history linearizability check (including pending calls).
(format t "~&~%Linearizable overlapping write/read history => ~S~%"
        (zk-pbt:linearizable-history-p
         'atomic-counter 0
         '((:call c1 t1 (:inc))
           (:call c2 t2 (:inc))
           (:return c2 (1))
           (:return c1 (2)))))
(format t "Linearizable pending in-flight call         => ~S~%"
        (zk-pbt:linearizable-history-p
         'atomic-counter 0
         '((:call c1 t1 (:inc))
           (:call c2 t2 (:inc))
           (:return c2 (2)))))
(format t "Non-linearizable duplicate-1 lost update     => ~S~%"
        (zk-pbt:linearizable-history-p
         'atomic-counter 0
         '((:call c1 t1 (:inc))
           (:call c2 t2 (:inc))
           (:return c2 (1))
           (:return c1 (1)))))

;; F. Fine-grained step-level RMW race via interleaved-steps & interleaving-schedules.
(let ((rmw-report
        (zk-pbt:for-all ((sched (zk-pbt:interleaving-schedules 2 :min 0 :max 8)))
          (:name demo-interleaved-rmw)
          (:seed 42)
          (:trials 30)
          (:check nil)
          (let ((counter (list 0))
                (tmp0 0)
                (tmp1 0))
            (zk-pbt:run-interleaved
             (list (zk-pbt:interleaved-steps
                     (setq tmp0 (car counter))
                     (setf (car counter) (1+ tmp0)))
                   (zk-pbt:interleaved-steps
                     (setq tmp1 (car counter))
                     (setf (car counter) (1+ tmp1))))
             sched)
            (= 2 (car counter))))))
  (zk-test:print-report rmw-report))

;; G. Generator distribution preview for REPL exploration.
(format t "~%")
(zk-pbt:inspect-distribution (zk-pbt:type-gen '(or null (integer 1 3)))
                             :count 100 :seed 42)
