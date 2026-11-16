;;;; Keep a branching release-note history with zkcl's persistent sequence.
;;;; The underlying tree is based on Sek by Arthur Charguéraud and François Pottier, "A Catenable,
;;;; Splittable, Transient Sequence Data Structure" (ICFP 2026).
;;;; Splits and joins share untouched subtrees, and snapshots take constant time.
;;;; Mutable handles update unshared nodes in place and copy shared nodes on write.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

(defpackage #:persistent-tree-example
  (:use #:cl)
  (:local-nicknames (#:tl #:zk-treelist)))
(in-package #:persistent-tree-example)

(defstruct (history (:constructor make-history (lines &optional (past (tl:empty))
                                                              (future (tl:empty)))))
  (lines (tl:empty) :read-only t)
  (past (tl:empty) :read-only t)
  (future (tl:empty) :read-only t))

(defun commit (history lines)
  ;; A commit starts a branch. The caller can keep the old history as well.
  (make-history lines (tl:push-back (history-past history) (history-lines history))))

(defun undo (history)
  (if (tl:empty-p (history-past history))
      history
      (multiple-value-bind (lines past) (tl:pop-back (history-past history))
        (make-history lines past (tl:push-back (history-future history) (history-lines history))))))

(defun redo (history)
  (if (tl:empty-p (history-future history))
      history
      (multiple-value-bind (lines future) (tl:pop-back (history-future history))
        (make-history lines (tl:push-back (history-past history) (history-lines history))
                      future))))

(defun splice-lines (lines start end replacement)
  ;; Replace [START, END) by joining persistent slices with the new lines.
  (multiple-value-bind (before tail) (tl:split lines start)
    (tl:append before replacement (tl:subseq tail (- end start)))))

(defun release-history ()
  (let* ((base (make-history (tl:treelist "Draft" "Compiler" "Runtime")))
         (compiler (commit base
                           (splice-lines (history-lines base) 1 2
                                         (tl:treelist "Compiler" "Faster builds"))))
         (restored (undo compiler))
         (runtime (commit restored
                          (splice-lines (history-lines restored) 2 3
                                        (tl:treelist "Runtime" "Async I/O")))))
    (values base restored compiler runtime)))

(defun prepare-editions (lines)
  (let ((draft (tl:thaw lines)))
    (tl:push-back! draft "Thanks to the contributors.")
    (let ((checkpoint (tl:snapshot draft))
          (preview (tl:copy draft)))
      ;; Both handles initially share contents. Writes affect their own edition.
      (setf (tl:ref draft 0) "Release 0.2"
            (tl:ref preview 0) "Release 0.2 (preview)")
      (values checkpoint (tl:freeze draft) (tl:freeze preview)))))

(zk-test:deftest branch-after-undo ()
  (multiple-value-bind (base restored compiler runtime) (release-history)
    (zk-test:is-eq base (undo base))
    (zk-test:is-eq (history-lines base) (history-lines restored))
    (zk-test:is-eq (history-lines compiler) (history-lines (redo restored)))
    (zk-test:is-eq runtime (redo runtime))
    (zk-test:is-equal '("Draft" "Compiler" "Faster builds" "Runtime")
                      (tl:to-list (history-lines compiler)))
    (zk-test:is-equal '("Draft" "Compiler" "Runtime" "Async I/O")
                      (tl:to-list (history-lines runtime)))))

(zk-test:deftest transient-editions ()
  (let ((base (tl:treelist "Draft" "Changes")))
    (multiple-value-bind (checkpoint release preview) (prepare-editions base)
      (zk-test:is-equal '("Draft" "Changes") (tl:to-list base))
      (zk-test:is-equal '("Draft" "Changes" "Thanks to the contributors.")
                        (tl:to-list checkpoint))
      (zk-test:is-equal "Release 0.2" (tl:first release))
      (zk-test:is-equal "Release 0.2 (preview)" (tl:first preview))))
  (let* ((scratch (tl:mutable-treelist :draft))
         (published (tl:freeze scratch)))
    (zk-test:is (tl:empty-p scratch))
    (tl:push-back! scratch :next)
    (zk-test:is-equal '(:draft) (tl:to-list published))))

(zk-test:deftest sequence-operations ()
  (let* ((original (tl:from-sequence (loop for i below 100 collect i)))
         (edited (tl:with-transient (draft original)
                   (setf (elt draft 50) 500)
                   (tl:push-front! draft -1)
                   (tl:push-back! draft 100))))
    (zk-test:is (typep edited 'sequence))
    (zk-test:is= 102 (length edited))
    (zk-test:is= 50 (elt original 50))
    (zk-test:is-equal '(49 500 51) (coerce (tl:subseq edited 50 53) 'list))
    (zk-test:is-equal '(0 1 2) (map 'list #'1+ (tl:subseq edited 0 3)))
    (zk-test:is= 100 (tl:last edited))))

(zk-test:defsuite persistent-tree-example
  branch-after-undo transient-editions sequence-operations)

(defun show-edition (label lines)
  (format t "~A~%" label)
  (tl:do-elements (line lines)
    (format t "  ~A~%" line)))

(let ((report (zk-test:run 'persistent-tree-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(multiple-value-bind (base restored compiler runtime) (release-history)
  (declare (ignore base))
  (show-edition "Undo restored" (history-lines restored))
  (show-edition "Compiler branch is still available" (history-lines compiler))
  (multiple-value-bind (checkpoint release preview) (prepare-editions (history-lines runtime))
    (show-edition "Saved checkpoint of the runtime branch" checkpoint)
    (show-edition "Published edition" release)
    (show-edition "Preview edition" preview)))
