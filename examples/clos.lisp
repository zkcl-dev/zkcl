;;;; Render a sensor event as text or as a record of its initialized slots.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-test))

(defpackage #:clos-example
  (:use #:cl)
  (:local-nicknames (#:mop #:zk-mop)))
(in-package #:clos-example)

(defclass event ()
  ((source :initarg :source :reader event-source)))

(defclass reading (event)
  ((value :initarg :value :reader reading-value)
   (unit :initarg :unit)))

(defclass disconnected (event)
  ((reason :initarg :reason :reader disconnected-reason)))

(defgeneric render (event target)
  (:documentation "Return a representation and its severity."))

(defmethod render ((event reading) (target (eql :text)))
  (declare (ignore target))
  (values (format nil "~A reports ~A" (event-source event) (reading-value event)) :info))

(defmethod render ((event disconnected) (target (eql :text)))
  (declare (ignore target))
  (values (format nil "~A disconnected (~A)" (event-source event) (disconnected-reason event))
          :warning))

(defmethod render ((event event) (target (eql :record)))
  (declare (ignore target))
  ;; Effective slots include those inherited from EVENT. UNIT may be unbound.
  (values (loop for slot in (mop:class-slots (class-of event))
                for name = (mop:slot-definition-name slot)
                when (slot-boundp event name) collect (cons name (slot-value event name)))
          :record))

(defvar *render-log* nil)

(defmethod render :around ((event event) target)
  (multiple-value-prog1 (call-next-method)
    (when *render-log* (format *render-log* "Rendered ~A as ~A~%" (event-source event) target))))

(zk-test:deftest dispatch-and-method-combination ()
  (let ((reading (make-instance 'reading :source "sensor-1" :value 24))
        (lost (make-instance 'disconnected :source "sensor-2" :reason :offline)))
    (zk-test:is-values (render reading :text) "sensor-1 reports 24" :info)
    (zk-test:is-equal (format nil "Rendered sensor-2 as TEXT~%")
                      (with-output-to-string (*render-log*)
                        (zk-test:is-values (render lost :text)
                                           "sensor-2 disconnected (OFFLINE)" :warning)))))

(zk-test:deftest inherited-and-unbound-slots ()
  (multiple-value-bind (fields severity)
      (render (make-instance 'reading :source "sensor-1" :value 24) :record)
    (zk-test:is-eq :record severity)
    (zk-test:is-equal "sensor-1" (cdr (assoc 'source fields)))
    (zk-test:is= 24 (cdr (assoc 'value fields)))
    (zk-test:is-eq nil (assoc 'unit fields))))

(zk-test:defsuite clos-example dispatch-and-method-combination inherited-and-unbound-slots)

(let ((report (zk-test:run 'clos-example)))
  (zk-test:print-report report)
  (unless (eq :passed (zk-test:report-status report))
    (zk:set-exit-code 1)))

(let ((event (make-instance 'reading :source "sensor-1" :value 24 :unit "°C"))
      (*render-log* *standard-output*))
  (terpri)
  (dolist (target '(:text :record))
    (multiple-value-bind (value severity) (render event target)
      (format t "  ~A ~S~%" severity value))))
