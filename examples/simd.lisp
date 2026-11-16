;;;; Brighten interleaved RGBA pixels with unsigned saturating addition.
;;;; A pack holds four pixels. A mask bounds the accesses at the tail.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

(defpackage #:simd-example
  (:use #:cl)
  (:local-nicknames (#:simd #:zk-simd)))
(in-package #:simd-example)

(defun brighten (pixels amount)
  (check-type pixels (simple-array (unsigned-byte 8) (*)))
  (check-type amount (unsigned-byte 8))
  (let ((count (length pixels)))
    (unless (zerop (mod count 4)) (error "RGBA needs four bytes per pixel."))
    (let* ((output (make-array count :element-type '(unsigned-byte 8)))
           (shape 'simd:u8x16)
           ;; Add zero to every alpha lane so that opacity is preserved.
           (delta (simd:make-pack shape amount amount amount 0 amount amount amount 0
                                       amount amount amount 0 amount amount amount 0))
           (tail (mod count 16))
           (tail-start (- count tail)))
      (loop for start from 0 below tail-start by 16
            for pack = (simd:load-pack shape pixels start)
            do (simd:store-pack output start (simd:add-saturating pack delta)))
      (when (plusp tail)
        (let* ((mask (simd:mask-first 16 tail))
               (pack (simd:load-masked shape pixels tail-start mask)))
          (simd:store-masked output tail-start mask (simd:add-saturating pack delta))))
      output)))

(defparameter *pixels*
  (zk-bits:octets 10 20 30 0   250 240 230 255   3 254 100 128
                  0 0 255 50   255 13 238 200))

(zk-test:deftest full-blocks-and-tail ()
  (loop for count from 0 to (length *pixels*) by 4
        for pixels = (subseq *pixels* 0 count)
        for expected = (loop for byte across pixels
                             for lane from 0
                             collect (if (= (mod lane 4) 3) byte (min 255 (+ byte 24))))
        do (zk-test:is-equal expected (coerce (brighten pixels 24) 'list))))

(zk-test:deftest overflow-policies ()
  (let ((max (simd:splat 'simd:u8x16 255))
        (one (simd:splat 'simd:u8x16 1)))
    (zk-test:is= 0 (simd:lane (simd:add max one) 0))
    (zk-test:is= 255 (simd:lane (simd:add-saturating max one) 0))
    (let ((condition (zk-test:signals simd:integer-overflow (simd:add-checked max one))))
      (zk-test:is= 0 (simd:condition-lane condition)))))

(zk-test:defsuite simd-example full-blocks-and-tail overflow-policies)

(let ((report (zk-test:run 'simd-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(format t "~%Brighten five pixels with one sixteen-lane block and a four-lane tail~%")
(let ((brighter (brighten *pixels* 24)))
  (loop for start from 0 below (length *pixels*) by 4
        for end = (+ start 4)
        do (format t "  ~S -> ~S~%" (subseq *pixels* start end) (subseq brighter start end))))
