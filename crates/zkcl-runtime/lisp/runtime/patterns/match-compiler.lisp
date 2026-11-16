;; Normalized tests: (:bind name), (:wild), (:lit datum), (:constant-ref sym known-p val),
;; (:node key children), (:and ...), (:or ...), (:type spec child), (:range kind bounds child),
;; (:view fn-form child), (:view-values fn-form children), (:compare form test-form has-test-p),
;; (:vector-rest prefix rest span-p suffix), (:special head payload...).

(defun valid-view-function-p (fn)
  (or (symbolp fn)
      (and (consp fn) (list-length fn)
           (if (eq (car fn) 'lambda) (not (null (cdr fn))) t))))

(defun normalize (p env &optional expanding)
  (cond ((wildcard-p p) '(:wild))
        ((or (null p) (eq p t) (keywordp p) (numberp p) (characterp p) (stringp p))
         (list :lit p))
        ((symbolp p)
         (unless (and (variable-name-p p) (not (nth-value 1 (zk::%pattern-variable-info p env))))
           (error "Invalid binding ~S; use (:const name) for constants" p))
         (list :bind p))
        ((atom p) (error "Invalid pattern ~S" p))
        (t
         (let ((head (car p)) (args (cdr p)))
           (flet ((sub (x) (normalize x env expanding)))
             (cond
               ((eq head 'quote) (arity p 1) (list :lit (first args)))
               ((eq head :const)
                (arity p 1)
                (multiple-value-bind (name known value) (resolve-constant-form (first args) env)
                  (list :constant-ref name known value)))
               ((eq head :=) (normalize-comparison args env))
               ((eq head 'cons) (arity p 2) (list :node 'cons (mapcar #'sub args)))
               ((eq head 'vector) (normalize-vector args #'sub))
               ((member head '(and or))
                (cons (if (eq head 'and) :and :or) (mapcar #'sub args)))
               ((eq head 'type)
                (unless (member (length args) '(1 2)) (error "Invalid type pattern ~S" p))
                (let* ((spec (first args))
                       (child (if (cdr args) (sub (second args)) '(:wild)))
                       (interval (integer-type-range spec)))
                  (if interval
                      (list :range :integer interval child)
                      (list :type spec child))))
               ((eq head 'zk:range)
                (unless (member (length args) '(2 3)) (error "Invalid range pattern ~S" p))
                (normalize-range (first args) (second args)
                                 (if (cddr args) (sub (third args)) '(:wild)) env))
               ((eq head 'zk:view)
                (arity p 2)
                (let ((fn (first args)) (pat (second args)))
                  (unless (and (not (member fn '(t nil))) (not (keywordp fn))
                               (valid-view-function-p fn))
                    (error "Expected view function or lambda in ~S" p))
                  (if (and (consp pat) (eq (car pat) :values))
                      (list :view-values fn (mapcar #'sub (cdr pat)))
                      (list :view fn (sub pat)))))
               ((gethash head *special-patterns*)
                (funcall (special-pattern-normalizer (gethash head *special-patterns*))
                         args #'sub env))
               ((gethash head *constructors*)
                (arity p (length (cdr (gethash head *constructors*))))
                (list :node head (mapcar #'sub args)))
               ((gethash head *expanders*)
                (when (or (member p expanding :test #'equal) (>= (length expanding) 128))
                  (error "Recursive pattern expansion at ~S" p))
                (normalize (zk:pattern-expand-1 p env) env (cons p expanding)))
               (t (error "Unknown pattern head ~S" head))))))))

(defun merge-names (sets)
  (let ((all nil))
    (dolist (names sets all)
      (dolist (name names)
        (when (member name all) (error "Duplicate pattern binding ~S" name))
        (push name all)))))

(defun bindings (p)
  (case (car p)
    (:bind (list (second p)))
    (:node (merge-names (mapcar #'bindings (third p))))
    (:vector-rest
     (merge-names (mapcar #'bindings (append (second p) (list (third p)) (fifth p)))))
    (:view-values (merge-names (mapcar #'bindings (third p))))
    (:range (bindings (fourth p)))
    (:and (merge-names (mapcar #'bindings (cdr p))))
    (:or (let ((sets (mapcar #'bindings (cdr p))))
           (dolist (s (cdr sets))
             (when (set-exclusive-or (first sets) s)
               (error "OR alternatives bind different variables: ~S" sets)))
           (first sets)))
    ((:view :type) (bindings (third p)))
    (:special (funcall (special-pattern-bindings (gethash (second p) *special-patterns*)) p))))

(defun head-test (key v)
  (cond ((eq key 'cons) `(consp ,v))
        ((consp key) `(and (vectorp ,v) (= (length ,v) ,(second key))))
        (t `(typep ,v ',key))))

(defun projection (key i v)
  (cond ((eq key 'cons) (list (if (zerop i) 'car 'cdr) v))
        ((consp key) (list 'aref v i))
        (t (list 'zk::%pattern-field v (list 'quote (nth i (cdr (gethash key *constructors*))))))))

(defun literal-test (source value)
  (let ((op (cond ((symbolp value) 'eq)
                  ((or (numberp value) (characterp value)) 'eql)
                  (t 'equal))))
    (list op source (list 'quote value))))

(defun test-path (test success miss)
  (if miss `(if ,test ,success ,miss) `(when ,test ,success)))

(defun view-call-form (fn v)
  (cond ((symbolp fn) (list fn v))
        ((and (consp fn) (eq (car fn) 'lambda)) (list fn v))
        (t (list 'funcall fn v))))

(defun emit-pattern (p source success targets &optional head-matched-p assign miss)
  ;; SUCCESS appears once. Ordinary bindings become LETs. Only OR alternatives
  ;; need to write into shared bindings before continuing with the same body.
  (case (car p)
    (:wild success)
    (:bind (let ((name (cdr (assoc (second p) targets))))
             (if assign `(progn (setq ,name ,source) ,success)
                 `(let ((,name ,source)) (declare (ignorable ,name)) ,success))))
    (:lit (if head-matched-p success (test-path (literal-test source (second p)) success miss)))
    (:constant-ref (test-path `(equal ,source (symbol-value ',(second p))) success miss))
    (otherwise
     (let* ((v (if (symbolp source) source (gensym "FIELD")))
            (code
             (case (car p)
               (:vector-rest (emit-vector (second p) (third p) (fourth p) (fifth p)
                                          v success targets assign miss))
               (:node
                (let ((key (second p)) (index (length (third p))) (body success))
                  (if (consp key)
                      (emit-vector (third p) nil nil nil v success targets assign miss)
                      (progn
                        (dolist (child (reverse (third p)))
                          (decf index)
                          (unless (eq (car child) :wild)
                            (setf body (emit-pattern child (projection key index v) body targets nil assign miss))))
                        (if head-matched-p body (test-path (head-test key v) body miss))))))
               (:and (reduce (lambda (q body) (emit-pattern q v body targets nil assign miss))
                             (cdr p) :from-end t :initial-value success))
               (:or
                (let* ((names (unless assign (mapcar (lambda (name) (cdr (assoc name targets))) (bindings p))))
                       (test (cons 'or (mapcar (lambda (q) (emit-pattern q v t targets nil t)) (cdr p))))
                       (body (test-path test success miss)))
                  (if names `(let ,names (declare (ignorable ,@names)) ,body) body)))
               (:type (test-path `(typep ,v ',(second p))
                                 (emit-pattern (third p) v success targets nil assign miss) miss))
               (:range (test-path (range-test (second p) (third p) v)
                                  (emit-pattern (fourth p) v success targets nil assign miss) miss))
               (:compare
                (let ((expected (gensym "EXPECTED")))
                  `(let ((,expected ,(second p)))
                     ,(test-path (if (fourth p) `(funcall ,(third p) ,v ,expected)
                                     `(equal ,v ,expected)) success miss))))
               (:view-values
                (emit-value-row (third p) (view-call-form (second p) v) success targets assign miss))
               (:view
                (let ((call (view-call-form (second p) v)) (child (third p)))
                  ;; Even a discarded view is evaluated. Its effects belong to
                  ;; this pattern, including any nonlocal transfer.
                  (if (eq (car child) :wild) `(progn ,call ,success)
                      (emit-pattern child call success targets nil assign miss))))
               (:special
                (funcall (special-pattern-emitter (gethash (second p) *special-patterns*))
                         p v success targets assign miss)))))
       (if (eq v source) code `(let ((,v ,source)) (declare (ignorable ,v)) ,code))))))

(defun infer-input-type (expr env)
  (cond ((and (consp expr) (eq (car expr) 'the)) (second expr))
        ((and (consp expr) (symbolp (car expr))
              (gethash (car expr) *constructors*)
              (not (nth-value 2 (zk::%pattern-variable-info (car expr) env)))
              (fboundp (car expr))
              (eq (fdefinition (car expr)) (gethash (car expr) *constructor-functions*)))
         (car expr))
        ((and (consp expr) (eq (car expr) 'quote))
         (let ((value (second expr)))
           (cond ((consp value) 'cons)
                 ((and (vectorp value) (typep value 'simple-array))
                  (list 'simple-array '* (list (length value))))
                 ((vectorp value) 'vector)
                 (t (list 'eql value)))))
        ((or (keywordp expr) (numberp expr) (characterp expr) (stringp expr)
             (eq expr t) (null expr)) (list 'eql expr))
        ((symbolp expr) (zk::%pattern-variable-info expr env))
        (t t)))

(defstruct (match-row (:type list)
                      (:constructor make-match-row
                          (patterns variables guard body source-pattern guard-may-fail-p)))
  patterns variables guard body source-pattern guard-may-fail-p)

(defun compile-rows (clauses width input-mode env)
  (loop for clause in clauses collect
    (destructuring-bind (head &rest body) clause
      (let* ((row (cond ((null input-mode) (list head))
                        ((wildcard-p head) (make-list width :initial-element head))
                        ((listp head) head)
                        (t (error "Expected a pattern row, got ~S" head))))
             (len (length row))
             (guarded (eq (car body) :when)))
        (when (or (> len width) (and (< len width) (not (eq input-mode :values))))
          (error "Expected ~D pattern columns, got ~D in ~S" width len head))
        (when (and guarded (null (cdr body))) (error "Missing guard in ~S" clause))
        (let* ((ps (append (mapcar (lambda (p) (normalize p env)) row) (wilds (- width len))))
               (vars (merge-names (mapcar #'bindings ps))))
          (make-match-row ps vars (if guarded (second body) t) (if guarded (cddr body) body)
                          head (and guarded (not (eq (second body) t)))))))))

(defun opaque-p (p)
  (case (car p)
    ((:view :view-values :type :constant-ref :compare :vector-rest) t)
    (:range (opaque-p (fourth p)))
    (:node (some #'opaque-p (third p)))
    ((:and :or) (some #'opaque-p (cdr p)))
    (:special (funcall (special-pattern-opaque-p (gethash (second p) *special-patterns*)) p))))

(defun user-test-p (p)
  (case (car p)
    ((:view :view-values :compare) t)
    (:range (user-test-p (fourth p)))
    (:vector-rest (some #'user-test-p (append (second p) (list (third p)) (fifth p))))
    (:type (or (not (side-effect-free-type-p (second p)))
               (user-test-p (third p))))
    (:node (some #'user-test-p (third p)))
    ((:and :or) (some #'user-test-p (cdr p)))
    (:special (funcall (special-pattern-user-test-p (gethash (second p) *special-patterns*)) p))))

(defun flat-head (p)
  (case (car p)
    ((:wild :bind) :any)
    (:lit (when (eql-literal-p (second p)) p))
    (:node (when (and (symbolp (second p)) (every #'universal-p (third p)))
             (list :node (second p))))))

(defun warn-unreachable-row (row preceding)
  (let ((name (and preceding (match-row-source-pattern preceding))))
    (if (and preceding (symbolp name) (not (wildcard-p name))
             (eq (caar (match-row-patterns preceding)) :bind))
        (warn "Unreachable pattern ~S: bare name ~S binds every value. Use (~S) for a nullary constructor."
              (match-row-source-pattern row) name name)
        (warn "Unreachable pattern ~S" (match-row-source-pattern row)))))

(defun check-flat-coverage (rows domain require-exhaustive-p)
  ;; One-column enumerations need only a set of covered heads, not a fresh
  ;; matrix specialization for each preceding prefix.
  (let ((keys (mapcar (lambda (row) (flat-head (car (match-row-patterns row)))) rows))
        (heads (finite-heads domain)))
    (when (or (member nil keys) (and (eq heads :open) (not (eq domain t))))
      (return-from check-flat-coverage nil))
    (let ((covered (make-hash-table :test 'equal))
          (allowed (make-hash-table :test 'equal))
          (remaining nil)
          (catch-all nil))
      (unless (eq heads :open)
        (dolist (h heads) (setf (gethash h allowed) t))
        (setf remaining (hash-table-count allowed)))
      (loop for row in rows for key in keys do
        (cond ((or catch-all (eql remaining 0) (gethash key covered)
                   (and remaining (not (eq key :any)) (not (gethash key allowed))))
               (warn-unreachable-row row catch-all))
              ((not (match-row-guard-may-fail-p row))
               (if (eq key :any) (setf catch-all row)
                   (progn (setf (gethash key covered) t) (when remaining (decf remaining)))))))
      (when (and require-exhaustive-p (not catch-all) (not (eql remaining 0)))
        (let* ((h (and remaining (find-if-not (lambda (k) (gethash k covered)) heads)))
               (w (if h (witness-head h (make-list (head-width h) :initial-element '_)) '_)))
          (error (if (some #'match-row-guard-may-fail-p rows)
                     "Guard may fail; missing unconditional coverage of ~S"
                     "Non-exhaustive match; missing pattern ~S") w)))
      t)))

(defun coverage-proof-key (rows domains require-exhaustive-p)
  ;; Keep the cheap cases on their existing path. The encoder bounds the
  ;; traversal of source type specifiers and accepts only proper list structure.
  (when (and *coverage-proofs* (nthcdr 7 rows) (not (nthcdr 256 rows))
             (notany #'match-row-guard-may-fail-p rows))
    (zk::%pattern-coverage-key (list (mapcar #'match-row-patterns rows)
                                     domains require-exhaustive-p))))

(defun check-coverage (rows domains require-exhaustive-p)
  (when (and (= 1 (length domains))
             (check-flat-coverage rows (car domains) require-exhaustive-p))
    (return-from check-coverage nil))
  (let ((key (coverage-proof-key rows domains require-exhaustive-p)))
    (when (and key (find key (coverage-proofs-keys *coverage-proofs*) :test #'equal))
      (return-from check-coverage nil))
    (check-matrix-coverage rows domains require-exhaustive-p
                           (and key (every #'coverage-proof-domain-p domains)
                                (every (lambda (row)
                                         (every #'coverage-proof-pattern-p (match-row-patterns row)))
                                       rows)
                                key))))

(defun check-matrix-coverage (rows domains require-exhaustive-p proof-key)
  (let ((*coverage-cache* (make-hash-table :test 'equal)) (matrix nil) (catch-all nil)
        (index (when (and domains (nthcdr 8 rows)) (make-hash-table :test 'equal))))
    (dolist (row rows)
      (if catch-all
          (progn (setq proof-key nil) (warn-unreachable-row row catch-all))
          (let* ((patterns (match-row-patterns row))
                 (head (and index (disjoint-head (car patterns))))
                 (reachable (usefulness-witness (if head (indexed-rows index head) matrix)
                                                patterns domains)))
            (unless reachable
              ;; A warning handler can change state before the next row.
              (setq proof-key nil)
              (warn-unreachable-row row nil))
            (unless (or (match-row-guard-may-fail-p row) (some #'user-test-p patterns))
              (push patterns matrix)
              (when index (index-row index patterns))
              (when (every #'universal-p patterns) (setq catch-all row))))))
    (when (and require-exhaustive-p (not catch-all))
      (let ((witness (usefulness-witness matrix (wilds (length domains)) domains)))
        (when witness
          (error (if (some (lambda (row)
                             (or (match-row-guard-may-fail-p row)
                                 (some #'opaque-p (match-row-patterns row)))) rows)
                     "Cannot prove exhaustive coverage of ~S (guard or opaque test); uncovered space ~S"
                     "Non-exhaustive match on ~S; missing pattern ~S")
                 domains (butlast witness)))))
    (when proof-key (remember-coverage-proof proof-key))))

;;; Maranget (2008) Decision Tree DAG Compiler
(defstruct (dt-arm (:type list) (:constructor make-dt-arm (index vars guard body)))
  index vars guard body)

(defstruct (dt-row (:type list) (:constructor make-dt-row (arm patterns env)))
  arm patterns env)

(defun pure-structural-p (p)
  (case (car p)
    ((:wild :bind :lit) t)
    (:constant-ref nil)
    (:node (every #'pure-structural-p (third p)))
    (:range (pure-structural-p (fourth p)))
    (:type (and (side-effect-free-type-p (second p)) (pure-structural-p (third p))))
    ((:and :or) (every #'pure-structural-p (cdr p)))
    (:special (not (funcall (special-pattern-user-test-p (gethash (second p) *special-patterns*)) p)))
    (otherwise nil)))

(defun unwrap-as-pattern (p env var)
  ;; Peel pure outer :bind/:wild wrappers inside :and so as-patterns like
  ;; (and whole (ok n)) participate in constructor switching.
  (loop while (eq (car p) :and) do
    (let ((arms (cdr p)))
      (cond ((null arms) (return (values '(:wild) env)))
            ((null (cdr arms)) (setq p (car arms)))
            ((eq (caar arms) :wild) (setq p (cons :and (cdr arms))))
            ((eq (caar arms) :bind)
             (push (cons (second (car arms)) var) env)
             (setq p (cons :and (cdr arms))))
            (t (return)))))
  (values p env))

(defun variant-node-p (p)
  (and (eq (car p) :node) (symbolp (second p)) (not (eq (second p) 'cons))))

(defun cons-node-p (p)
  (and (eq (car p) :node) (eq (second p) 'cons)))

(defun eql-lit-node-p (p)
  (and (eq (car p) :lit) (eql-literal-p (second p))))

(defun patterns-disjoint-p (a b)
  (let ((ka (car a)) (kb (car b)))
    (cond
      ((and (eql-lit-node-p a) (eql-lit-node-p b)) (not (eql (second a) (second b))))
      ((and (variant-node-p a) (variant-node-p b)) (not (eq (second a) (second b))))
      ((and (cons-node-p a) (eql-lit-node-p b)) t)
      ((and (eql-lit-node-p a) (cons-node-p b)) t)
      ((and (variant-node-p a) (or (cons-node-p b) (eql-lit-node-p b))) t)
      ((and (variant-node-p b) (or (cons-node-p a) (eql-lit-node-p a))) t)
      ((and (eq ka :range) (eq kb :range) (eq (second a) (second b)))
       (let ((la (first (third a))) (ha (second (third a)))
             (lb (first (third b))) (hb (second (third b))))
         (or (and ha lb (< ha lb)) (and hb la (< hb la)))))
      (t nil))))

(defun splittable-or-arms (p)
  (when (eq (car p) :or)
    (let ((arms (cdr p)))
      (when (and (cdr arms)
                 (every #'pure-structural-p arms)
                 (or (null (bindings (car arms)))
                     (loop for (a . rest) on arms always
                       (every (lambda (b) (patterns-disjoint-p a b)) rest))))
        arms))))

(defun expand-disjoint-or-in-row (row)
  ;; Flatten the leftmost splittable :or column in a pure prefix of a row.
  (let ((patterns (dt-row-patterns row)))
    (loop for p in patterns for col from 0 do
      (unless (pure-structural-p p) (return (list row)))
      (let ((arms (splittable-or-arms p)))
        (when arms
          (return-from expand-disjoint-or-in-row
            (mapcan (lambda (arm)
                      (expand-disjoint-or-in-row
                       (make-dt-row (dt-row-arm row)
                                    (loop for q in patterns for i from 0
                                          collect (if (= i col) arm q))
                                    (dt-row-env row))))
                    arms)))))
    (list row)))

(defun column-switch-kind (p)
  (cond ((variant-node-p p) :tag)
        ((cons-node-p p) :cons)
        ((eql-lit-node-p p) :lit)
        ((eq (car p) :range) :range)
        ((and (eq (car p) :type) (side-effect-free-type-p (second p))) :type)))

(defun pure-left-columns-p (row col)
  (loop for p in (dt-row-patterns row) for i below col always (pure-structural-p p)))

(defun column-necessity-score (rows col kind)
  ;; Count contiguous prefix rows that match the same switch family on `col`
  ;; while keeping all columns left of `col` pure.
  (let ((count 0) (keys nil) (has-default nil))
    (dolist (row rows)
      (unless (pure-left-columns-p row col) (return))
      (let* ((p (nth col (dt-row-patterns row)))
             (rk (column-switch-kind p)))
        (cond ((universal-p p)
               (when (and (>= (length keys) 8)
                          (or (some (lambda (q) (not (universal-p q))) (dt-row-patterns row))
                              (not (eq (dt-arm-guard (aref *dt-arms* (dt-row-arm row))) t))))
                 (return))
               (setf has-default t)
               (incf count))
              ((eq rk kind)
               (pushnew (second p) keys :test #'eql)
               (incf count))
              ((and (eq kind :cons) (equal p '(:lit nil)))
               (pushnew :nil keys)
               (incf count))
              (t (return)))))
    (values count (length keys) has-default)))

(defun choose-dispatch-column (rows)
  (let* ((first (car rows))
         (patterns (dt-row-patterns first))
         (best-col nil)
         (best-kind nil)
         (best-score -1))
    (loop for p in patterns for col from 0 do
      (multiple-value-bind (unwrapped) (unwrap-as-pattern p nil nil)
        (let ((kind (column-switch-kind unwrapped)))
          (when kind
            (multiple-value-bind (prefix-len distinct has-default)
                (column-necessity-score rows col kind)
              (declare (ignore distinct has-default))
              ;; Favor longer switchable prefixes; earlier columns win ties.
              (when (> prefix-len best-score)
                (setf best-col col best-kind kind best-score prefix-len))))))
      (unless (pure-structural-p p) (return)))
    (values best-col best-kind)))

(defun drop-column (patterns col &optional replacement)
  (loop for p in patterns for i from 0
        if (= i col) append replacement
        else collect p))

(defun drop-input (inputs col &optional replacement)
  (loop for v in inputs for i from 0
        if (= i col) append replacement
        else collect v))

(defun bind-universal-column (row col input-var replacement)
  (let* ((p (nth col (dt-row-patterns row)))
         (env (dt-row-env row)))
    (multiple-value-bind (q new-env) (unwrap-as-pattern p env input-var)
      (when (eq (car q) :bind)
        (push (cons (second q) input-var) new-env))
      (make-dt-row (dt-row-arm row)
                   (drop-column (dt-row-patterns row) col replacement)
                   new-env))))

(defun collect-prefix (rows col kind)
  (let ((prefix nil) (rest rows))
    (loop while rest do
      (let* ((row (car rest))
             (p (nth col (dt-row-patterns row)))
             (ok (and (pure-left-columns-p row col)
                      (multiple-value-bind (q) (unwrap-as-pattern p nil nil)
                        (or (universal-p q)
                            (eq (column-switch-kind q) kind)
                            (and (eq kind :cons) (equal q '(:lit nil))))))))
        (unless ok (return))
        (when (and (>= (length prefix) 8)
                   (universal-p p)
                   (or (some (lambda (q) (not (universal-p q))) (dt-row-patterns row))
                       (not (eq (dt-arm-guard (aref *dt-arms* (dt-row-arm row))) t))))
          (return))
        ;; User tests can mutate sibling fields before their own patterns run.
        ;; Compile that row sequentially instead of projecting the whole row.
        (when (and (member kind '(:cons :tag))
                   (some #'user-test-p (dt-row-patterns row)))
          (return))
        (push (pop rest) prefix)
        ;; A failed guard may mutate fields observed by later rows.
        (when (and (member kind '(:cons :tag))
                   (not (eq (dt-arm-guard (aref *dt-arms* (dt-row-arm row))) t)))
          (return))
        ;; A guard or nested user test can change the class hierarchy even when
        ;; the matched object stays intact. The tail must test its current type.
        (when (and (eq kind :type)
                   (or (not (eq (dt-arm-guard (aref *dt-arms* (dt-row-arm row))) t))
                       (not (every #'pure-structural-p (dt-row-patterns row)))))
          (return))))
    (values (nreverse prefix) rest)))

(defvar *dt-arms*)
(defvar *dt-top-inputs*)
(defvar *dt-ir-memo*)
(defvar *dt-dag-defs*)
(defvar *dt-dag-counts*)
(defvar *dt-arm-counts*)
(defvar *dt-next-dag*)

(defun record-arm-ref (arm-index)
  (incf (aref *dt-arm-counts* arm-index)))

(defun all-envs-top-level-p (rows)
  (every (lambda (row)
           (every (lambda (entry) (member (cdr entry) *dt-top-inputs*))
                  (dt-row-env row)))
         rows))

(defun build-dt (rows inputs)
  (cond
    ((null rows) '(:miss))
    ((and *dt-ir-memo*
          (every (lambda (v) (member v *dt-top-inputs*)) inputs)
          (all-envs-top-level-p rows))
     (let ((key (list rows inputs)))
       (multiple-value-bind (label found) (gethash key *dt-ir-memo*)
         (if found
             (progn (incf (gethash label *dt-dag-counts* 0))
                    (list :dag-ref label))
             (let* ((label (incf *dt-next-dag*))
                    (ir (progn
                          (setf (gethash key *dt-ir-memo*) label)
                          (setf (gethash label *dt-dag-counts*) 1)
                          (build-dt-uncached rows inputs))))
               (setf (gethash label *dt-dag-defs*) ir)
               (list :dag-ref label))))))
    (t (build-dt-uncached rows inputs))))

(defun build-dt-uncached (rows inputs)
  (let ((first (car rows)))
    (when (every #'universal-p (dt-row-patterns first))
      (let ((env (dt-row-env first)))
        (loop for p in (dt-row-patterns first) for v in inputs do
          (when (eq (car p) :bind) (push (cons (second p) v) env)))
        (let* ((arm-idx (dt-row-arm first))
               (arm (aref *dt-arms* arm-idx)))
          (record-arm-ref arm-idx)
          (if (eq (dt-arm-guard arm) t)
              (return-from build-dt-uncached (list :leaf arm-idx env))
              (return-from build-dt-uncached
                (list :guard-branch arm-idx env (build-dt (cdr rows) inputs)))))))
    (multiple-value-bind (col kind) (choose-dispatch-column rows)
      (if (or (null col)
              (and (member kind '(:cons :tag))
                   (some #'user-test-p (dt-row-patterns first))))
          (let ((arm-idx (dt-row-arm first)))
            (record-arm-ref arm-idx)
            (list :seq first inputs (build-dt (cdr rows) inputs)))
          (multiple-value-bind (prefix tail) (collect-prefix rows col kind)
            (let ((main
                   (case kind
                     (:tag (build-tag-switch prefix col inputs))
                     (:cons (build-cons-split prefix col inputs))
                     (:lit (build-lit-switch prefix col inputs))
                     (:range (build-range-split prefix col inputs))
                     (:type (build-type-split prefix col inputs)))))
              (if tail
                  (list :with-fallback main (build-dt tail inputs))
                  main)))))))

(defun used-slot-indices (rows col key width)
  (let ((used (make-array width :initial-element nil)))
    (dolist (row rows)
      (let ((p (unwrap-as-pattern (nth col (dt-row-patterns row)) nil nil)))
        (when (and (eq (car p) :node) (eql (second p) key))
          (loop for child in (third p) for i from 0 do
            (unless (eq (car child) :wild) (setf (aref used i) t))))))
    (loop for i below width when (aref used i) collect i)))

(defun build-tag-switch (rows col inputs)
  (let ((v (nth col inputs))
        (tags nil)
        (buckets (make-hash-table :test 'eql))
        (universals nil))
    (dolist (row rows)
      (multiple-value-bind (p env) (unwrap-as-pattern (nth col (dt-row-patterns row)) (dt-row-env row) v)
        (if (universal-p p)
            (push (list row env p) universals)
            (let ((tag (second p)))
              (unless (gethash tag buckets) (push tag tags))
              (push (list row env p) (gethash tag buckets))))))
    (setf tags (nreverse tags))
    (let* ((branches
            (loop for tag in tags collect
              (let* ((width (node-width tag))
                     (used (used-slot-indices rows col tag width))
                     (fvars (mapcar (lambda (idx)
                                      (declare (ignore idx))
                                      (gensym "FIELD"))
                                    used))
                     (sub-inputs (drop-input inputs col fvars))
                     (wild-fill (wilds (length used)))
                     (sub-rows
                      (loop for row in rows
                            for p-raw = (nth col (dt-row-patterns row))
                            append
                              (multiple-value-bind (p env) (unwrap-as-pattern p-raw (dt-row-env row) v)
                                (cond ((universal-p p)
                                       (when (eq (car p) :bind) (push (cons (second p) v) env))
                                       (list (make-dt-row (dt-row-arm row)
                                                          (drop-column (dt-row-patterns row) col wild-fill)
                                                          env)))
                                      ((eq (second p) tag)
                                       (let* ((children (third p))
                                              (picked (mapcar (lambda (i) (nth i children)) used)))
                                         (list (make-dt-row (dt-row-arm row)
                                                            (drop-column (dt-row-patterns row) col picked)
                                                            env)))))))))
                (list tag used fvars (build-dt sub-rows sub-inputs)))))
           (default-rows (loop for row in rows
                               for p-raw = (nth col (dt-row-patterns row))
                               append
                                 (multiple-value-bind (p env) (unwrap-as-pattern p-raw (dt-row-env row) v)
                                   (when (universal-p p)
                                     (when (eq (car p) :bind) (push (cons (second p) v) env))
                                     (list (make-dt-row (dt-row-arm row)
                                                        (drop-column (dt-row-patterns row) col nil)
                                                        env))))))
           (default-ir (build-dt default-rows (drop-input inputs col nil))))
      (list :tag-switch v default-ir branches))))

(defun build-cons-split (rows col inputs)
  (let* ((v (nth col inputs))
         (used (used-slot-indices rows col 'cons 2))
         (fvars (mapcar (lambda (i) (gensym (if (zerop i) "CAR" "CDR"))) used))
         (sub-inputs (drop-input inputs col fvars))
         (wild-fill (wilds (length used)))
         (has-nil (some (lambda (row)
                          (multiple-value-bind (p) (unwrap-as-pattern (nth col (dt-row-patterns row)) nil nil)
                            (equal p '(:lit nil))))
                        rows))
         (cons-rows
          (loop for row in rows
                for p-raw = (nth col (dt-row-patterns row))
                append
                  (multiple-value-bind (p env) (unwrap-as-pattern p-raw (dt-row-env row) v)
                    (cond ((universal-p p)
                           (when (eq (car p) :bind) (push (cons (second p) v) env))
                           (list (make-dt-row (dt-row-arm row)
                                              (drop-column (dt-row-patterns row) col wild-fill)
                                              env)))
                          ((cons-node-p p)
                           (let* ((children (third p))
                                  (picked (mapcar (lambda (i) (nth i children)) used)))
                             (list (make-dt-row (dt-row-arm row)
                                                (drop-column (dt-row-patterns row) col picked)
                                                env))))))))
         (nil-rows
          (when has-nil
            (loop for row in rows
                  for p-raw = (nth col (dt-row-patterns row))
                  append
                    (multiple-value-bind (p env) (unwrap-as-pattern p-raw (dt-row-env row) v)
                      (cond ((universal-p p)
                             (when (eq (car p) :bind) (push (cons (second p) v) env))
                             (list (make-dt-row (dt-row-arm row)
                                                (drop-column (dt-row-patterns row) col nil)
                                                env)))
                            ((equal p '(:lit nil))
                             (list (make-dt-row (dt-row-arm row)
                                                (drop-column (dt-row-patterns row) col nil)
                                                env))))))))
         (default-rows
          (loop for row in rows
                for p-raw = (nth col (dt-row-patterns row))
                append
                  (multiple-value-bind (p env) (unwrap-as-pattern p-raw (dt-row-env row) v)
                    (when (universal-p p)
                      (when (eq (car p) :bind) (push (cons (second p) v) env))
                      (list (make-dt-row (dt-row-arm row)
                                         (drop-column (dt-row-patterns row) col nil)
                                         env))))))
         (dropped-inputs (drop-input inputs col nil)))
    (list :cons-split v
          used fvars (build-dt cons-rows sub-inputs)
          (when has-nil (build-dt nil-rows dropped-inputs))
          (build-dt default-rows dropped-inputs))))

(defun build-lit-switch (rows col inputs)
  (let ((v (nth col inputs))
        (keys nil)
        (seen (make-hash-table :test 'eql)))
    (dolist (row rows)
      (multiple-value-bind (p) (unwrap-as-pattern (nth col (dt-row-patterns row)) nil nil)
        (when (eql-lit-node-p p)
          (let ((k (second p)))
            (unless (gethash k seen)
              (setf (gethash k seen) t)
              (push k keys))))))
    (setf keys (nreverse keys))
    (let* ((dropped-inputs (drop-input inputs col nil))
           (branches
            (loop for k in keys collect
              (let ((sub-rows
                     (loop for row in rows
                           for p-raw = (nth col (dt-row-patterns row))
                           append
                             (multiple-value-bind (p env) (unwrap-as-pattern p-raw (dt-row-env row) v)
                               (cond ((universal-p p)
                                      (when (eq (car p) :bind) (push (cons (second p) v) env))
                                      (list (make-dt-row (dt-row-arm row)
                                                         (drop-column (dt-row-patterns row) col nil)
                                                         env)))
                                     ((and (eql-lit-node-p p) (eql (second p) k))
                                      (list (make-dt-row (dt-row-arm row)
                                                         (drop-column (dt-row-patterns row) col nil)
                                                         env))))))))
                (list k (build-dt sub-rows dropped-inputs)))))
           (default-rows
            (loop for row in rows
                  for p-raw = (nth col (dt-row-patterns row))
                  append
                    (multiple-value-bind (p env) (unwrap-as-pattern p-raw (dt-row-env row) v)
                      (when (universal-p p)
                        (when (eq (car p) :bind) (push (cons (second p) v) env))
                        (list (make-dt-row (dt-row-arm row)
                                           (drop-column (dt-row-patterns row) col nil)
                                           env))))))
           (default-ir (build-dt default-rows dropped-inputs)))
      (list :lit-switch v default-ir branches))))

(defun build-range-split (rows col inputs)
  (let* ((v (nth col inputs))
         (first-p (unwrap-as-pattern (nth col (dt-row-patterns (car rows))) nil nil))
         (kind (second first-p))
         (bounds (third first-p))
         (then-rows nil)
         (else-rows nil))
    (dolist (row rows)
      (multiple-value-bind (p env) (unwrap-as-pattern (nth col (dt-row-patterns row)) (dt-row-env row) v)
        (cond ((universal-p p)
               (let ((u (bind-universal-column row col v (list '(:wild)))))
                 (push u then-rows)
                 (push u else-rows)))
              ((and (eq (car p) :range) (eq (second p) kind) (equal (third p) bounds))
               (push (make-dt-row (dt-row-arm row)
                                  (drop-column (dt-row-patterns row) col (list (fourth p)))
                                  env)
                     then-rows))
              (t
               (let ((kept (make-dt-row (dt-row-arm row)
                                        (drop-column (dt-row-patterns row) col (list p))
                                        env)))
                 (push kept then-rows)
                 (push kept else-rows))))))
    (list :test (range-test kind bounds v)
          (build-dt (nreverse then-rows) inputs)
          (build-dt (nreverse else-rows) inputs))))

(defun build-type-split (rows col inputs)
  (let* ((v (nth col inputs))
         (first-p (unwrap-as-pattern (nth col (dt-row-patterns (car rows))) nil nil))
         (spec (second first-p))
         (then-rows nil)
         (else-rows nil))
    (dolist (row rows)
      (multiple-value-bind (p env) (unwrap-as-pattern (nth col (dt-row-patterns row)) (dt-row-env row) v)
        (cond ((universal-p p)
               (let ((u (bind-universal-column row col v (list '(:wild)))))
                 (push u then-rows)
                 (push u else-rows)))
              ((and (eq (car p) :type) (equal (second p) spec))
               (push (make-dt-row (dt-row-arm row)
                                  (drop-column (dt-row-patterns row) col (list (third p)))
                                  env)
                     then-rows))
              (t
               (let ((kept (make-dt-row (dt-row-arm row)
                                        (drop-column (dt-row-patterns row) col (list p))
                                        env)))
                 (push kept then-rows)
                 (push kept else-rows))))))
    (list :test `(typep ,v ',spec)
          (build-dt (nreverse then-rows) inputs)
          (build-dt (nreverse else-rows) inputs))))

;;; Emit Lisp AST from Decision Tree / DAG IR
(defun arm-result-form (arm)
  (let ((body (dt-arm-body arm)))
    (cond ((null body) nil)
          ((cdr body) (cons 'progn body))
          (t (car body)))))

(defun wrap-let-env (env inner)
  (if env
      `(let ,(mapcar (lambda (entry) (list (car entry) (cdr entry))) env)
         (declare (ignorable ,@(mapcar #'car env)))
         ,inner)
      inner))

(defun literal-ir-value (ir)
  (when (and (consp ir) (eq (car ir) :leaf))
    (let* ((arm (aref *dt-arms* (second ir)))
           (form (arm-result-form arm)))
      (when (and (eq (dt-arm-guard arm) t)
                 (or (numberp form) (characterp form) (keywordp form) (member form '(t nil))
                     (and (consp form) (eq (car form) 'quote) (= (length form) 2)
                          (symbolp (second form)))))
        (list (if (and (consp form) (eq (car form) 'quote)) (second form) form))))))

(defun constant-switch-values (branches)
  (let ((vals nil))
    (dolist (br branches (nreverse vals))
      (let ((lit (literal-ir-value (car (last br)))))
        (unless lit (return-from constant-switch-values nil))
        (push (car lit) vals)))))

(defun dense-results (keys values)
  (when (every #'integerp keys)
    (let ((lo (reduce #'min keys)) (hi (reduce #'max keys)))
      (when (= (1+ (- hi lo)) (length keys))
        (let ((table (make-array (length keys))))
          (mapc (lambda (k v) (setf (svref table (- k lo)) v)) keys values)
          (values table lo hi))))))

(defun result-dispatch (subject keys values done default)
  (let ((input (gensym "KEY")) (value (gensym "RESULT")) (found (gensym "FOUND"))
        (miss (when default (list 'quote (car default)))))
    (flet ((hit (form) `(return-from ,done ,form)))
      (multiple-value-bind (dense lo hi) (dense-results keys values)
        (let* ((table (unless dense `(load-time-value
                                      (make-result-table ',(cdr keys) ',(cdr values)) t)))
               (lookup (when table `(gethash ,input ,table)))
               (expr
                `(let ((,input ,subject))
                   ,(cond
                      (dense
                       `(if (and (integerp ,input) (<= ,lo ,input) (<= ,input ,hi))
                            ,(hit `(svref ',dense ,(if (zerop lo) input (list '- input lo))))
                            ,@(when miss (list (hit miss)))))
                      (t
                       `(if ,(literal-test input (car keys))
                            ,(hit (list 'quote (car values)))
                            ,(cond (default `(return-from ,done (zk::%pattern-lookup ,input ,table ,miss)))
                                   ((member nil (cdr values))
                                    `(multiple-value-bind (,value ,found) ,lookup
                                       (when ,found ,(hit value))))
                                   (t `(let ((,value ,lookup)) (when ,value ,(hit value)))))))))))
          expr)))))

(defun ordinal-branches (index bodies start end)
  (if (= (1+ start) end)
      (aref bodies start)
      (let ((mid (floor (+ start end) 2)))
        `(if (< ,index ,mid)
             ,(ordinal-branches index bodies start mid)
             ,(ordinal-branches index bodies mid end)))))

(defvar *dt-arm-tags*)
(defvar *dt-arm-storages*)
(defvar *dt-dag-tags*)
(defvar *dt-done*)

(defun arm-shared-p (arm-idx)
  (> (aref *dt-arm-counts* arm-idx) 1))

(defun emit-arm-jump (arm-idx env-bindings)
  (let ((arm (aref *dt-arms* arm-idx)))
    (if (arm-shared-p arm-idx)
        (let ((storage (aref *dt-arm-storages* arm-idx))
              (tag (aref *dt-arm-tags* arm-idx)))
          (if storage
              `(progn
                 (setq ,@(loop for (user . slot) in storage
                               for val = (or (cdr (assoc user env-bindings)) user)
                               append (list slot val)))
                 (go ,tag))
              `(go ,tag)))
        `(return-from ,*dt-done* ,(arm-result-form arm)))))

(defun emit-indexed-switch (key-form keys branch-codes default-code)
  (let ((size (length keys))
        (index (gensym "ORDINAL"))
        (value (gensym "KEY")))
    `(let ((,value ,key-form))
       (if (eql ,value ',(car keys))
           ,(first branch-codes)
           (let ((,index (gethash ,value
                                  (load-time-value (make-dispatch-index ',(cdr keys)) t))))
             (if ,index
                 ,(ordinal-branches index (coerce (cdr branch-codes) 'vector) 0 (1- size))
                 ,@(when default-code (list default-code))))))))

(defun emit-ir (ir)
  (case (car ir)
    (:miss nil)
    (:dag-ref
     (let ((label (second ir)))
       (if (> (gethash label *dt-dag-counts* 0) 1)
           `(go ,(gethash label *dt-dag-tags*))
           (emit-ir (gethash label *dt-dag-defs*)))))
    (:with-fallback
     (let ((main (emit-ir (second ir)))
           (fb (emit-ir (third ir))))
       (cond ((null fb) main)
             ((null main) fb)
             (t `(progn ,main ,fb)))))
    (:leaf
     (let* ((arm-idx (second ir))
            (env (third ir)))
       (if (arm-shared-p arm-idx)
           (emit-arm-jump arm-idx env)
           (wrap-let-env env `(return-from ,*dt-done* ,(arm-result-form (aref *dt-arms* arm-idx)))))))
    (:guard-branch
     (let* ((arm-idx (second ir))
            (arm (aref *dt-arms* arm-idx))
            (env (third ir))
            (fail-code (emit-ir (fourth ir)))
            (hit (wrap-let-env env
                               `(when ,(dt-arm-guard arm)
                                  ,(if (arm-shared-p arm-idx)
                                       (let ((storage (aref *dt-arm-storages* arm-idx))
                                             (tag (aref *dt-arm-tags* arm-idx)))
                                         (if storage
                                             `(progn (setq ,@(loop for (u . s) in storage
                                                                   append (list s u)))
                                                     (go ,tag))
                                             `(go ,tag)))
                                       `(return-from ,*dt-done* ,(arm-result-form arm)))))))
       (if fail-code `(progn ,hit ,fail-code) hit)))
    (:seq
     (let* ((row (second ir))
            (inputs (third ir))
            (fail-code (emit-ir (fourth ir)))
            (arm-idx (dt-row-arm row))
            (arm (aref *dt-arms* arm-idx))
            (env (dt-row-env row))
            (patterns (dt-row-patterns row))
            (rem-vars (merge-names (mapcar #'bindings patterns)))
            (targets (append env (mapcar (lambda (v) (cons v (gensym "BINDING"))) rem-vars)))
            (guard (dt-arm-guard arm))
            (success (if (eq guard t)
                         (if (arm-shared-p arm-idx)
                             (emit-arm-jump arm-idx targets)
                             (wrap-let-env targets `(return-from ,*dt-done* ,(arm-result-form arm))))
                         (let ((hit-form (if (arm-shared-p arm-idx)
                                             (emit-arm-jump arm-idx nil)
                                             `(return-from ,*dt-done* ,(arm-result-form arm)))))
                           ;; Bind both env vars and target vars for guard evaluation.
                           (wrap-let-env targets `(when ,guard ,hit-form)))))
            (code success))
       (loop for p in (reverse patterns) for v in (reverse inputs) do
         (setf code (emit-pattern p v code targets nil nil nil)))
       (if fail-code `(progn ,code ,fail-code) code)))
    (:tag-switch
     (let* ((v (second ir))
            (default-ir (third ir))
            (branches (fourth ir))
            (default-code (emit-ir default-ir))
            (keys (mapcar #'first branches))
            (size (length keys))
            (const-vals (when (and (>= size 16) (every (lambda (br) (null (second br))) branches))
                          (constant-switch-values branches))))
       (when const-vals
         (let ((default-lit (literal-ir-value default-ir)))
           (when (or default-lit (eq (car default-ir) :miss))
             (return-from emit-ir
               (result-dispatch `(zk::%pattern-tag ,v) keys const-vals *dt-done* default-lit)))))
       (let ((branch-codes
              (loop for (tag used fvars sub-ir) in branches collect
                (let ((sub-code (emit-ir sub-ir)))
                  (if used
                      `(let ,(loop for idx in used for fv in fvars
                                   collect (list fv (projection tag idx v)))
                         (declare (ignorable ,@fvars))
                         ,sub-code)
                      sub-code)))))
         (cond
           ((= size 1)
            (let ((tag (first keys)) (bcode (first branch-codes)))
              (if default-code
                  `(if ,(head-test tag v) ,bcode ,default-code)
                  `(when ,(head-test tag v) ,bcode))))
           ((>= size 16)
            (emit-indexed-switch `(zk::%pattern-tag ,v) keys branch-codes default-code))
           (t
            (let ((tag-var (gensym "TAG")))
              `(let ((,tag-var (zk::%pattern-tag ,v)))
                 (cond ,@(loop for tag in keys for bcode in branch-codes
                               collect `((eq ,tag-var ',tag) ,bcode))
                       ,@(when default-code `((t ,default-code)))))))))))
    (:cons-split
     (let* ((v (second ir))
            (used (third ir))
            (fvars (fourth ir))
            (cons-code (emit-ir (fifth ir)))
            (nil-ir (sixth ir))
            (default-code (emit-ir (seventh ir)))
            (then-form (if used
                           `(let ,(loop for idx in used for fv in fvars
                                        collect (list fv (projection 'cons idx v)))
                              (declare (ignorable ,@fvars))
                              ,cons-code)
                           cons-code)))
       (if nil-ir
           (let ((nil-code (emit-ir nil-ir)))
             `(cond ((consp ,v) ,then-form)
                    ((null ,v) ,nil-code)
                    ,@(when default-code `((t ,default-code)))))
           (if default-code
               `(if (consp ,v) ,then-form ,default-code)
               `(when (consp ,v) ,then-form)))))
    (:lit-switch
     (let* ((v (second ir))
            (default-ir (third ir))
            (branches (fourth ir))
            (default-code (emit-ir default-ir))
            (keys (mapcar #'first branches))
            (size (length keys))
            (const-vals (when (>= size 16) (constant-switch-values branches))))
       (when const-vals
         (let ((default-lit (literal-ir-value default-ir)))
           (when (or default-lit (eq (car default-ir) :miss))
             (return-from emit-ir
               (result-dispatch v keys const-vals *dt-done* default-lit)))))
       (let ((branch-codes (mapcar (lambda (br) (emit-ir (second br))) branches)))
         (cond
           ((= size 1)
            (let ((k (first keys)) (bcode (first branch-codes)))
              (if default-code
                  `(if ,(literal-test v k) ,bcode ,default-code)
                  `(when ,(literal-test v k) ,bcode))))
           ((>= size 16)
            (emit-indexed-switch v keys branch-codes default-code))
           ((every #'symbolp keys)
            `(cond ,@(loop for k in keys for bcode in branch-codes
                           collect `(,(literal-test v k) ,bcode))
                   ,@(when default-code `((t ,default-code)))))
           (t
            `(case ,v
               ,@(loop for k in keys for bcode in branch-codes
                       collect `((,k) ,bcode))
               ,@(when default-code `((otherwise ,default-code)))))))))
    (:test
     (let ((test-form (second ir))
           (then-code (emit-ir (third ir)))
           (else-code (emit-ir (fourth ir))))
       (if else-code
           `(if ,test-form ,then-code ,else-code)
           `(when ,test-form ,then-code))))))

(defun decision-code (rows inputs done)
  (let* ((arm-count (length rows))
         (*dt-arms* (make-array arm-count))
         (*dt-top-inputs* inputs)
         (*dt-ir-memo* (make-hash-table :test 'equal))
         (*dt-dag-defs* (make-hash-table :test 'eql))
         (*dt-dag-counts* (make-hash-table :test 'eql))
         (*dt-arm-counts* (make-array arm-count :initial-element 0))
         (*dt-next-dag* 0)
         (dt-rows nil))
    (loop for row in rows for idx from 0 do
      (setf (aref *dt-arms* idx)
            (make-dt-arm idx (match-row-variables row) (match-row-guard row) (match-row-body row)))
      (dolist (expanded (expand-disjoint-or-in-row (make-dt-row idx (match-row-patterns row) nil)))
        (push expanded dt-rows)))
    (setf dt-rows (nreverse dt-rows))
    (let* ((root-ir (build-dt dt-rows inputs))
           (*dt-arm-tags* (make-array arm-count :initial-element nil))
           (*dt-arm-storages* (make-array arm-count :initial-element nil))
           (*dt-dag-tags* (make-hash-table :test 'eql))
           (*dt-done* done)
           (storage-vars nil)
           (shared-arms nil)
           (shared-dags nil))
      (dotimes (i arm-count)
        (when (> (aref *dt-arm-counts* i) 1)
          (push i shared-arms)
          (setf (aref *dt-arm-tags* i) (gensym (format nil "ARM-~D" i)))
          (let ((storage (mapcar (lambda (v)
                                   (let ((slot (gensym (symbol-name v))))
                                     (push slot storage-vars)
                                     (cons v slot)))
                                 (dt-arm-vars (aref *dt-arms* i)))))
            (setf (aref *dt-arm-storages* i) storage))))
      (maphash (lambda (label count)
                 (when (> count 1)
                   (push label shared-dags)
                   (setf (gethash label *dt-dag-tags*) (gensym (format nil "DAG-~D" label)))))
               *dt-dag-counts*)
      (let ((main-code (emit-ir root-ir)))
        (if (and (null shared-arms) (null shared-dags))
            (if (and (consp main-code) (eq (car main-code) 'progn))
                (cdr main-code)
                (when main-code (list main-code)))
            (let* ((arm-blocks
                    (loop for i in (nreverse shared-arms) append
                      (let* ((arm (aref *dt-arms* i))
                             (storage (aref *dt-arm-storages* i))
                             (body `(return-from ,done ,(arm-result-form arm)))
                             (wrapped (if storage
                                          `(let ,(mapcar (lambda (e) (list (car e) (cdr e))) storage)
                                             (declare (ignorable ,@(mapcar #'car storage)))
                                             ,body)
                                          body)))
                        (list (aref *dt-arm-tags* i) wrapped))))
                   (dag-blocks
                    (loop for label in (sort shared-dags #'<) append
                      (list (gethash label *dt-dag-tags*)
                            (emit-ir (gethash label *dt-dag-defs*)))))
                   (exit-tag (gensym "MATCH-MISS"))
                   (body-form
                    `(tagbody
                       ,@(when main-code (list main-code))
                       (go ,exit-tag)
                       ,@dag-blocks
                       ,@arm-blocks
                       ,exit-tag)))
              (if storage-vars
                  (list `(let ,(nreverse storage-vars)
                           (declare (ignorable ,@storage-vars))
                           ,body-form))
                  (list body-form))))))))

(defun source-mode (source)
  (and (consp source) (member (car source) '(:inputs :values)) (car source)))

(defun known-value-count-operator (form env)
  ;; Count the producer's results, independently of the pattern width.
  ;; A local operator or macro may use the same symbol with a different arity.
  (when (and (consp form) (member (car form) '(values gethash))
             (not (nth-value 2 (zk::%pattern-variable-info (car form) env))))
    (car form)))

(defun compile-match (matcher source clauses env &optional input-types prepared-rows else-form)
  (let* ((mode (source-mode source))
         (exprs (case mode (:inputs (cdr source)) (:values (arity source 1) (cdr source))
                  (otherwise (list source))))
         (multiple (eq mode :values))
         (width (if multiple
                    (reduce #'max clauses :key (lambda (c) (if (wildcard-p (car c)) 0
                                                              (length (car c)))) :initial-value 0)
                    (length exprs)))
         (inputs (loop repeat width collect (gensym "INPUT")))
         (domains (or input-types (if multiple (make-list width :initial-element t)
                                      (mapcar (lambda (expr) (infer-input-type expr env)) exprs))))
         (rows (or prepared-rows (compile-rows clauses width mode env)))
         (done (gensym "MATCH")))
    (check-coverage rows domains (eq matcher 'zk:match))
    (let* ((branches (decision-code rows inputs done))
           (only (first branches))
           (direct (and (null (rest branches)) (consp only) (eq (car only) 'return-from)
                        (eq (second only) done)))
           (match-error-possible-p
             (and (member matcher '(zk:match zk:ematch zk:cmatch)) (not direct)
                  (not (some (lambda (row) (and (not (match-row-guard-may-fail-p row))
                                               (every #'universal-p (match-row-patterns row)))) rows))))
           (producer (and multiple (known-value-count-operator (first exprs) env)))
           (count (and match-error-possible-p
                       (case producer (values (length (cdar exprs))) (gethash 2))))
           (bindings (if (and count (> count width))
                         (append inputs (loop repeat (- count width) collect (gensym "EXTRA"))) inputs))
           (all (and multiple match-error-possible-p (null count) (gensym "VALUES")))
           (tree
             (if direct (third only)
                 (let ((miss
                         (cond ((eq matcher 'zk:if-match) else-form)
                               ((not match-error-possible-p) '(error 'zk:match-error :inputs nil :patterns nil))
                               (t (let ((initial (or all
                                                  (cons 'list (if count (subseq bindings 0 count) inputs))))
                                        (patterns (mapcar #'car clauses)))
                                    (if (eq matcher 'zk:cmatch)
                                        `(cerror "Return NIL from this match" 'zk:match-error
                                                 :inputs ,initial :patterns ',patterns)
                                        `(error 'zk:match-error :inputs ,initial :patterns ',patterns)))))))
                   `(block ,done ,@branches ,miss)))))
      (cond ((eq producer 'values)
             ;; VALUES evaluates its arguments left to right in single-value
             ;; context. Unmatched extras must still run, even on success.
             (let* ((remaining (cdar exprs))
                    (inits (mapcar (lambda (b) (list b (pop remaining))) bindings)))
               `(let ,inits ,@remaining ,tree)))
            (all `(let ((,all (multiple-value-list ,(first exprs))))
                    (destructuring-bind (&optional ,@inputs &rest ,(gensym "EXTRA")) ,all ,tree)))
            (multiple `(multiple-value-bind ,bindings ,(first exprs) ,tree))
            (t `(let ,(mapcar #'list inputs exprs) ,tree))))))

(defmacro zk:match (source &body clauses &environment env) (compile-match 'zk:match source clauses env))
(defmacro zk:ematch (source &body clauses &environment env) (compile-match 'zk:ematch source clauses env))
(defmacro zk:cmatch (source &body clauses &environment env) (compile-match 'zk:cmatch source clauses env))

(defmacro zk:if-match (pattern source &rest tail &environment env)
  (let (guard then else)
    (cond
      ((and (consp tail) (eq (car tail) :when))
       (unless (and (cdr tail) (cddr tail) (null (cdddr tail)) (null (cddddr tail)))
         (unless (and (cdr tail) (cddr tail) (<= (length tail) 4))
           (error "Invalid IF-MATCH form ~S" `(zk:if-match ,pattern ,source ,@tail))))
       (setf guard (second tail)
             then (third tail)
             else (fourth tail))
       (compile-match 'zk:if-match source (list (list pattern :when guard then)) env nil nil else))
      ((and (consp tail) (<= (length tail) 2))
       (setf then (first tail)
             else (second tail))
       (compile-match 'zk:if-match source (list (list pattern then)) env nil nil else))
      (t (error "Invalid IF-MATCH form ~S" `(zk:if-match ,pattern ,source ,@tail))))))

(defmacro zk:when-match (pattern source &body body)
  (if (eq (car body) :when)
      (progn
        (unless (cdr body) (error "Missing guard in WHEN-MATCH"))
        `(zk:if-match ,pattern ,source :when ,(second body) (progn ,@(cddr body))))
      `(zk:if-match ,pattern ,source (progn ,@body))))

(defmacro zk:unless-match (pattern source &body body)
  (if (eq (car body) :when)
      (progn
        (unless (cdr body) (error "Missing guard in UNLESS-MATCH"))
        `(zk:if-match ,pattern ,source :when ,(second body) nil (progn ,@(cddr body))))
      `(zk:if-match ,pattern ,source nil (progn ,@body))))

(defmacro zk:let-match* (bindings &body body)
  (reduce (lambda (binding inner)
            (destructuring-bind (pattern source) binding
              `(zk:ematch ,source (,pattern ,inner))))
          bindings :from-end t :initial-value `(locally ,@body)))

(defmacro zk:let-match (bindings &body body &environment env)
  (let ((inits nil) (inputs nil) (names nil))
    (dolist (binding bindings)
      (destructuring-bind (pattern source) binding
        (let* ((mode (source-mode source))
               (row-width (case mode (:inputs (length (cdr source)))
                                (:values (if (wildcard-p pattern) 0 (length pattern)))
                                (otherwise 1)))
               (row (first (compile-rows (list (list pattern)) row-width mode env)))
               (cached (case mode
                         (:inputs (cons :inputs
                                     (loop for e in (cdr source) for v = (gensym "ARG")
                                           do (push (list v e) inits) collect v)))
                         (:values (arity source 1)
                                  (let ((v (gensym "VALUES")))
                                    (push (list v (list 'multiple-value-list (second source))) inits)
                                    (list :values (list 'values-list v))))
                         (otherwise (let ((v (gensym "ARG")))
                                      (push (list v source) inits) v)))))
          (push (match-row-variables row) names)
          (push (list row cached) inputs))))
    (merge-names names)
    (let ((inner `(locally ,@body)))
      (dolist (item inputs)
        (destructuring-bind (row source) item
          (setf (match-row-body row) (list inner))
          (let ((clause (list (match-row-source-pattern row))))
            (setq inner (compile-match 'zk:ematch source (list clause) env nil (list row))))))
      `(let ,(nreverse inits) ,inner))))
