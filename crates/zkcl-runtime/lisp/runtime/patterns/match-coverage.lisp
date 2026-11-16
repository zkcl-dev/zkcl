;;;; Coverage uses specialization of a pattern matrix. A witness is a row;
;;;; the trailing :end distinguishes an empty witness from an empty space.
(defvar *coverage-cache*)

;; The FIFO holds only bounded, compiler-owned encodings of warning-free
;; coverage proofs. A key has at most 8 KiB of payload and retains no source
;; objects. NIL disables reuse, including key construction.
(defstruct (coverage-proofs (:constructor make-coverage-proofs ()) (:copier nil) (:predicate nil))
  (keys (make-array 16 :initial-element nil) :read-only t)
  (next 0))
(defparameter *coverage-proofs* (make-coverage-proofs))

(defun remember-coverage-proof (key)
  (let ((keys (coverage-proofs-keys *coverage-proofs*))
        (slot (coverage-proofs-next *coverage-proofs*)))
    (setf (aref keys slot) key
          (coverage-proofs-next *coverage-proofs*) (mod (1+ slot) (length keys)))))

;; This subset reaches neither subtype checks nor user projections, and its
;; domains specialize through built-in heads without consulting the registries.
(defun coverage-proof-pattern-p (pattern)
  (case (car pattern)
    ((:wild :bind) t)
    (:lit (atom (second pattern)))
    (:node (and (eq (second pattern) 'cons)
                (every #'coverage-proof-pattern-p (third pattern))))
    ((:and :or) (every #'coverage-proof-pattern-p (cdr pattern)))
    (:range (coverage-proof-pattern-p (fourth pattern)))))

(defun coverage-proof-domain-p (domain)
  (cond ((symbolp domain)
         (member domain '(t nil boolean null cons list character base-char
                          integer fixnum bit signed-byte unsigned-byte)))
        ((consp domain)
         (case (car domain)
           ((eql member) (every #'atom (cdr domain)))
           (cons (every (lambda (field) (or (eq field '*) (coverage-proof-domain-p field)))
                        (cdr domain)))
           (or (every #'coverage-proof-domain-p (cdr domain)))
           ((integer signed-byte unsigned-byte mod) (integer-type-range domain))))))

(defun wilds (count) (make-list count :initial-element '(:wild)))
(defun universal-p (p) (member (car p) '(:bind :wild)))
(defun eql-literal-p (value) (or (symbolp value) (numberp value) (characterp value)))
(defun node-width (key)
  (cond ((eq key 'cons) 2) ((consp key) (second key))
        (t (length (cdr (gethash key *constructors*))))))
(defun head-width (h) (if (eq (car h) :node) (or (third h) (node-width (second h))) 0))

(defun simple-vector-size (domain)
  (when (consp domain)
    (case (car domain)
      ((simple-vector simple-string simple-base-string simple-bit-vector)
       (when (typep (second domain) '(integer 0 *)) (second domain)))
      (simple-array
       (let ((shape (third domain)))
         (when (and (consp shape) (null (cdr shape))
                    (typep (car shape) '(integer 0 *)))
           (car shape)))))))

(defun finite-heads (domain)
  (cond ((null domain) nil)
        ((eq domain t) :open)
        ((eq domain 'boolean) '((:lit nil) (:lit t)))
        ((eq domain 'null) '((:lit nil)))
        ((eq domain 'cons) '((:node cons)))
        ((eq domain 'list) '((:lit nil) (:node cons)))
        ;; Variant registry keys are user-defined symbols. These built-in
        ;; domains (including the recursive CONS fields) need no registry facts.
        ((consp domain)
         (case (car domain)
           (cons '((:node cons)))
           ((eql member) (mapcar (lambda (v) (list :lit v)) (cdr domain)))
           (otherwise :open)))
        ((gethash domain *families*)
         (mapcar (lambda (case) (list :node (car case))) (gethash domain *families*)))
        ((gethash domain *constructors*) (list (list :node domain)))
        (t :open)))

(defun active-sequence-domain (domain)
  (let ((exact (simple-vector-size domain)))
    (cond ((eq domain 'zk-treelist:treelist) (list :treelist 0 nil))
          (exact (list :vector exact exact))
          ((member domain '(vector string base-string bit-vector simple-vector
                            simple-string simple-base-string simple-bit-vector))
           (list :vector 0 nil))
          ((consp domain)
           (case (car domain)
             ((vector string base-string bit-vector)
              (let ((length (if (eq (car domain) 'vector) (third domain) (second domain))))
                (list :vector 0 (when (and (integerp length) (not (minusp length))) length))))
             ((array simple-array)
              (let ((shape (third domain)))
                (when (and (consp shape) (null (cdr shape)))
                  (let ((dim (car shape)))
                    (list :vector 0 (when (and (integerp dim) (not (minusp dim))) dim)))))))))))

(defun vector-widths (pattern)
  (case (car pattern)
    (:vector-rest (values (length (second pattern)) (length (fifth pattern))))
    (:node (values (if (consp (second pattern)) (second (second pattern)) 0) 0))
    (:type (vector-widths (third pattern)))
    (:range (vector-widths (fourth pattern)))
    ((:and :or) (vector-projections (cdr pattern)))
    (:special
     (let* ((spec (gethash (second pattern) *special-patterns*))
            (fn (and spec (special-pattern-vector-widths spec))))
       (if fn (funcall fn pattern) (values 0 0))))
    (otherwise (values 0 0))))

(defun vector-projections (patterns)
  (let ((left 0) (right 0))
    (dolist (pattern patterns (values left right))
      (multiple-value-bind (prefix suffix) (vector-widths pattern)
        (setf left (max left prefix) right (max right suffix))))))

(defun vector-heads (domain matrix query)
  (let ((spec (active-sequence-domain domain)))
    (when spec
      (destructuring-bind (tag low upper) spec
        (multiple-value-bind (left right) (vector-projections (cons query (mapcar #'car matrix)))
          (let* ((slots (+ left right))
                 (high (min (or upper (1+ slots)) (1+ slots))))
            (values (loop for length from low to (max low high) collect
                      (list :node (list tag length) (min slots length))) t)))))))

(defun pattern-heads (p)
  (case (car p)
    (:node (list (list :node (second p))))
    (:lit (list p))
    (:constant-ref (when (third p) (list (list :lit (fourth p)))))
    ((:and :or) (mapcan #'pattern-heads (cdr p)))
    (:type (pattern-heads (third p)))
    (:range (pattern-heads (fourth p)))
    (:special
     (let* ((spec (gethash (second p) *special-patterns*))
            (fn (and spec (special-pattern-heads spec))))
       (when fn (funcall fn p))))))

(defun range-contains (range number)
  (and (or (null (first range)) (<= (first range) number))
       (or (null (second range)) (<= number (second range)))))

(defun range-boundaries (range)
  (append (when (first range) (list (first range)))
          (when (second range) (list (1+ (second range))))))

(defun scalar-number (kind value)
  (if (eq kind :integer)
      (when (integerp value) value)
      (when (characterp value) (char-code value))))

(defun type-scalar-boundaries (specifier kind)
  (let ((interval (and (eq kind :integer) (integer-type-range specifier))))
    (cond (interval (range-boundaries interval))
          ((not (consp specifier)) nil)
          ((eq (car specifier) 'eql)
           (let ((number (scalar-number kind (second specifier))))
             (when number (list number (1+ number)))))
          ((eq (car specifier) 'member)
           (loop for value in (cdr specifier)
                 for number = (scalar-number kind value)
                 when number append (list number (1+ number))))
          ((member (car specifier) '(or and not))
           (mapcan (lambda (child) (type-scalar-boundaries child kind)) (cdr specifier))))))

(defun scalar-boundaries (pattern kind)
  (case (car pattern)
    ((:and :or) (mapcan (lambda (child) (scalar-boundaries child kind)) (cdr pattern)))
    (:range (append (when (eq (second pattern) kind) (range-boundaries (third pattern)))
                    (scalar-boundaries (fourth pattern) kind)))
    (:type
     (append (type-scalar-boundaries (second pattern) kind)
             (scalar-boundaries (third pattern) kind)))
    ((:lit :constant-ref)
     (let ((number (scalar-number kind (if (eq (car pattern) :lit) (second pattern) (fourth pattern)))))
       (when number (list number (1+ number)))))))

(defun scalar-domain (domain)
  (let ((interval (integer-type-range domain)))
    (cond (interval (values :integer (list interval)))
          ((member domain '(character base-char))
           (values :character '((0 #xd7ff) (#xe000 #x10ffff)))))))

(defun scalar-heads (domain matrix query)
  (multiple-value-bind (kind ranges) (scalar-domain domain)
    (when kind
      (let ((boundaries
              (sort (remove-duplicates
                     (append (mapcan #'range-boundaries ranges) (scalar-boundaries query kind)
                             (mapcan (lambda (row) (scalar-boundaries (car row) kind)) matrix))) #'<)))
        (values
         (loop for (low next) on (cons nil boundaries)
               for high = (and next (1- next))
               for representative = (or low high 0)
               when (some (lambda (range) (range-contains range representative)) ranges)
                 collect (list :scalar kind low high
                               (if (eq kind :integer) representative (code-char representative))))
         t)))))

(defun head-type (h domain)
  (case (car h)
    (:lit (list 'eql (second h)))
    (:scalar (cond ((and (third h) (eql (third h) (fourth h))) (list 'eql (fifth h)))
                   ((eq (second h) :integer) (list 'integer (or (third h) '*) (or (fourth h) '*)))
                   (t 'character)))
    (:node (if (consp (second h))
               (if (eq (car (second h)) :treelist) 'zk-treelist:treelist 'vector)
               (second h)))
    (otherwise domain)))

(defun side-effect-free-type-p (type)
  ;; Only accept specifiers that can be tested without user code.
  (cond ((symbolp type) (or (member type '(boolean nil)) (find-class type nil)))
        ((or (member (car type) '(member eql)) (integer-type-range type)) t)
        ((member (car type) '(or and not cons)) (every #'side-effect-free-type-p (cdr type)))
        (t nil)))

(defun provable-subtype-p (from to)
  (or (eq to t)
      (and (side-effect-free-type-p from) (side-effect-free-type-p to)
           (or (equal from to) (subtypep from to)))))

(defun field-domains (head domain)
  (cond ((and (equal head '(:node cons)) (consp domain) (eq (car domain) 'cons))
         (loop for i from 1 to 2 collect
           (if (and (nthcdr i domain) (not (eq (nth i domain) '*))) (nth i domain) t)))
        ((and (eq (car head) :node) (symbolp (second head))
              (gethash (second head) *constructor-field-types*))
         (gethash (second head) *constructor-field-types*))
        (t (make-list (head-width head) :initial-element t))))

(defun intersect-pattern (a b)
  (cond ((universal-p a) b) ((or (universal-p b) (equal a b)) a)
        (t (list :and a b))))

(defun specialize-pattern (p h domain include-uncertain-matches-p)
  (let ((all (list (wilds (head-width h)))))
    (case (car p)
      ((:bind :wild) all)
      (:constant-ref (if (third p)
                         (specialize-pattern (list :lit (fourth p)) h domain include-uncertain-matches-p)
                         (when include-uncertain-matches-p all)))
      (:lit (cond ((equal p h) all)
                  ((and (eq (car h) :scalar) (third h) (eql (third h) (fourth h))
                        (eql (second p) (fifth h))) all)
                  ;; A quoted compound datum can also inhabit a structural head.
                  ((and include-uncertain-matches-p (eq (car h) :node)
                        (or (consp (second p)) (vectorp (second p)) (zk-treelist:treelist-p (second p)))) all)))
      (:node (cond ((and (eq (car h) :node) (equal (second h) (second p))) (list (third p)))
                   ((and include-uncertain-matches-p (eq (car h) :lit)
                         (or (consp (second h)) (vectorp (second h)) (zk-treelist:treelist-p (second h)))) all)))
      (:or (remove-duplicates
            (mapcan (lambda (arm) (specialize-pattern arm h domain include-uncertain-matches-p))
                    (cdr p)) :test #'equal))
      (:and (dolist (arm (cdr p) all)
              (let ((next nil))
                (dolist (x all)
                  (dolist (y (specialize-pattern arm h domain include-uncertain-matches-p))
                    (push (mapcar #'intersect-pattern x y) next)))
                (setq all next))))
      (:type (when (or include-uncertain-matches-p (provable-subtype-p domain (second p))
                       (provable-subtype-p (head-type h domain) (second p)))
               (specialize-pattern (third p) h domain include-uncertain-matches-p)))
      (:range
       (case (car h)
         ((:lit :scalar) (let ((number (scalar-number (second p) (if (eq (car h) :lit) (second h) (fifth h)))))
                 (when (and number (range-contains (third p) number))
                   (specialize-pattern (fourth p) h domain include-uncertain-matches-p))))
         (:other (when include-uncertain-matches-p
                   (specialize-pattern (fourth p) h domain t)))))
      (:vector-rest
       (let* ((prefix (second p)) (rest (third p)) (suffix (fifth p))
              (minimum (+ (length prefix) (length suffix)))
              (key (second h)))
         (cond ((and (eq (car h) :node) (consp key) (eq (car key) :vector))
                (when (and (>= (second key) minimum)
                           (or include-uncertain-matches-p (universal-p rest)))
                  (list (append prefix (wilds (- (head-width h) minimum)) suffix))))
               ((and (eq (car h) :other) (zerop minimum) (universal-p rest)
                     (or (member domain '(vector string base-string bit-vector simple-vector
                                          simple-string simple-base-string simple-bit-vector))
                         (and (consp domain) (member (car domain) '(vector string base-string bit-vector))))) all)
               (include-uncertain-matches-p all))))
      (:special
       (let* ((spec (gethash (second p) *special-patterns*))
              (fn (and spec (special-pattern-specializer spec))))
         (if fn
             (funcall fn p h domain include-uncertain-matches-p)
             (when include-uncertain-matches-p all))))
      ((:view-values :compare) (when include-uncertain-matches-p all))
      (:view (when (or include-uncertain-matches-p (universal-p (third p))) all)))))

(defun disjoint-head (p)
  ;; Compound EQUAL literals, views, type filters and alternatives can
  ;; intersect several heads; keep them in the shared part of the matrix.
  (case (car p)
    (:node (list :node (second p)))
    (:lit (when (eql-literal-p (second p)) p))))

(defun index-row (index row)
  (push row (gethash (or (disjoint-head (car row)) :shared) index)))

(defun indexed-rows (index head)
  (let ((exact (gethash head index)) (shared (gethash :shared index)))
    (cond ((null shared) exact) ((null exact) shared) (t (append exact shared)))))

(defun specialize-matrix (matrix h domain)
  (let ((out nil))
    (dolist (row matrix (nreverse out))
      (dolist (fields (specialize-pattern (car row) h domain nil))
        (push (append fields (cdr row)) out)))))

(defun witness-head (h children)
  (case (car h)
    (:node (let ((tag (second h)))
             (cond ((not (consp tag)) (cons tag children))
                   ((= (second tag) (length children))
                    (cons (if (eq (car tag) :treelist) 'zk-treelist:treelist 'vector) children))
                   (t (list (if (eq (car tag) :treelist) :treelist-length :vector-length)
                            (second tag) :tested-elements children)))))
    (:lit (second h))
    (:scalar (fifth h))
    (otherwise '_)))

(defun usefulness-witness (matrix query domains)
  (let ((key (list matrix query domains)))
    (multiple-value-bind (answer cached) (gethash key *coverage-cache*)
      (if cached answer
          (setf (gethash key *coverage-cache*) (find-witness matrix query domains))))))

(defun find-witness (matrix query domains)
  (cond ((some (lambda (row) (every #'universal-p row)) matrix) nil)
        ((null query) '(:end))
        ((and (consp (car domains)) (eq (caar domains) 'or))
         (some (lambda (d) (usefulness-witness matrix query (cons d (cdr domains)))) (cdar domains)))
        (t
         (let* ((domain (car domains))
                (q (car query))
                (partition (or (let ((scalar (multiple-value-list (scalar-heads domain matrix q))))
                                 (when (second scalar) scalar))
                               (multiple-value-list (vector-heads domain matrix q))))
                (heads (if (second partition) (first partition) (finite-heads domain))))
           (cond ((and (not (second partition)) (eq (car q) :lit) (eql-literal-p (second q)))
                  ;; A scalar literal has exactly one possible head.
                  (setq heads (and (or (eq heads :open) (member q heads :test #'equal))
                                   (list q))))
                 ((eq heads :open)
                  (let ((used (append (pattern-heads q)
                                      (mapcan (lambda (row) (pattern-heads (car row))) matrix))))
                    (setq heads (append (remove-duplicates used :test #'equal) '((:other)))))))
           (let ((index (when (and matrix (nthcdr 8 heads)
                                   (every (lambda (h) (or (eq (car h) :other) (disjoint-head h))) heads))
                          (make-hash-table :test 'equal))))
             (when index (dolist (row matrix) (index-row index row)))
             (dolist (h heads)
               (let ((width (head-width h)))
                 (dolist (qs (specialize-pattern q h domain t))
                   (let ((rest (usefulness-witness
                                 (specialize-matrix (if index (indexed-rows index (disjoint-head h)) matrix) h domain)
                                 (append qs (cdr query))
                                 (append (field-domains h domain) (cdr domains)))))
                     (when rest
                       (return-from find-witness
                         (cons (witness-head h (subseq rest 0 width)) (nthcdr width rest)))))))))))))
