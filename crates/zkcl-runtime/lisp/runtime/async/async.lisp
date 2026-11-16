;;;; Structured concurrency, cooperative suspension, and task cancellation.
(defpackage #:zk-async
  (:use #:cl)
  (:export
   #:async-error
   #:async-error-message
   #:timeout-error
   #:timeout-error-seconds
   #:waited-task-cancelled
   #:waited-task-cancelled-task
   #:with-task-group
   #:task-group-p
   #:task-group-cancelled-p
   #:task-group-closed-p
   #:cancel-task-group
   #:spawn
   #:spawn-task
   #:await
   #:await-any
   #:await-all
   #:race
   #:race-calls
   #:select
   #:cancel-task
   #:task-cancelled-p
   #:protect-from-cancel
   #:with-cancel-scope
   #:cancel-scope-p
   #:cancel-scope
   #:cancel-scope-cancelled-p
   #:with-timeout
   #:with-timeout-opt
   #:yield
   #:current-task
   #:taskp
   #:task-name
   #:task-done-p
   #:task-running-p
   #:task-completed-p
   #:task-failed-p
   #:make-promise
   #:promisep
   #:promise-done-p
   #:fulfill-promise
   #:fail-promise
   #:await-promise
   #:make-event
   #:eventp
   #:event-set-p
   #:set-event
   #:reset-event
   #:wait-event
   #:make-semaphore
   #:semaphorep
   #:semaphore-count
   #:acquire-semaphore
   #:try-acquire-semaphore
   #:release-semaphore
   #:with-semaphore
   #:make-channel
   #:channelp
   #:channel-closed-p
   #:close-channel
   #:channel-send
   #:channel-recv
   #:channel-try-send
   #:channel-try-recv
   #:read-file-octets))

(in-package #:zk-async)

(define-condition async-error (error)
  ((message :initarg :message :initform "Async operation failed" :reader async-error-message))
  (:report (lambda (condition stream)
             (write-string (async-error-message condition) stream))))

(define-condition timeout-error (async-error)
  ((seconds :initarg :seconds :reader timeout-error-seconds))
  (:report (lambda (condition stream)
             (format stream "Operation timed out after ~S seconds"
                     (timeout-error-seconds condition)))))

(define-condition waited-task-cancelled (async-error)
  ((task :initarg :task :reader waited-task-cancelled-task))
  (:report (lambda (condition stream)
             (format stream "Waited task ~S was cancelled"
                     (waited-task-cancelled-task condition)))))

(defun %signal-async-error (message)
  (error 'async-error :message message))

(defun %signal-waited-task-cancelled (task)
  (error 'waited-task-cancelled :task task))

(defmacro with-task-group ((group-var) &body body)
  "Run BODY with GROUP-VAR bound to a task group. Preserve all of BODY's values.
On normal exit, cancel :NO-WAIT children and join every child. On a nonlocal
exit, cancel all outstanding children and join them before completing the exit."
  (let ((group (gensym "GROUP")))
    `(zk::%call-with-task-group
      (lambda (,group)
        (unwind-protect
             (multiple-value-prog1
                 (let ((,group-var ,group)) ,@body)
               (zk::%finish-task-group ,group nil))
          (zk::%finish-task-group ,group t))))))

(defun spawn (group fn &key no-wait allow-failure name)
  "Spawn a child task executing FN inside GROUP."
  (zk::%spawn group (if (functionp fn) fn (fdefinition fn)) no-wait allow-failure name))

(defmacro spawn-task ((group &key no-wait allow-failure name) &body body)
  "Spawn a child task executing BODY inside GROUP."
  `(zk::%spawn ,group (lambda () ,@body) ,no-wait ,allow-failure ,name))

(defun await (task)
  "Return TASK's values, waiting if needed. Signal its error or WAITED-TASK-CANCELLED
if it failed or was cancelled. Signal ASYNC-ERROR if waiting would create a
dependency cycle without a usable deadline or cancellation before joining."
  (zk::%await task))

(defun cancel-task (task)
  "Request cooperative cancellation of TASK. Return true for a new request."
  (zk::%cancel-task task))

(defun task-cancelled-p (&optional task)
  "Return true if TASK (defaulting to the current task) has a pending cancellation."
  (zk::%task-cancelled-p task))

(defmacro protect-from-cancel (&body body)
  "Defer cancellation from enclosing scopes until BODY exits. Task groups and
timeouts established within BODY can still handle their own failures or deadlines."
  `(zk::%protect-from-cancel (lambda () ,@body)))

(defmacro with-cancel-scope ((scope-var &key (on-cancel nil on-cancel-p)) &body body)
  "Run BODY with SCOPE-VAR bound to a cancellable scope. Preserve BODY values on
normal exit. If cancelled via CANCEL-SCOPE, unwind BODY cleanly and evaluate
ON-CANCEL (or return NIL if omitted)."
  `(zk::%with-cancel-scope
    (lambda (,scope-var) ,@body)
    ,(when on-cancel-p
       `(lambda () ,on-cancel))))

(defun cancel-scope-p (object)
  "Return true if OBJECT is a cancel scope."
  (zk::%cancel-scope-p object))

(defun cancel-scope (scope)
  "Request cooperative cancellation of SCOPE. Return true if newly triggered."
  (zk::%cancel-scope scope))

(defun cancel-scope-cancelled-p (scope)
  "Return true if SCOPE has been cancelled or unwound due to cancellation."
  (zk::%cancel-scope-cancelled-p scope))

(defmacro with-timeout ((seconds &key (on-timeout nil on-timeout-p)) &body body)
  "Run BODY with a timeout of SECONDS. Zero skips BODY. On expiration cancel BODY
and wait for its cleanups before running ON-TIMEOUT or signaling TIMEOUT-ERROR."
  (let ((secs (gensym "SECONDS")))
    `(let ((,secs ,seconds))
       (zk::%with-timeout
        ,secs
        (lambda () ,@body)
        (lambda ()
          ,(if on-timeout-p
               on-timeout
               `(error 'timeout-error :seconds ,secs)))))))

(defmacro with-timeout-opt ((seconds) &body body)
  "Run BODY with a timeout of SECONDS. Returns (VALUES primary-value T) on
completion or (VALUES NIL NIL) if timed out."
  `(with-timeout (,seconds :on-timeout (values nil nil))
     (values (locally ,@body) t)))

(defun yield ()
  "Yield execution to other ready tasks in the current runtime."
  (zk::%yield))

(defun current-task ()
  "Return the stable handle of the currently executing task."
  (zk::%current-task))

(defun taskp (object)
  "Return true if OBJECT is a task handle."
  (zk::%taskp object))

(defun task-name (task)
  "Return the optional name of TASK, or NIL."
  (zk::%task-name task))

(defun task-done-p (task)
  "Return true if TASK has finished running."
  (zk::%task-done-p task))

(defun task-running-p (task)
  "Return true if TASK is still running."
  (zk::%task-running-p task))

(defun task-completed-p (task)
  "Return true if TASK finished normally without error or cancellation."
  (zk::%task-completed-p task))

(defun task-failed-p (task)
  "Return true if TASK finished with an unhandled failure."
  (zk::%task-failed-p task))

(defun task-group-p (object)
  "Return true if OBJECT is a task group handle."
  (zk::%task-group-p object))

(defun task-group-cancelled-p (group)
  "Return true if GROUP is currently cancelling."
  (zk::%task-group-cancelled-p group))

(defun task-group-closed-p (group)
  "Return true if GROUP is no longer open for new spawns."
  (zk::%task-group-closed-p group))

(defun cancel-task-group (group)
  "Cancel all tasks currently in GROUP and close GROUP to new spawns.
Return true if GROUP transitioned to cancelling."
  (zk::%cancel-task-group group))

(defun await-any (tasks)
  "Return the first finished task in TASKS, using list order if several have
already finished. Otherwise wait and return the task whose completion wakes
this wait."
  (zk::%await-any tasks))

(defun await-all (tasks)
  "Await each task in TASKS and return a list of their primary values in order."
  (mapcar #'await tasks))

(defun race-calls (thunks)
  "Run zero-argument functions in THUNKS concurrently inside a fresh task group
and return the multiple values of the first task to finish, cancelling and joining
remaining tasks before returning."
  (when (null thunks)
    (error 'program-error :format-control "RACE requires at least one branch."))
  (if (null (rest thunks))
      (funcall (first thunks))
      (let (results)
        (with-task-group (group)
          (let* ((tasks (mapcar (lambda (fn)
                                  (spawn group fn :no-wait t))
                                thunks))
                 (winner (await-any tasks)))
            (cancel-task-group group)
            (setq results (multiple-value-list (await winner)))))
        (values-list results))))

(defmacro race (&body forms)
  "Evaluate FORMS concurrently and return the multiple values of the first
branch to finish, cancelling and joining remaining branches before returning."
  (when (null forms)
    (error 'program-error :format-control "RACE requires at least one branch."))
  (if (null (rest forms))
      (first forms)
      `(race-calls (list ,@(mapcar (lambda (form) `(lambda () ,form)) forms)))))

(defun make-promise ()
  "Create a one-shot multi-value async promise."
  (zk::%make-promise))

(defun promisep (object)
  "Return true if OBJECT is an async promise."
  (zk::%promisep object))

(defun promise-done-p (promise)
  "Return true if PROMISE has been fulfilled or failed."
  (zk::%promise-done-p promise))

(defun fulfill-promise (promise &rest values)
  "Resolve PROMISE with VALUES and wake all waiters. Return true if newly settled."
  (apply #'zk::%fulfill-promise promise values))

(defun fail-promise (promise datum &rest arguments)
  "Fail PROMISE with a condition constructed from DATUM and ARGUMENTS.
Return true if newly settled."
  (let ((reason (cond
                  ((stringp datum)
                   (if arguments
                       (apply #'format nil datum arguments)
                       datum))
                  ((typep datum 'condition) datum)
                  ((symbolp datum) (apply #'make-condition datum arguments))
                  (t (error 'type-error
                            :datum datum
                            :expected-type '(or string symbol condition))))))
    (zk::%fail-promise promise reason)))

(defun await-promise (promise)
  "Wait until PROMISE resolves, returning its values or signaling its failure."
  (zk::%await-promise promise))

(defun make-event (&key signaled)
  "Create a resettable async event, initially signaled if SIGNALED is true."
  (zk::%make-event signaled))

(defun eventp (object)
  "Return true if OBJECT is an async event."
  (zk::%eventp object))

(defun event-set-p (event)
  "Return true if EVENT is currently signaled."
  (zk::%event-set-p event))

(defun set-event (event)
  "Signal EVENT and wake all waiting tasks. Return true if the state changed."
  (zk::%set-event event))

(defun reset-event (event)
  "Reset EVENT to unsignaled. Return true if it was previously signaled."
  (zk::%reset-event event))

(defun wait-event (event)
  "Wait until EVENT is signaled, returning T."
  (zk::%wait-event event))

(defun make-semaphore (&optional (permits 1))
  "Create a FIFO counting semaphore initialized with PERMITS non-negative permits."
  (zk::%make-semaphore permits))

(defun semaphorep (object)
  "Return true if OBJECT is an async semaphore."
  (zk::%semaphorep object))

(defun semaphore-count (semaphore)
  "Return the number of currently available permits in SEMAPHORE."
  (zk::%semaphore-count semaphore))

(defun acquire-semaphore (semaphore)
  "Acquire one permit from SEMAPHORE, waiting in FIFO order if zero permits are
available. Return T."
  (zk::%acquire-semaphore semaphore))

(defun try-acquire-semaphore (semaphore)
  "Try to acquire one permit from SEMAPHORE without waiting. Return T if
acquired or NIL if none are available."
  (zk::%try-acquire-semaphore semaphore))

(defun release-semaphore (semaphore)
  "Release one permit back to SEMAPHORE, waking the oldest waiting acquirer if
any. Return the resulting available permit count."
  (zk::%release-semaphore semaphore))

(defmacro with-semaphore ((semaphore) &body body)
  "Acquire SEMAPHORE around BODY, releasing the acquired permit on exit or unwind."
  (let ((sem-var (gensym "SEMAPHORE"))
        (acquired (gensym "ACQUIRED")))
    `(let ((,sem-var ,semaphore)
           (,acquired (cons nil nil)))
       (unwind-protect
           (progn
             (zk::%acquire-semaphore ,sem-var ,acquired)
             ,@body)
         (when (car ,acquired)
           (release-semaphore ,sem-var))))))

(defun make-channel (&key capacity)
  "Create a FIFO async channel. CAPACITY may be NIL (unbounded), 0 (rendezvous),
or a positive integer."
  (zk::%make-channel capacity))

(defun channelp (object)
  "Return true if OBJECT is an async channel."
  (zk::%channelp object))

(defun channel-closed-p (channel)
  "Return true if CHANNEL has been closed."
  (zk::%channel-closed-p channel))

(defun close-channel (channel)
  "Close CHANNEL and wake any waiting senders or receivers. Return true if newly closed."
  (zk::%close-channel channel))

(defun channel-send (channel value)
  "Send VALUE to CHANNEL, waiting if bounded or unbuffered capacity is full.
Signals ASYNC-ERROR if CHANNEL is closed. A transfer starts with the oldest
waiting receiver. Cancelling a wait before selection withdraws it."
  (zk::%channel-send channel value))

(defun channel-recv (channel)
  "Receive a value from CHANNEL, waiting if needed. Returns (VALUES value T),
or (VALUES NIL NIL) once CHANNEL is closed and empty. A value handed to a
waiting receiver is reserved for it. A later cancellation leaves the handoff
committed."
  (zk::%channel-recv channel))

(defun channel-try-send (channel value)
  "Try to send VALUE to CHANNEL without waiting. Returns true if sent, NIL if full.
Signals ASYNC-ERROR if CHANNEL is closed."
  (zk::%channel-try-send channel value))

(defun channel-try-recv (channel)
  "Try to receive a value from CHANNEL without waiting. Returns (VALUES value T)
or (VALUES NIL NIL)."
  (zk::%channel-try-recv channel))

(defun read-file-octets (pathname &key (start 0) end)
  "Read a regular file on a native worker and return a vector of octets.
START and END are byte offsets in a half-open range. NIL END reads through EOF.
Cancellation waits for the worker to release the file before unwinding Lisp."
  (zk::%read-file-octets pathname start end))

(defun %check-select-var (var clause)
  (when (and var (or (not (symbolp var)) (constantp var)))
    (error 'program-error
           :format-control "Invalid SELECT binding variable ~S in clause ~S."
           :format-arguments (list var clause))))

(defmacro select (&body clauses)
  "Wait atomically for the first ready concurrency clause and execute its body.
An optional leading (:BIASED flag) clause controls priority order (defaults to
fair rotation across ready non-timeout branches). Supported clauses are
(:RECV ch [var [ok-var]]), (:SEND ch val [var]), (:EVENT ev), (:TASK task [var]),
(:PROMISE promise [var]), (:TIMEOUT seconds), and OTHERWISE / :DEFAULT."
  (let ((biased-form nil))
    (when (and (consp clauses)
               (consp (first clauses))
               (eq (first (first clauses)) :biased))
      (let ((option (first clauses)))
        (unless (= (length option) 2)
          (error 'program-error
                 :format-control "Invalid (:BIASED ...) option ~S in SELECT."
                 :format-arguments (list option)))
        (setf biased-form (second option)
              clauses (rest clauses))))
    (when (null clauses)
      (error 'program-error :format-control "SELECT requires at least one clause."))
    (let ((bindings '())
          (specs '())
          (opcodes '())
          (flat-args '())
          (cases '())
          (default-body nil)
          (has-default nil)
          (next-index 0)
          (idx-sym (gensym "SELECT-INDEX"))
          (v1-sym (gensym "SELECT-V1"))
          (v2-sym (gensym "SELECT-V2")))
      (dolist (clause clauses)
        (unless (consp clause)
          (error 'program-error
                 :format-control "Malformed SELECT clause ~S."
                 :format-arguments (list clause)))
        (let ((head (first clause))
              (body (rest clause)))
          (cond
            ((or (eq head 'otherwise) (eq head :default)
                 (and (consp head) (eq (first head) :default) (null (rest head))))
             (when has-default
               (error 'program-error :format-control "Duplicate default clause in SELECT."))
             (setf has-default t
                   default-body body))
            ((consp head)
             (let ((op (first head))
                   (args (rest head))
                   (index next-index)
                   (local-bindings '()))
               (incf next-index)
               (case op
                 (:recv
                  (unless (<= 1 (length args) 3)
                    (error 'program-error
                           :format-control "Invalid (:RECV ...) clause ~S in SELECT."
                           :format-arguments (list clause)))
                  (let ((ch-sym (gensym "CH"))
                        (var (second args))
                        (ok-var (third args)))
                    (%check-select-var var clause)
                    (%check-select-var ok-var clause)
                    (push `(,ch-sym ,(first args)) bindings)
                    (push `(list :recv ,ch-sym) specs)
                    (push 0 opcodes)
                    (push ch-sym flat-args)
                    (when var (push `(,var ,v1-sym) local-bindings))
                    (when ok-var (push `(,ok-var ,v2-sym) local-bindings))))
                 (:send
                  (unless (<= 2 (length args) 3)
                    (error 'program-error
                           :format-control "Invalid (:SEND ...) clause ~S in SELECT."
                           :format-arguments (list clause)))
                  (let ((ch-sym (gensym "CH"))
                        (val-sym (gensym "VAL"))
                        (var (third args)))
                    (%check-select-var var clause)
                    (push `(,ch-sym ,(first args)) bindings)
                    (push `(,val-sym ,(second args)) bindings)
                    (push `(list :send ,ch-sym ,val-sym) specs)
                    (push 1 opcodes)
                    (push ch-sym flat-args)
                    (push val-sym flat-args)
                    (when var (push `(,var ,v1-sym) local-bindings))))
                 (:event
                  (unless (= (length args) 1)
                    (error 'program-error
                           :format-control "Invalid (:EVENT ...) clause ~S in SELECT."
                           :format-arguments (list clause)))
                  (let ((ev-sym (gensym "EV")))
                    (push `(,ev-sym ,(first args)) bindings)
                    (push `(list :event ,ev-sym) specs)
                    (push 2 opcodes)
                    (push ev-sym flat-args)))
                 (:task
                  (unless (<= 1 (length args) 2)
                    (error 'program-error
                           :format-control "Invalid (:TASK ...) clause ~S in SELECT."
                           :format-arguments (list clause)))
                  (let ((task-sym (gensym "TASK"))
                        (var (second args)))
                    (%check-select-var var clause)
                    (push `(,task-sym ,(first args)) bindings)
                    (push `(list :task ,task-sym) specs)
                    (push 3 opcodes)
                    (push task-sym flat-args)
                    (when var (push `(,var ,v1-sym) local-bindings))))
                 (:promise
                  (unless (<= 1 (length args) 2)
                    (error 'program-error
                           :format-control "Invalid (:PROMISE ...) clause ~S in SELECT."
                           :format-arguments (list clause)))
                  (let ((promise-sym (gensym "PROMISE"))
                        (var (second args)))
                    (%check-select-var var clause)
                    (push `(,promise-sym ,(first args)) bindings)
                    (push `(list :promise ,promise-sym) specs)
                    (push 4 opcodes)
                    (push promise-sym flat-args)
                    (when var (push `(,var ,v1-sym) local-bindings))))
                 (:timeout
                  (unless (= (length args) 1)
                    (error 'program-error
                           :format-control "Invalid (:TIMEOUT ...) clause ~S in SELECT."
                           :format-arguments (list clause)))
                  (let ((secs-sym (gensym "SECS")))
                    (push `(,secs-sym ,(first args)) bindings)
                    (push `(list :timeout ,secs-sym) specs)
                    (push 5 opcodes)
                    (push secs-sym flat-args)))
                 (t
                  (error 'program-error
                         :format-control "Unknown SELECT clause operator ~S."
                         :format-arguments (list op))))
               (push `(,index
                       ,(if local-bindings
                            `(let ,(nreverse local-bindings) ,@body)
                            `(progn ,@body)))
                     cases)))
            (t
             (error 'program-error
                    :format-control "Malformed SELECT clause ~S."
                    :format-arguments (list clause))))))
      `(let ,(nreverse bindings)
         (multiple-value-bind (,idx-sym ,v1-sym ,v2-sym)
             ,(if (<= (length flat-args) 200)
                  `(zk::%select-packed ,(apply #'vector (nreverse opcodes))
                                       ,(if has-default 't 'nil)
                                       ,biased-form
                                       ,@(nreverse flat-args))
                  `(zk::%select (list ,@(nreverse specs))
                                ,(if has-default 't 'nil)
                                ,biased-form))
           (declare (ignorable ,v1-sym ,v2-sym))
           (case ,idx-sym
             ,@(nreverse cases)
             ,@(when has-default `(((nil) ,@default-body)))))))))
