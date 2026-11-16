;;;; Structured concurrency with ZK-ASYNC (ASYNC in this package).
;;;; Run from the repository root with
;;;;   ./target/release/zkcl run examples/async.lisp
;;;; Lisp tasks take turns in one runtime. A wait can let other tasks run, and
;;;; WITH-TASK-GROUP owns the lifetime of every child spawned into it.

(eval-when (:compile-toplevel :load-toplevel :execute)
  (require :zk-pbt))

(defpackage #:async-example
  (:use #:cl)
  (:local-nicknames (#:async #:zk-async)))
(in-package #:async-example)

;;; ---------------------------------------------------------------------------
;;; 1. Task groups, implicit join, and explicit results
;;; ---------------------------------------------------------------------------

(defun submit-index ()
  "Return the group body's values, the join trace, and the worker handle."
  (let ((submitted (async:make-event))
        (worker nil)
        (trace nil))
    (let ((body-values
            (multiple-value-list
             (async:with-task-group (group)
               (setf worker
                     (async:spawn-task (group :name "indexer")
                       (async:wait-event submitted)
                       (push :index-built trace)
                       (values :indexed 42)))
               (push :submitted trace)
               (async:set-event submitted)
               (values :accepted 1)))))
      ;; The group joined the worker without an explicit AWAIT.
      (push :outside-group trace)
      (values body-values (nreverse trace) worker))))

(zk-test:deftest group-joins-before-returning ()
  (multiple-value-bind (body-values trace worker) (submit-index)
    (zk-test:is-equal '(:accepted 1) body-values)
    (zk-test:is-equal '(:submitted :index-built :outside-group) trace)
    (zk-test:is (async:taskp worker))
    (zk-test:is-equal "indexer" (async:task-name worker))
    (zk-test:is (async:task-completed-p worker))
    (zk-test:is-values (async:await worker) :indexed 42)))

(zk-test:deftest await-keeps-zero-and-multiple-values ()
  (let ((release (async:make-event)))
    (async:with-task-group (group)
      (let ((counted (async:spawn-task (group)
                       (async:wait-event release)
                       (values 42 :rows (async:current-task))))
            (empty (async:spawn group (lambda () (values)))))
        (zk-test:is (async:task-running-p counted))
        (async:set-event release)
        ;; AWAIT-ALL collects primary values in input order.
        (zk-test:is-equal '(nil 42) (async:await-all (list empty counted)))
        (zk-test:is-values (async:await empty))
        (zk-test:is-values (async:await counted) 42 :rows counted)))))

(zk-test:deftest implicit-join-participates-in-cycle-detection ()
  (let ((owner (async:current-task)))
    ;; The owner must join the child to leave the group. A child that waits
    ;; for the owner to finish creates a dependency cycle.
    (let ((condition (zk-test:signals async:async-error
                       (async:with-task-group (group)
                         (async:spawn-task (group) (async:await owner))
                         :submitted))))
      (zk-test:is (search "dependency cycle" (async:async-error-message condition))))))

;;; ---------------------------------------------------------------------------
;;; 2. FIFO channels, backpressure, and a disconnectable subscription
;;; ---------------------------------------------------------------------------

(defun next-update (inbox stop)
  ;; If both branches are ready, disconnect before consuming more messages.
  (async:select
    (:biased t)
    ((:event stop) (values nil :disconnected))
    ((:recv inbox message received-p)
     (values message (if received-p :message :drained)))))

(defun read-updates (inbox stop limit)
  (let ((messages nil)
        (count 0))
    (loop
      (multiple-value-bind (message status) (next-update inbox stop)
        (unless (eq status :message)
          (return (values (nreverse messages) status)))
        (push message messages)
        (incf count)
        (when (eql limit count) (async:set-event stop))))))

(defun subscribe (updates &key limit (capacity 1))
  "Return the messages read, exit reason, producer count at EOF, and trace."
  (check-type limit (or null (integer 1)))
  (let ((inbox (async:make-channel :capacity capacity))
        (stop (async:make-event))
        (trace nil))
    (multiple-value-bind (messages outcome sent)
        (async:with-task-group (group)
          ;; At EOF, await the producer's result explicitly. On disconnect,
          ;; group exit cancels this :NO-WAIT child and joins its cleanup.
          (let ((producer
                  (async:spawn-task (group :name "subscription" :no-wait t)
                    (unwind-protect
                        (dolist (update updates (values (length updates) :complete))
                          (async:channel-send inbox update))
                      (async:close-channel inbox)
                      (push :producer-closed trace)))))
            (multiple-value-bind (messages outcome) (read-updates inbox stop limit)
              (let ((sent nil))
                (when (eq outcome :drained)
                  (multiple-value-bind (count status) (async:await producer)
                    (assert (eq status :complete))
                    (setf sent count)))
                (push :leaving-group trace)
                (values messages outcome sent)))))
      (push :outside-group trace)
      (values messages outcome sent (nreverse trace)))))

(zk-test:deftest drain-subscription ()
  ;; A published NIL counts as a message. A drained channel carries no message.
  (zk-test:is-values (subscribe '(nil :updated :saved))
                     '(nil :updated :saved) :drained 3
                     '(:producer-closed :leaving-group :outside-group)))

(zk-test:deftest disconnect-with-pending-messages ()
  ;; With four updates, one consumed message and a capacity of one, the
  ;; producer still has work when the reader leaves the group.
  (zk-test:is-values (subscribe '(:first :second :third :fourth) :limit 1)
                     '(:first) :disconnected nil
                     '(:leaving-group :producer-closed :outside-group)))

(zk-test:deftest stop-beats-buffered-message ()
  (let ((inbox (async:make-channel :capacity 1))
        (stop (async:make-event :signaled t)))
    (async:channel-send inbox nil)
    (zk-test:is-values (next-update inbox stop) nil :disconnected)
    (zk-test:is-values (async:channel-recv inbox) nil t)))

(zk-test:deftest channel-probes-and-close ()
  (let ((inbox (async:make-channel :capacity 1)))
    (zk-test:is (async:channelp inbox))
    (zk-test:is-values (async:channel-try-recv inbox) nil nil)
    (zk-test:is (async:channel-try-send inbox nil))
    (zk-test:is (not (async:channel-try-send inbox :full)))
    (zk-test:is (async:close-channel inbox))
    (zk-test:is (async:channel-closed-p inbox))
    ;; A closed channel still delivers its buffered messages, including NIL.
    (zk-test:is-values (async:channel-recv inbox) nil t)
    (zk-test:is-values (async:channel-recv inbox) nil nil)
    (zk-test:signals async:async-error (async:channel-send inbox :too-late))))

(zk-pbt:defproperty subscription-drains-in-fifo-order
    ((updates (zk-pbt:list-of (zk-pbt:elements nil :connected :updated :saved) :max 12))
     (capacity (zk-pbt:elements 0 1 3 nil)))
  "Rendezvous, bounded, and unbounded channels preserve order and payloads."
  (:trials 40)
  (:examples (nil 0) ((nil :updated nil) 0) ((:saved :connected) nil))
  (zk-test:is-values (subscribe updates :capacity capacity)
                     updates :drained (length updates)
                     '(:producer-closed :leaving-group :outside-group)))

;;; ---------------------------------------------------------------------------
;;; 3. Child failure and nonlocal exit
;;; ---------------------------------------------------------------------------

(define-condition index-error (error)
  ((code :initarg :code :reader index-error-code))
  (:report (lambda (condition stream)
             (format stream "Index worker failed with code ~D"
                     (index-error-code condition)))))

(defun run-failing-index (failure)
  (let ((started (async:make-event))
        (reply (async:make-event))
        (trace nil))
    (handler-case
        (async:with-task-group (group)
          (async:spawn-task (group :name "waiting for reply")
            (unwind-protect
                 (progn
                   (async:set-event started)
                   (async:wait-event reply))
              (push :sibling-released trace)))
          (async:spawn-task (group :name "failing indexer")
            (async:wait-event started)
            (push :worker-failed trace)
            (error failure))
          :submitted)
      (index-error (condition)
        (push :failure-observed trace)
        (values condition (nreverse trace))))))

(zk-test:deftest child-failure-drains-siblings-before-propagating ()
  (let ((failure (make-condition 'index-error :code 503)))
    (zk-test:is-values (run-failing-index failure)
                       failure '(:worker-failed :sibling-released :failure-observed))))

(zk-test:deftest allowed-failure-is-observed-on-await ()
  (let ((failure (make-condition 'index-error :code 404))
        (release (async:make-event)))
    (async:with-task-group (group)
      (let ((failed (async:spawn-task (group :allow-failure t) (error failure)))
            (sibling (async:spawn-task (group)
                       (async:wait-event release)
                       :indexed)))
        (zk-test:is-eq failure (zk-test:signals index-error (async:await failed)))
        (zk-test:is (async:task-failed-p failed))
        (async:set-event release)
        (zk-test:is-eq :indexed (async:await sibling))))))

(zk-test:deftest throw-cancels-an-ordinary-child ()
  (let ((started (async:make-event))
        (reply (async:make-event))
        (released nil))
    (zk-test:is-eq :abandoned
      (catch 'abandon
        (async:with-task-group (group)
          (async:spawn-task (group)
            (unwind-protect
                 (progn
                   (async:set-event started)
                   (async:wait-event reply))
              (setf released t)))
          (async:wait-event started)
          (throw 'abandon :abandoned))))
    (zk-test:is released)))

;;; ---------------------------------------------------------------------------
;;; 4. Explicit cancellation, shielding, and cancellation-safe permits
;;; ---------------------------------------------------------------------------

(defun cancel-during-flush ()
  (let ((flushing (async:make-event))
        (ack (async:make-event))
        (trace nil))
    (values
     (async:with-task-group (group)
       (async:with-cancel-scope (scope :on-cancel (progn
                                                   (push :on-cancel trace)
                                                   :cancelled))
         (async:spawn-task (group :name "disconnect")
           (async:wait-event flushing)
           (async:cancel-scope scope)
           (push :cancel-requested trace)
           (async:set-event ack))
         (unwind-protect
              (progn
                ;; Keep the acknowledgement wait active during cancellation.
                (async:protect-from-cancel
                  (async:set-event flushing)
                  (async:wait-event ack)
                  (push :flushed trace))
                (push :after-shield trace))
           (push :resource-released trace))))
     (nreverse trace))))

(zk-test:deftest shield-defers-cancellation-until-flush-finishes ()
  (zk-test:is-values (cancel-during-flush)
                     :cancelled '(:cancel-requested :flushed :resource-released :on-cancel)))

(zk-test:deftest cancel-group-closes-it-to-new-tasks ()
  (let ((started (async:make-event))
        (reply (async:make-event))
        (released nil))
    (async:with-task-group (group)
      (zk-test:is (async:task-group-p group))
      (async:spawn-task (group)
        (unwind-protect
             (progn
               (async:set-event started)
               (async:wait-event reply))
          (setf released t)))
      (async:wait-event started)
      (zk-test:is (async:cancel-task-group group))
      (zk-test:is (async:task-group-cancelled-p group))
      (zk-test:is (async:task-group-closed-p group))
      (zk-test:signals async:async-error (async:spawn-task (group) :too-late)))
    (zk-test:is released)))

(zk-test:deftest task-cancellation-releases-its-permit ()
  (let ((slots (async:make-semaphore 1))
        (admitted (async:make-event))
        (reply (async:make-event)))
    (async:with-task-group (group)
      (let ((request (async:spawn-task (group :name "request")
                       (async:with-semaphore (slots)
                         (async:set-event admitted)
                         (async:wait-event reply)))))
        (async:wait-event admitted)
        (zk-test:is= 0 (async:semaphore-count slots))
        (zk-test:is (not (async:try-acquire-semaphore slots)))
        (let ((next-request (async:spawn-task (group)
                              (async:with-semaphore (slots) :admitted))))
          (zk-test:is (async:task-running-p next-request))
          (zk-test:is (async:cancel-task request))
          (let ((condition (zk-test:signals async:waited-task-cancelled
                             (async:await request))))
            (zk-test:is-eq request (async:waited-task-cancelled-task condition)))
          (zk-test:is-eq :admitted (async:await next-request)))
        (zk-test:is (async:task-done-p request))
        (zk-test:is= 1 (async:semaphore-count slots))
        (zk-test:is (async:try-acquire-semaphore slots))
        (zk-test:is= 1 (async:release-semaphore slots))))))

;;; ---------------------------------------------------------------------------
;;; 5. Deadlines and fallback values
;;; ---------------------------------------------------------------------------

(defun timed-request ()
  (let ((trace nil))
    (async:with-timeout (1/20 :on-timeout
                              (progn
                                (push :using-cache trace)
                                (values :cached (nreverse trace))))
      (unwind-protect
           ;; SLEEP parks this task. The earlier deadline cancels the wait.
           (sleep 5)
        (push :request-released trace)))))

(zk-test:deftest deadline-unwinds-before-fallback ()
  (zk-test:is-values (timed-request) :cached '(:request-released :using-cache)))

(zk-test:deftest timeout-error-and-completion-flag ()
  (let ((condition (zk-test:signals async:timeout-error
                     (async:with-timeout (0)
                       (error "A zero deadline skips the body")))))
    (zk-test:is= 0 (async:timeout-error-seconds condition)))
  (zk-test:is-values (async:with-timeout-opt (0) :skipped) nil nil)
  ;; A successful NIL has a true completion flag. Only the primary value is kept.
  (zk-test:is-values (async:with-timeout-opt (5) (values nil :metadata)) nil t))

;;; ---------------------------------------------------------------------------
;;; 6. Events, promises, and selection across different wait sources
;;; ---------------------------------------------------------------------------

(zk-test:deftest event-stays-signaled-until-reset ()
  (let ((gate (async:make-event)))
    (async:with-task-group (group)
      (let ((reader (async:spawn-task (group) (async:wait-event gate) :read))
            (writer (async:spawn-task (group) (async:wait-event gate) :wrote)))
        (zk-test:is (async:set-event gate))
        (zk-test:is-equal '(:read :wrote) (async:await-all (list reader writer)))
        (zk-test:is (async:wait-event gate))))
    (zk-test:is (async:reset-event gate))
    (zk-test:is (not (async:event-set-p gate)))))

(zk-test:deftest promise-settles-once-with-values-or-a-condition ()
  (let ((reply (async:make-promise)))
    (async:with-task-group (group)
      (let ((consumer (async:spawn-task (group) (async:await-promise reply))))
        (zk-test:is (not (async:promise-done-p reply)))
        (zk-test:is (async:fulfill-promise reply nil :cache-hit))
        (zk-test:is (not (async:fulfill-promise reply :late)))
        (zk-test:is (async:promise-done-p reply))
        (zk-test:is-values (async:await consumer) nil :cache-hit)
        (zk-test:is-values (async:await-promise reply) nil :cache-hit))))
  (let ((reply (async:make-promise))
        (failure (make-condition 'index-error :code 401)))
    (zk-test:is (async:fail-promise reply failure))
    (zk-test:is-eq failure (zk-test:signals index-error (async:await-promise reply)))))

(defun next-reply-event (updates reply)
  (async:select
    ((:recv updates payload received-p)
     (values (if received-p :update :closed) payload))
    ((:promise reply settled)
     ;; The clause binds the promise handle. AWAIT-PROMISE extracts its values.
     (values :reply (multiple-value-list (async:await-promise settled))))))

(zk-test:deftest select-multiplexes-updates-and-reply ()
  (let ((updates (async:make-channel :capacity 1))
        (reply (async:make-promise)))
    (async:channel-send updates 50)
    (zk-test:is-values (next-reply-event updates reply) :update 50)
    (async:with-task-group (group)
      (async:spawn-task (group) (async:fulfill-promise reply :indexed 42))
      (zk-test:is-values (next-reply-event updates reply) :reply '(:indexed 42)))))

(zk-test:deftest select-commits-only-one-ready-send ()
  (let ((primary (async:make-channel :capacity 1))
        (backup (async:make-channel :capacity 1)))
    (zk-test:is-eq :payload
      (async:select (:biased t)
        ((:send primary :payload sent) sent)
        ((:send backup :payload) :backup)))
    (zk-test:is-values (async:channel-recv primary) :payload t)
    (zk-test:is-eq :idle
      (async:select
        ((:recv backup message) message)
        (otherwise :idle)))
    (zk-test:is-eq :timed-out
      (async:select
        ((:recv backup message) message)
        ((:timeout 0) :timed-out)))))

(zk-test:deftest unbiased-select-rotates-between-ready-sources ()
  (let ((left (async:make-channel))
        (right (async:make-channel)))
    (dotimes (i 2)
      (async:channel-send left :left)
      (async:channel-send right :right))
    (let ((selected (loop repeat 2 collect
                      (async:select
                        ((:recv left message) message)
                        ((:recv right message) message)))))
      (zk-test:is-equal '(:left :right) (sort selected #'string< :key #'symbol-name)))))

;;; ---------------------------------------------------------------------------
;;; 7. Observing the first completion and racing replicas
;;; ---------------------------------------------------------------------------

(zk-test:deftest await-any-leaves-other-tasks-running ()
  (let ((release (async:make-event)))
    (async:with-task-group (group)
      (let ((primary (async:spawn-task (group)
                       (async:wait-event release)
                       :primary))
            (replica (async:spawn-task (group) (values :replica 42))))
        (zk-test:is-eq replica (async:await-any (list primary replica)))
        (zk-test:is (async:task-running-p primary))
        (zk-test:is-values (async:select
                             ((:task replica finished) (async:await finished)))
                           :replica 42)
        (async:set-event release)
        (zk-test:is-eq :primary (async:await primary))
        ;; If both are already finished, the first handle in the list wins.
        (zk-test:is-eq primary (async:await-any (list primary replica)))))))

(defun first-replica ()
  (let ((primary-started (async:make-event))
        (primary-reply (async:make-event))
        (primary-released nil))
    ;; RACE owns a fresh group and returns the winner's values after joining losers.
    (multiple-value-bind (result source)
        (async:race
          (unwind-protect
               (progn
                 (async:set-event primary-started)
                 (async:wait-event primary-reply))
            (setf primary-released t))
          (progn
            (async:wait-event primary-started)
            (async:yield)
            (values 42 :replica)))
      (values result source primary-released))))

(zk-test:deftest race-keeps-values-and-joins-the-loser ()
  (zk-test:is-values (first-replica) 42 :replica t))

(zk-test:deftest race-propagates-a-first-failure ()
  (let ((reply (async:make-event))
        (failure (make-condition 'index-error :code 502)))
    ;; RACE-CALLS takes a list of thunks. Failure can be the first completion.
    (zk-test:is-eq failure
      (zk-test:signals index-error
        (async:race-calls (list (lambda () (async:wait-event reply))
                                (lambda () (error failure))))))))

;;; ---------------------------------------------------------------------------
;;; 8. Worker-backed file I/O in the native CLI
;;; ---------------------------------------------------------------------------

#-(or javascript wasm32 wasm64)
(progn
  (defparameter *example-file* *load-truename*)

  (defun read-header-ranges (pathname)
    ;; The workers read half-open byte ranges and deliver octet vectors to Lisp.
    (async:with-task-group (group)
      (let ((prefix (async:spawn-task (group)
                      (async:read-file-octets pathname :start 0 :end 4)))
            (suffix (async:spawn-task (group)
                      (async:read-file-octets pathname :start 4 :end 8))))
        (async:await-all (list prefix suffix)))))

  (zk-test:deftest async-file-ranges-match-binary-stream ()
    ;; Use the file being loaded, so the same test also works from a FASL.
    (let ((header (make-array 8 :element-type '(unsigned-byte 8))))
      (with-open-file (stream *example-file* :element-type '(unsigned-byte 8))
        (zk-test:is= 8 (read-sequence header stream)))
      (destructuring-bind (prefix suffix) (read-header-ranges *example-file*)
        (zk-test:is-equalp (subseq header 0 4) prefix)
        (zk-test:is-equalp (subseq header 4 8) suffix)))))

;;; ---------------------------------------------------------------------------
;;; Run the suite, then print the lifecycle and result of selected examples.
;;; ---------------------------------------------------------------------------

(zk-test:defsuite async-example
  group-joins-before-returning
  await-keeps-zero-and-multiple-values
  implicit-join-participates-in-cycle-detection
  drain-subscription
  disconnect-with-pending-messages
  stop-beats-buffered-message
  channel-probes-and-close
  subscription-drains-in-fifo-order
  child-failure-drains-siblings-before-propagating
  allowed-failure-is-observed-on-await
  throw-cancels-an-ordinary-child
  shield-defers-cancellation-until-flush-finishes
  cancel-group-closes-it-to-new-tasks
  task-cancellation-releases-its-permit
  deadline-unwinds-before-fallback
  timeout-error-and-completion-flag
  event-stays-signaled-until-reset
  promise-settles-once-with-values-or-a-condition
  select-multiplexes-updates-and-reply
  select-commits-only-one-ready-send
  unbiased-select-rotates-between-ready-sources
  await-any-leaves-other-tasks-running
  race-keeps-values-and-joins-the-loser
  race-propagates-a-first-failure
  #-(or javascript wasm32 wasm64) async-file-ranges-match-binary-stream)

(defun show-async-examples ()
  (format t "~&~%=== Live Structured Concurrency Examples ===~%")
  (multiple-value-bind (body-values trace worker) (submit-index)
    (format t "~%Task group returned ~S and joined a worker with values ~S~%"
            body-values (multiple-value-list (async:await worker)))
    (format t "  ~{~A~^ -> ~}~%" trace))

  (dolist (limit '(nil 1))
    (multiple-value-bind (messages outcome sent trace)
        (subscribe '(nil :connected :updated :saved) :limit limit)
      (format t "~%Subscription ~A after receiving ~S~%" outcome messages)
      (when sent (format t "  Producer returned a count of ~D~%" sent))
      (format t "  ~{~A~^ -> ~}~%" trace)))

  (multiple-value-bind (condition trace) (run-failing-index (make-condition 'index-error :code 503))
    (format t "~%~A~%  ~{~A~^ -> ~}~%" condition trace))
  (multiple-value-bind (outcome trace) (cancel-during-flush)
    (format t "~%Shielded scope returned ~S~%  ~{~A~^ -> ~}~%" outcome trace))
  (multiple-value-bind (outcome trace) (timed-request)
    (format t "~%Deadline returned ~S~%  ~{~A~^ -> ~}~%" outcome trace))
  (multiple-value-bind (result source primary-released) (first-replica)
    (format t "~%Race returned ~S from ~A (primary released ~S)~%"
            result source primary-released)))

(let ((report (zk-test:run 'async-example :seed 42)))
  (zk-test:print-report report)
  (if (eq :passed (zk-test:report-status report))
      (show-async-examples)
      (zk:set-exit-code 1)))
