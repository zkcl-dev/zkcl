;;;; Count the set bits in a bitmap. LLVM can inline a VOP defined in Lisp.

(defpackage #:bitmap-vop-example
  (:use #:common-lisp))
(in-package #:bitmap-vop-example)

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-pbt))

(defun popcount-low-u64 (word)
  "Count the ones in the low 64 bits of an arbitrary Lisp integer."
  (logcount (logand word #xffffffffffffffff)))

(zk-sys:defknown popcount-low-u64 (integer) (integer 0 64))

(zk-sys:define-vop popcount-u64
  (:translate popcount-low-u64)
  (:args ((word :type (unsigned-byte 64) :representation :u64)))
  (:results ((count :type (integer 0 64) :representation :u64)))
  (:effects :pure :no-gc :no-unwind)
  (:lower :portable (:popcnt word))
  ;; Count the bits in each byte, then add the eight lane counts.
  (:asm :aarch64
    (:requires :asimd)
    (:in (word :gpr64))
    (:out (count :gpr64))
    (:temp (lanes :simd128))
    "fmov {lanes:d}, {word}"
    "cnt {lanes:v}.8b, {lanes:v}.8b"
    "uaddlv {lanes:h}, {lanes:v}.8b"
    "fmov {count}, {lanes:d}")
  (:fallback popcount-low-u64))

(defun count-bitmap-bits (words)
  (declare (type (simple-array (unsigned-byte 64) (*)) words))
  (let ((total 0))
    (dotimes (index (length words) total)
      (incf total (popcount-low-u64 (aref words index))))))

(defun count-after-evaluation (next-word)
  (popcount-low-u64 (funcall next-word)))

(defun count-via-lisp (word)
  (declare (notinline popcount-low-u64))
  (popcount-low-u64 word))

(defun example-bitmap ()
  (make-array 4 :element-type '(unsigned-byte 64)
                :initial-contents '(0 1 #x8000000000000000 #xffffffffffffffff)))

;;; The full-width cases exceed the fixnum range. The fallback accepts any
;;; integer, including negative integers and integers wider than a word.
(zk-test:deftest bitmap-count-boundaries ()
  (zk-test:is= 66 (count-bitmap-bits (example-bitmap)))
  (zk-test:is= 0 (count-after-evaluation (lambda () #x10000000000000000)))
  (zk-test:signals type-error (count-after-evaluation (lambda () :not-an-integer)))
  (let ((calls 0))
    (zk-test:is= 64 (count-after-evaluation (lambda () (incf calls) -1)))
    (zk-test:is= 1 calls)))

(zk-test:deftest bitmap-count-observes-redefinition ()
  (let ((original (symbol-function 'popcount-low-u64)))
    (unwind-protect
        (progn
          (setf (symbol-function 'popcount-low-u64)
                (lambda (word) (declare (ignore word)) 7))
          (zk-test:is= 28 (count-bitmap-bits (example-bitmap))))
      (setf (symbol-function 'popcount-low-u64) original)))
  (zk-test:is= 66 (count-bitmap-bits (example-bitmap))))

(zk-pbt:defproperty bitmap-popcount-matches-reference
    ((word (zk-pbt:integers :min 0 :max #xffffffffffffffff)))
  (:trials 300)
  (:examples (0) (1) (#x8000000000000000) (#xffffffffffffffff))
  (let ((words (make-array 1 :element-type '(unsigned-byte 64)
                             :initial-element word)))
    (= (count-bitmap-bits words) (count-via-lisp word))))

(zk-test:defsuite bitmap-vop-showcase
  bitmap-count-boundaries
  bitmap-count-observes-redefinition
  bitmap-popcount-matches-reference)

(let ((report (zk-test:run 'bitmap-vop-showcase :seed 42)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(format t "~&Set bits across four bitmap words: ~D~%" (count-bitmap-bits (example-bitmap)))
