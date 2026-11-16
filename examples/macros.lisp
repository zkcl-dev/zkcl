;;;; Reorder work items with a macro that swaps two single-valued SETF places.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

(defpackage #:macros-example
  (:use #:cl))
(in-package #:macros-example)

(defmacro swapf (left right &environment env)
  "Read both places, then write the two old values in the opposite order."
  ;; Each expansion gives temporary bindings, store variables, a writer,
  ;; and a reader. ENV carries lexical definitions such as SYMBOL-MACROLET.
  (multiple-value-bind (left-temps left-forms left-stores left-write left-read)
      (get-setf-expansion left env)
    (multiple-value-bind (right-temps right-forms right-stores right-write right-read)
        (get-setf-expansion right env)
      (unless (and (= (length left-stores) 1) (= (length right-stores) 1))
        (error "SWAPF takes two single-valued places."))
      `(let* (,@(mapcar #'list left-temps left-forms)
              ,@(mapcar #'list right-temps right-forms)
              (,(first right-stores) ,left-read)
              (,(first left-stores) ,right-read))
         ,left-write
         ,right-write
         nil))))

(defun swap-next-two (jobs)
  (let ((index 0))
    (flet ((next-index () (prog1 index (incf index))))
      (swapf (aref jobs (next-index)) (aref jobs (next-index))))
    (values jobs index)))

(defun swap-first-and-last (jobs)
  (symbol-macrolet ((first-job (aref jobs 0))
                    (last-job (aref jobs (1- (length jobs)))))
    (swapf first-job last-job))
  jobs)

(zk-test:deftest place-subforms-run-once ()
  (multiple-value-bind (jobs index-calls) (swap-next-two (vector :index :publish :archive))
    (zk-test:is-equalp #(:publish :index :archive) jobs)
    (zk-test:is= 2 index-calls)))

(zk-test:deftest lexical-places ()
  (zk-test:is-equalp #(:archive :publish :index)
                     (swap-first-and-last (vector :index :publish :archive))))

(zk-test:defsuite macros-example place-subforms-run-once lexical-places)

(let ((report (zk-test:run 'macros-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(let ((jobs (vector :index :publish :archive)))
  (format t "~%Original queue ~S~%" jobs)
  (multiple-value-bind (reordered reads) (swap-next-two jobs)
    (format t "  Swap the first two places -> ~S with ~D index calls~%" reordered reads))
  (format t "  Swap through symbol macros -> ~S~%" (swap-first-and-last jobs)))
