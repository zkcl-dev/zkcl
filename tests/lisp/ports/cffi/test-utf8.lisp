;;;; Run after zkcl-cffi-port:register. Starting with Babel alone also checks
;;;; that loading the port leaves Babel's existing vector mapping intact.

(asdf:load-system "babel")

(let* ((mapping (babel-encodings:lookup-mapping babel::*string-vector-mappings* :utf-8))
       (counter (babel-encodings:octet-counter mapping))
       (encoder (babel-encodings:encoder mapping))
       (factory (babel-encodings::get-abstract-mapping :utf-8))
       (make-counter (babel-encodings::octet-counter-factory factory))
       (make-encoder (babel-encodings::encoder-factory factory)))
  (asdf:load-system "cffi")
  (assert (eq factory (babel-encodings::get-abstract-mapping :utf-8)))
  (assert (eq make-counter (babel-encodings::octet-counter-factory factory)))
  (assert (eq make-encoder (babel-encodings::encoder-factory factory)))
  (assert (eq mapping (babel-encodings:lookup-mapping babel::*string-vector-mappings* :utf-8)))
  (assert (eq counter (babel-encodings:octet-counter mapping)))
  (assert (eq encoder (babel-encodings:encoder mapping)))
  (assert (equal '(65 233 155 170 240 159 154 128)
                 (coerce (babel:string-to-octets "A雪🚀" :encoding :utf-8) 'list))))

(in-package #:zkcl-cffi-port)

(defun check-utf8-buffer (string size encoded &key (start 0) end (offset 0))
  (let ((expected (make-list 16 :initial-element #x5a)))
    (replace expected encoded :start1 offset)
    (cffi:with-foreign-pointer (p 16)
      (dotimes (i 16) (setf (cffi:mem-ref p :uint8 i) #x5a))
      (cffi:lisp-string-to-foreign string p size :encoding :utf-8
                                   :start start :end end :offset offset)
      (assert (equal expected (loop for i below 16 collect (cffi:mem-ref p :uint8 i)))))))

(check-utf8-buffer "abc" 4 '(97 98 99 0))
(check-utf8-buffer "abc" 6 '(97 98 99 0))
(check-utf8-buffer "xA雪🚀By" 9 '(65 233 155 170 240 159 154 128 0)
                   :start 1 :end 5 :offset 2)
(check-utf8-buffer "xA雪🚀By" 8 '(65 233 155 170 0) :start 1 :end 5 :offset 2)
(check-utf8-buffer "雪" 3 '(0))
(check-utf8-buffer "abc" 1 '(0) :start 2 :end 2)

(let* ((foreign (babel-encodings:lookup-mapping cffi::*foreign-string-mappings* :utf-8))
       (vector (babel-encodings:lookup-mapping babel::*string-vector-mappings* :utf-8))
       (counter (babel-encodings:octet-counter foreign)))
  (assert (not (eq foreign vector)))
  (assert (equal '(4 3) (multiple-value-list (funcall counter "xA雪🚀" 1 4 7))))
  (dolist (maximum '(0 -1))
    (assert (equal '(8 4) (multiple-value-list (funcall counter "xA雪🚀" 1 4 maximum)))))
  (cffi:with-foreign-pointer (p 6)
    (dotimes (i 6) (setf (cffi:mem-ref p :uint8 i) #x5a))
    (assert (= 3 (funcall (babel-encodings:encoder foreign) "x雪y" 1 2 p 2)))
    (assert (equal '(90 90 233 155 170 90)
                   (loop for i below 6 collect (cffi:mem-ref p :uint8 i))))))

(let ((text (coerce (list #\A (code-char 0) #\雪 #\B) 'string)))
  (multiple-value-bind (p bytes)
      (cffi:foreign-string-alloc text :start 1 :end 4 :encoding :utf-8 :null-terminated-p nil)
    (unwind-protect
         (progn
           (assert (= 5 bytes))
           (assert (equal '(0 233 155 170 66)
                          (loop for i below bytes collect (cffi:mem-ref p :uint8 i))))
           (assert (string= (subseq text 1 4)
                            (cffi:foreign-string-to-lisp p :count bytes :encoding :utf-8))))
      (cffi:foreign-string-free p))))

;; The native capacity includes NUL, including the one-byte destination
;; that CFFI cannot distinguish from Babel's unbounded size query.
(cffi:with-foreign-pointer (p 2)
  (setf (cffi:mem-ref p :uint8 0) #x5a
        (cffi:mem-ref p :uint8 1) #x5a)
  (zk-ffi:write-c-string p "abc" 1)
  (assert (and (zerop (cffi:mem-ref p :uint8 0)) (= #x5a (cffi:mem-ref p :uint8 1)))))

(format t "~&CFFI UTF-8 buffer and Babel mapping checks passed.~%")
(in-package #:cl-user)
