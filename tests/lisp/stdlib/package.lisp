(defpackage #:zkcl-stdlib-tests
  (:use #:cl)
  (:export #:stdlib-all #:portable-all))

(in-package #:zkcl-stdlib-tests)

(defun naive-logbitp-model (index n)
  (oddp (floor n (ash 1 index))))

(defun insertion-sort-oracle (items pred)
  (let ((vec (coerce (copy-seq items) 'vector)))
    (loop for i from 1 below (length vec) do
      (let ((x (aref vec i))
            (j (1- i)))
        (loop while (and (>= j 0) (funcall pred x (aref vec j))) do
          (setf (aref vec (1+ j)) (aref vec j))
          (decf j))
        (setf (aref vec (1+ j)) x)))
    (coerce vec 'list)))
