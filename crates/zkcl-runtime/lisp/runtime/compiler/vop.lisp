(cl:defpackage #:zk-sys
  (:use #:common-lisp)
  (:export #:defknown #:define-vop))
(cl:in-package #:zk-sys)

(defmacro defknown (name arguments result)
  "Proclaim a Lisp function signature for compilation."
  `(declaim (ftype (function ,arguments ,result) ,name)))

(defmacro define-vop (name &body options)
  "Declare an instruction template for subsequent named calls in this compiled file."
  `(%define-vop ,name ,@options))

(in-package #:cl-user)
