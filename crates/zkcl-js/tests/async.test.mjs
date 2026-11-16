import assert from 'node:assert/strict';
import test from 'node:test';
import { NIL, T, createRuntime } from '../runtime/runtime.mjs';

test('Lisp task groups capture dynamic bindings and await all child values', async () => {
  const rt = createRuntime();
  rt.evalString('(defparameter *task-value* 0)');
  const run = rt.compileString(`
    (let ((*task-value* 42))
      (zk-async:with-task-group (group)
        (let ((task (zk-async:spawn-task (group)
                      (zk-async:yield) (values *task-value* 2))))
          (setq *task-value* 9)
          (zk-async:await task))))`);
  assert.deepEqual((await rt.callAsync(run, [], { signal: AbortSignal.timeout(5000) })).values, [42, 2]);
  assert.deepEqual(rt.evalString('*task-value*').values, [0]);
});

test('Lisp cancellation runs cleanup and the owner observes unhandled child conditions', async () => {
  const rt = createRuntime();
  const result = await rt.evalStringAsync(`
    (let ((event (zk-async:make-event)) (cleaned nil))
      (values (zk-async:with-cancel-scope (scope :on-cancel :cancelled)
                (unwind-protect (progn (zk-async:cancel-scope scope) (zk-async:wait-event event))
                  (setf cleaned t)))
              cleaned (zk-async:event-set-p event)))`, { signal: AbortSignal.timeout(5000) });
  assert.equal(result.values[0].name, 'CANCELLED');
  assert.deepEqual(result.values.slice(1), [T, NIL]);
  const failure = await rt.evalStringAsync(`
    (handler-case (zk-async:with-task-group (group)
                    (zk-async:spawn-task (group) (error "Child failure"))
                    (zk-async:yield) :unexpected)
      (simple-error (condition) (simple-condition-format-control condition)))`, { signal: AbortSignal.timeout(5000) });
  assert.equal(failure.values[0].toString(), 'Child failure');
});

test('normal group exit cancels no-wait children and joins nested cleanup', async () => {
  const rt = createRuntime();
  const result = await rt.evalStringAsync(`
    (let ((owner (zk-async:current-task))
          (started (zk-async:make-event))
          (released (zk-async:make-event))
          (cleanups nil))
      (multiple-value-bind (first second)
          (zk-async:with-task-group (outer)
            (zk-async:spawn-task (outer :no-wait t)
              (unwind-protect
                   (zk-async:with-task-group (inner)
                     (zk-async:spawn-task (inner)
                       (unwind-protect
                            (progn (zk-async:set-event started) (zk-async:await owner))
                         (push :inner cleanups)
                         (zk-async:set-event released))))
                (push :outer cleanups)))
            (zk-async:spawn-task (outer)
              (zk-async:wait-event released)
              (push :joined cleanups))
            (zk-async:wait-event started)
            (assert (null cleanups))
            (values 6 7))
        (values first second (member :inner cleanups) (member :outer cleanups)
                (member :joined cleanups))))`, { signal: AbortSignal.timeout(5000) });
  assert.deepEqual(result.values.slice(0, 2), [6, 7]);
  assert.ok(result.values.slice(2).every(value => value !== NIL));
});

test('group join retains dependencies through no-wait cancellation shields', async () => {
  const rt = createRuntime();
  const result = await rt.evalStringAsync(`
    (let ((owner (zk-async:current-task)) (caught nil)
          (started (zk-async:make-event)) (proceed (zk-async:make-event)))
      (zk-async:with-task-group (outer)
        (zk-async:spawn-task (outer :no-wait t)
          (zk-async:protect-from-cancel
            (zk-async:set-event started)
            (zk-async:wait-event proceed)
            (zk-async:with-task-group (inner)
              (zk-async:spawn-task (inner)
                (handler-case (zk-async:await owner)
                  (zk-async:async-error () (setq caught t)))))))
        (zk-async:wait-event started)
        (zk-async:set-event proceed))
      caught)`, { signal: AbortSignal.timeout(5000) });
  assert.deepEqual(result.values, [T]);
});

test('a child condition from nested cleanup supersedes a transfer past its groups', async () => {
  const rt = createRuntime();
  const result = await rt.evalStringAsync(`
    (let ((started (zk-async:make-event)) (never (zk-async:make-event)) (cleaned nil))
      (values
        (handler-case
            (block out
              (zk-async:with-task-group (outer)
                (zk-async:spawn-task (outer)
                  (unwind-protect
                       (zk-async:with-task-group (inner)
                         (zk-async:spawn-task (inner)
                           (unwind-protect
                                (progn (zk-async:set-event started) (zk-async:wait-event never))
                             (error "Nested cleanup failure"))))
                    (setq cleaned t)))
                (zk-async:wait-event started)
                (return-from out :escaped)))
          (simple-error (condition) (simple-condition-format-control condition)))
        cleaned))`, { signal: AbortSignal.timeout(5000) });
  assert.equal(result.values[0].toString(), 'Nested cleanup failure');
  assert.equal(result.values[1], T);
});

test('cancelling a parked task removes its dependency before the cancelling task awaits it', async () => {
  const rt = createRuntime();
  const result = await rt.evalStringAsync(`
    (flet ((exercise (use-select)
             (let ((started (zk-async:make-event)) (waiter nil) (canceller nil) (cleaned nil))
               (zk-async:with-task-group (group)
                 (setq waiter
                       (zk-async:spawn-task (group)
                         (unwind-protect
                              (progn (zk-async:set-event started)
                                     (if use-select
                                         (zk-async:select ((:task canceller) :unexpected))
                                         (zk-async:await-any (list canceller))))
                           (setq cleaned t))))
                 (setq canceller
                       (zk-async:spawn-task (group)
                         (zk-async:wait-event started)
                         (zk-async:cancel-task waiter)
                         (handler-case (zk-async:await waiter)
                           (zk-async:waited-task-cancelled () cleaned))))
                 (zk-async:await canceller)))))
      (values (exercise nil) (exercise t)))`, { signal: AbortSignal.timeout(5000) });
  assert.deepEqual(result.values, [T, T]);
});

test('channels preserve FIFO transfers, distinguish NIL from EOF and withdraw cancelled receivers', async () => {
  const rt = createRuntime();
  const run = rt.compileString(`
    (let ((channel (zk-async:make-channel :capacity 1))
          (rendezvous (zk-async:make-channel :capacity 0)))
      (zk-async:channel-send channel 11)
      (zk-async:with-task-group (group)
        (let ((sender (zk-async:spawn-task (group) (zk-async:channel-send channel nil)))
              (abandoned (zk-async:spawn-task (group) (zk-async:channel-recv rendezvous))))
          (zk-async:yield)
          (zk-async:cancel-task abandoned)
          (let* ((withdrawn (not (zk-async:channel-try-send rendezvous 99)))
                 (first (zk-async:channel-recv channel)))
            (zk-async:await sender)
            (zk-async:close-channel channel)
            (multiple-value-bind (item ok) (zk-async:channel-recv channel)
              (multiple-value-bind (eof open) (zk-async:channel-recv channel)
                (values first item ok eof open withdrawn
                        (handler-case (zk-async:channel-try-send channel 3)
                          (zk-async:async-error () t)))))))))`);
  assert.deepEqual((await rt.callAsync(run, [], { signal: AbortSignal.timeout(5000) })).values,
    [11, NIL, T, NIL, NIL, T, T]);
});

test('a cancelled semaphore handoff returns its permit and WITH-SEMAPHORE preserves values', async () => {
  const rt = createRuntime();
  const result = await rt.evalStringAsync(`
    (let ((sem (zk-async:make-semaphore 0)) (entered nil))
      (let ((count (zk-async:with-task-group (group)
                     (let ((child (zk-async:spawn-task (group)
                                    (zk-async:with-semaphore (sem) (setq entered t)))))
                       (zk-async:yield)
                       (prog1 (zk-async:release-semaphore sem) (zk-async:cancel-task child))))))
        (multiple-value-bind (a b) (zk-async:with-semaphore (sem) (values 6 7))
          (values count entered a b (zk-async:semaphore-count sem)))))`, { signal: AbortSignal.timeout(5000) });
  assert.deepEqual(result.values, [0, NIL, 6, 7, 1]);
});

test('SELECT commits one branch, preserves its bindings and rotates ready branches', async () => {
  const rt = createRuntime();
  const result = await rt.evalStringAsync(`
    (let ((channel (zk-async:make-channel :capacity 0))
          (event (zk-async:make-event))
          (done (zk-async:make-promise))
          (resolutions 0))
      (zk-async:with-task-group (group)
        (let ((child (zk-async:spawn-task (group)
                       (zk-async:select
                         (:biased t)
                         ((:recv channel item ok) (incf resolutions) (values item ok))
                         ((:event event) (incf resolutions) (values :event nil))))))
          (zk-async:yield)
          (let ((sent (zk-async:select ((:send channel 42 item) item) (otherwise nil))))
            (zk-async:set-event event)
            (zk-async:fulfill-promise done 9)
            (multiple-value-bind (item ok) (zk-async:await child)
              (flet ((choose () (zk-async:select ((:event event) :event) ((:promise done) :promise))))
                (let ((first (choose)) (second (choose)))
                  (values sent item ok resolutions (not (eq first second))
                    (zk-async:select (:biased t) ((:promise done p) (eq p done)) ((:task child) nil))
                    (zk-async:select ((:task child task) (eq task child)))
                    (zk-async:select ((:recv channel) :unexpected) (otherwise :default))
                    (zk-async:select ((:timeout 0.005) :timeout))))))))))`, { signal: AbortSignal.timeout(5000) });
  assert.deepEqual(result.values.map(value => value?.name ?? value),
    [42, 42, 'T', 1, 'T', 'T', 'T', 'DEFAULT', 'TIMEOUT']);
});

test('READ-FILE-OCTETS uses binary ranges and awaits handle release before cancelling', async () => {
  const bytes = Uint8Array.of(0, 13, 128, 255), operations = [];
  let beginRead, releaseRead;
  const begun = new Promise(resolve => { beginRead = resolve; });
  const pending = new Promise(resolve => { releaseRead = resolve; });
  const rt = createRuntime({ pathnameHost: {
    openFile(pathname, _context, options) {
      assert.equal(options.elementType, 'octet');
      return {
        readAt(start, length) {
          operations.push([pathname, start, length]);
          if (pathname === '/wait.bin') { beginRead(); return pending; }
          return bytes.slice(start, length === null ? undefined : start + length);
        },
        async close() { operations.push(['close', pathname]); },
      };
    },
  } });
  const [vector] = (await rt.evalStringAsync('(zk-async:read-file-octets "/file.bin" :start 1 :end 3)')).values;
  assert.equal(vector.elementKind, 'octet');
  assert.deepEqual(vector.storage, [13, 128]);
  const controller = new AbortController();
  const reason = new Error('cancel reading');
  const cancelled = assert.rejects(rt.evalStringAsync('(zk-async:read-file-octets "/wait.bin")', { signal: controller.signal }), error => error === reason);
  await begun;
  controller.abort(reason);
  assert.deepEqual(operations, [['/file.bin', 1, 2], ['close', '/file.bin'], ['/wait.bin', 0, null]]);
  releaseRead(bytes);
  await cancelled;
  assert.deepEqual(operations.at(-1), ['close', '/wait.bin']);
});
