(cl:defpackage #:zk-sys
  (:use #:common-lisp)
  (:export #:defknown #:deftransform #:define-vop))
(cl:in-package #:zk-sys)

(defmacro defknown (name arguments result)
  "Proclaim a Lisp function signature for compilation."
  `(declaim (ftype (function ,arguments ,result) ,name)))

(defmacro deftransform (function operands operation)
  "Declare a guarded semantic operation for calls to FUNCTION."
  `(%deftransform ,function ,operands ,operation))

(defmacro define-vop (name operation &body options)
  "Declare a predefined machine instruction sequence for OPERATION. Cost defaults to 8."
  `(%define-vop ,name ,operation ,@options))

(in-package #:cl-user)
