(cl:in-package #:zk-simd)

(cl:define-condition simd-error (cl:error)
  ((lane :initarg :lane :initform cl:nil :reader condition-lane)
   (level :initarg :level :initform cl:nil :reader condition-level)
   (index :initarg :index :initform cl:nil :reader condition-index)))

(cl:defun report-arithmetic-error (condition stream)
  (cl:let ((operation (cl:when (cl:slot-boundp condition 'cl::operation)
                        (cl:arithmetic-error-operation condition))))
    (cl:format stream "~@[~S signaled ~]~S~@[ at lane ~D~]~@[ at reduction level ~D~]~@[ at array index ~D~]"
               operation (cl:class-name (cl:class-of condition)) (condition-lane condition)
               (condition-level condition) (condition-index condition))))

(cl:define-condition simd-division-by-zero (cl:division-by-zero simd-error) ()
  (:report report-arithmetic-error))

(cl:define-condition simd-floating-point-overflow (cl:floating-point-overflow simd-error) ()
  (:report report-arithmetic-error))

(cl:define-condition simd-floating-point-invalid-operation
    (cl:floating-point-invalid-operation simd-error) ()
  (:report report-arithmetic-error))

(cl:define-condition integer-overflow (cl:arithmetic-error simd-error) ()
  (:report report-arithmetic-error))

(cl:define-condition bounds-error (simd-error)
  ((array :initarg :array :initform cl:nil :reader condition-array))
  (:report (cl:lambda (condition stream)
             (cl:format stream "SIMD array index ~S is out of bounds~@[ at lane ~D~]"
                        (condition-index condition) (condition-lane condition)))))

(cl:in-package #:cl-user)
