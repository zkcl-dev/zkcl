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
