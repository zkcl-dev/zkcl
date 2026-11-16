(defun test-weakness (table)
  #+sbcl (sb-ext:hash-table-weakness table)
  #-sbcl (zk:hash-table-weakness table))

(defun collect-weak-entries ()
  #+sbcl (sb-ext:gc :full t)
  #-sbcl (zk:gc))

(defun populate-weak-table (weakness keep)
  (let ((table (make-hash-table :weakness weakness :test 'eq :size 1))
        (key (list :key))
        (value (list :value)))
    (setf (gethash key table) value)
    (list table
          (when (member keep '(:key :both)) key)
          (when (member keep '(:value :both)) value))))

(defvar *weak-cases*
  (loop for mode in '(nil :key :value :key-or-value :key-and-value)
        append (loop for keep in '(:none :key :value :both)
                     collect (cons mode (populate-weak-table mode keep)))))

;; The value holds its own weak key, and a pair of entries depends on each
;; other. Neither conditional edge may start tracing these objects.
(defun circular-entries ()
  (let ((table (make-hash-table :weakness :key :test 'eq))
        (a (list :a)) (b (list :b)) (c (list :c)))
    (setf (gethash a table) (list a)
          (gethash b table) c
          (gethash c table) b)
    table))

(defvar *weak-cycles* (circular-entries))

;; Each value leads to the next key through an ordinary cons. The second
;; table is itself reachable only after tracing the first table's entry.
(defun make-weak-chain ()
  (let ((outer (make-hash-table :weakness :key))
        (inner (make-hash-table :weakness :key :test 'eq))
        (head (list 0)))
    (loop for key = head then next
          for i from 1 to 25
          for next = (list i)
          do (setf (gethash key inner) (list next)))
    (setf (gethash head outer) inner)
    (list outer head)))

(defvar *weak-chain* (make-weak-chain))

;; Reaching A keeps both keys mapped to it in the :VALUE table. One of
;; those keys, B, keeps C through the :KEY-OR-VALUE table. Only then are
;; both endpoints of the last entry reachable.
(defun make-mixed-chain (keep-head)
  (let ((forward (make-hash-table :weakness :key))
        (reverse (make-hash-table :weakness :value))
        (either (make-hash-table :weakness :key-or-value))
        (both (make-hash-table :weakness :key-and-value))
        (head (list :head)) (a (list :a)) (b (list :b)) (c (list :c)))
    (setf (gethash head forward) a
          (gethash b reverse) a
          (gethash (list :b2) reverse) a
          (gethash c either) b
          (gethash head both) c)
    (list (when keep-head head) forward reverse either both)))

(defvar *mixed-chain* (make-mixed-chain t))
(defvar *dead-mixed-chain* (make-mixed-chain nil))

(collect-weak-entries)
(collect-weak-entries)

(dolist (entry *weak-cases*)
  (destructuring-bind (mode table key value) entry
    (let ((keep (case mode
                  ((nil) t)
                  (:key key)
                  (:value value)
                  (:key-or-value (or key value))
                  (:key-and-value (and key value)))))
      (assert (eq (test-weakness table) mode))
      (assert (= (hash-table-count table) (if keep 1 0)))
      (let ((count 0))
        (maphash (lambda (k v)
                   (incf count)
                   (assert (equal k '(:key)))
                   (assert (equal v '(:value)))
                   (when key (assert (eq k key)))
                   (when value (assert (eq v value)))
                   (assert (eq v (gethash k table))))
                 table)
        (assert (= count (hash-table-count table))))
      (when (and key (null keep))
        (assert (eq (gethash key table :missing) :missing))))))

(assert (zerop (hash-table-count *weak-cycles*)))
(destructuring-bind (outer head) *weak-chain*
  (let ((inner (gethash head outer)))
    (assert (= (hash-table-count inner) 25))
    (loop for key = head then (car (gethash key inner))
          for i from 0 to 25 do (assert (= (car key) i)))))
(setf (second *weak-chain*) nil)
(collect-weak-entries)
(assert (zerop (hash-table-count (first *weak-chain*))))

(destructuring-bind (head forward reverse either both) *mixed-chain*
  (let* ((c (gethash head both)) (b (gethash c either)) (a (gethash b reverse)))
    (assert (equal c '(:c)))
    (assert (equal b '(:b)))
    (assert (equal a '(:a)))
    (assert (eq a (gethash head forward)))
    (assert (= (hash-table-count reverse) 2))
    (maphash (lambda (key value)
               (assert (member (car key) '(:b :b2)))
               (assert (eq value a))
               (assert (eq (gethash key reverse) a)))
             reverse)))
(assert (every #'zerop (mapcar #'hash-table-count (rest *dead-mixed-chain*))))
