(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-gray))

(in-package #:zkcl-stdlib-tests)

(zk:defvariant pbt-tree
  (pbt-leaf value)
  (pbt-node left right))

(defun pbt-tree-gen ()
  (zk-pbt:recursive
   (zk-pbt:map-gen #'pbt-leaf (zk-pbt:integers :min -10 :max 10))
   (lambda (sub)
     (zk-pbt:map-gen (lambda (pair) (pbt-node (first pair) (second pair)))
                     (zk-pbt:tuple sub sub)))
   :max-depth 3))

(defun naive-tree-sum (tree)
  (etypecase tree
    (pbt-leaf (pbt-leaf-value tree))
    (pbt-node (+ (naive-tree-sum (pbt-node-left tree))
                 (naive-tree-sum (pbt-node-right tree))))))

(defun match-tree-sum (tree)
  (zk:ematch tree
    ((pbt-leaf v) v)
    ((pbt-node l r) (+ (match-tree-sum l) (match-tree-sum r)))))

(zk-pbt:defproperty variant-match-matches-etypecase ((tree (pbt-tree-gen)))
  (zk-test:is= (naive-tree-sum tree) (match-tree-sum tree))
  t)

(zk-pbt:defproperty ematch-signals-match-error-on-miss ((n (zk-pbt:integers :min 1 :max 20)))
  (let ((c (zk-test:signals zk:match-error
             (zk:ematch n
               (0 :zero)
               (-1 :neg-one)))))
    (zk-test:is-equal (list n) (zk:match-error-values c)))
  t)

(zk-pbt:defproperty async-task-group-and-cancellation-invariants
    ((xs (zk-pbt:list-of (zk-pbt:integers :min -10 :max 10) :min 1 :max 8))
     (fail-index (zk-pbt:integers :min 0 :max 7)))
  (let ((results (make-array (length xs) :initial-element nil))
        (cleaned 0))
    (zk-async:with-task-group (g)
      (loop for x in xs for i from 0 do
        (let ((idx i) (val x))
          (zk-async:spawn-task (g)
            (zk-async:yield)
            (setf (aref results idx) (* val 2))))))
    (zk-test:is-equal (mapcar (lambda (x) (* x 2)) xs) (coerce results 'list))
    ;; Sibling cancellation + protect-from-cancel cleanup on child error.
    (let ((target-idx (mod fail-index (length xs)))
          (entered 0)
          (protected-ran 0)
          (children (length xs)))
      (zk-test:signals error
        (zk-async:with-task-group (g)
          (dotimes (i children)
            (let ((idx i))
              (zk-async:spawn-task (g)
                (unwind-protect
                     (progn
                       (incf entered)
                       (when (= idx target-idx)
                         (loop while (< entered children) do (zk-async:yield))
                         (error "boom-~D" idx))
                       (loop (zk-async:yield)))
                  (zk-async:protect-from-cancel
                    (incf cleaned)
                    (incf protected-ran))))))))
      (zk-test:is= children entered)
      (zk-test:is= children cleaned)
      (zk-test:is= children protected-ran)))
  t)

(zk-pbt:defproperty async-timeout-opt-contract ((fast (zk-pbt:booleans)))
  (if fast
      (zk-test:is-values (zk-async:with-timeout-opt (0.5) :ok) :ok t)
      (let ((cleaned nil))
        (zk-test:is-values
          (zk-async:with-timeout-opt (0.005)
            (unwind-protect
                 (loop (zk-async:yield))
              (zk-async:protect-from-cancel (setf cleaned t))))
          nil nil)
        (zk-test:is cleaned)))
  t)

(defclass model-char-stream (zk-gray:fundamental-character-input-stream
                             zk-gray:fundamental-character-output-stream)
  ((buf :initform (make-array 16 :element-type 'character :fill-pointer 0 :adjustable t)
        :reader model-stream-buf)
   (pos :initform 0 :accessor model-stream-pos)))

(defmethod zk-gray:stream-write-char ((s model-char-stream) ch)
  (vector-push-extend ch (model-stream-buf s))
  ch)

(defmethod zk-gray:stream-read-char ((s model-char-stream))
  (let ((buf (model-stream-buf s))
        (pos (model-stream-pos s)))
    (if (< pos (length buf))
        (prog1 (char buf pos) (setf (model-stream-pos s) (1+ pos)))
        :eof)))

(defmethod zk-gray:stream-unread-char ((s model-char-stream) ch)
  (declare (ignore ch))
  (when (plusp (model-stream-pos s))
    (decf (model-stream-pos s)))
  nil)

(zk-pbt:defmachine gray-stream-machine
  (:model () (list "" 0))
  (:open () (make-instance 'model-char-stream))
  (:close (s) (close s))
  (:invariant (model s)
    (and (string= (first model) (coerce (model-stream-buf s) 'string))
         (= (second model) (model-stream-pos s))))
  (:command :write-str (text)
    (:args (m) (zk-pbt:tuple (zk-pbt:ascii-strings :min 0 :max 10)))
    (:step (m) (values (list (concatenate 'string (first m) text) (second m))
                       (list text)))
    (:act (s) (zk-gray:stream-write-string s text)))
  (:command :read-ch ()
    (:args (m) (zk-pbt:tuple))
    (:step (m)
      (let ((buf (first m)) (pos (second m)))
        (if (< pos (length buf))
            (values (list buf (1+ pos)) (list (char buf pos)))
            (values m (list :eof)))))
    (:act (s) (zk-gray:stream-read-char s)))
  (:command :peek-ch ()
    (:args (m) (zk-pbt:tuple))
    (:step (m)
      (let ((buf (first m)) (pos (second m)))
        (values m (list (if (< pos (length buf)) (char buf pos) :eof)))))
    (:act (s) (zk-gray:stream-peek-char s))))

(zk-pbt:defproperty gray-stream-state-machine
    ((ops (zk-pbt:commands 'gray-stream-machine :max 20)))
  (zk-pbt:run-commands 'gray-stream-machine ops))
