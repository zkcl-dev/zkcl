import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T, MultipleValues, LispCondition } from '../runtime/runtime.mjs';

const evaluate = (runtime, source) => runtime.evalString(source).values;
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};

test('independent runtimes own the mutable properties of canonical symbols', () => {
  const left = createRuntime(), right = createRuntime();
  for (const spelling of ['nil', 't']) {
    assert.deepEqual(evaluate(left, `(setf (get ${spelling} :architecture-property) 17)`), [17]);
    assert.deepEqual(evaluate(left, `(get ${spelling} :architecture-property)`), [17]);
    assert.deepEqual(evaluate(right, `(symbol-plist ${spelling})`), [NIL]);
    assert.deepEqual(evaluate(right, `(get ${spelling} :architecture-property)`), [NIL]);
    assert.deepEqual(evaluate(right, `(setf (symbol-plist ${spelling}) (list :architecture-property 29)) (get ${spelling} :architecture-property)`), [29]);
    assert.deepEqual(evaluate(left, `(get ${spelling} :architecture-property)`), [17]);
    assert.deepEqual(evaluate(left, `(remprop ${spelling} :architecture-property) (get ${spelling} :architecture-property)`), [NIL]);
    assert.deepEqual(evaluate(right, `(get ${spelling} :architecture-property)`), [29]);
    assert.deepEqual(evaluate(left, `(eq (symbol-package ${spelling}) (find-package "COMMON-LISP"))`), [T]);
  }
});

test('function replacement changes the binding while retaining saved function identity', () => {
  const runtime = createRuntime();
  evaluate(runtime, '(defun architecture-replaced () 1) (defparameter *architecture-saved* #\'architecture-replaced)');
  evaluate(runtime, '(defmacro architecture-replaced () 2)');
  assert.deepEqual(evaluate(runtime, '(values (architecture-replaced) (funcall *architecture-saved*))'), [2, 1]);
  assert.throws(() => evaluate(runtime, '(funcall \'architecture-replaced)'), error => error.className === 'UNDEFINED-FUNCTION');
  evaluate(runtime, '(defun architecture-replaced () 3)');
  assert.deepEqual(evaluate(runtime, '(values (architecture-replaced) (funcall *architecture-saved*) (macro-function \'architecture-replaced))'), [3, 1, NIL]);
});

test('file compilation restores the visible package after success and failure', () => {
  const runtime = createRuntime(), original = runtime.currentPackage;
  runtime.compileString('(defpackage #:architecture-phase (:use #:cl)) (in-package #:architecture-phase) (defun answer () 42)');
  assert.equal(runtime.currentPackage, original);
  assert.equal(runtime.readVariable(runtime.intern('*PACKAGE*', 'COMMON-LISP')), original);
  assert.deepEqual(evaluate(runtime, '(eq *package* (find-package "CL-USER"))'), [T]);
  assert.throws(() => runtime.compileString('(in-package #:architecture-phase) (if 1)'));
  assert.equal(runtime.currentPackage, original);
  assert.equal(runtime.readVariable(runtime.intern('*PACKAGE*', 'COMMON-LISP')), original);
});

test('compiled-file loading shares reader state across entries and restores its caller', () => {
  const runtime = createRuntime(), originalPackage = runtime.currentPackage;
  const packageSymbol = runtime.intern('*PACKAGE*', 'COMMON-LISP'), readtableSymbol = runtime.intern('*READTABLE*', 'COMMON-LISP');
  const originalReadtable = runtime.readVariable(readtableSymbol), loadPackage = runtime.makePackage('ARCHITECTURE-LOAD');
  const loadReadtable = evaluate(runtime, '(copy-readtable nil)')[0];
  runtime.defineBuiltin('ARCHITECTURE-LOAD-SWITCH', (_args, rt, task) => {
    rt.writeVariable(packageSymbol, loadPackage, task);
    rt.writeVariable(readtableSymbol, loadReadtable, task);
    return NIL;
  });
  runtime.defineBuiltin('ARCHITECTURE-LOAD-OBSERVE', (_args, rt, task) => rt.multipleValues(rt.readVariable(packageSymbol, task), rt.readVariable(readtableSymbol, task)));
  runtime.defineBuiltin('ARCHITECTURE-LOAD-FAIL', () => { throw new LispCondition('PROGRAM-ERROR', 'load failure'); });
  const entry = index => ({
    registers: 1, constants: [index], arity: { required: 0, maximum: 0 },
    instructions: [{ Constant: [0, 0] }, { Call: { callee: { Value: 0 }, out: null, args: { start: 0, end: 0 }, at: 0 } }],
  });
  const program = { target: 'js', nodes: [{ Primitive: 'ARCHITECTURE-LOAD-SWITCH' }, { Primitive: 'ARCHITECTURE-LOAD-OBSERVE' }, { Primitive: 'ARCHITECTURE-LOAD-FAIL' }], entries: [entry(0), entry(1)] };
  assert.deepEqual(runtime.load(program).values, [loadPackage, loadReadtable]);
  assert.equal(runtime.currentPackage, originalPackage);
  assert.equal(runtime.readVariable(packageSymbol), originalPackage);
  assert.equal(runtime.readVariable(readtableSymbol), originalReadtable);
  assert.throws(() => runtime.load({ ...program, entries: [entry(0), entry(2)] }), error => error.className === 'PROGRAM-ERROR');
  assert.equal(runtime.currentPackage, originalPackage);
  assert.equal(runtime.readVariable(packageSymbol), originalPackage);
  assert.equal(runtime.readVariable(readtableSymbol), originalReadtable);
});

test('cancelled asynchronous loading releases reader bindings and never starts a later entry', async () => {
  const runtime = createRuntime(), wait = deferred(), started = deferred(), seen = [];
  const packageSymbol = runtime.intern('*PACKAGE*', 'COMMON-LISP'), readtableSymbol = runtime.intern('*READTABLE*', 'COMMON-LISP');
  const originalPackage = runtime.currentPackage, originalReadtable = runtime.readVariable(readtableSymbol);
  const loadPackage = runtime.makePackage('ARCHITECTURE-ASYNC-LOAD'), loadReadtable = evaluate(runtime, '(copy-readtable nil)')[0];
  runtime.defineBuiltin('ARCHITECTURE-LOAD-WAIT', (_args, rt, task) => {
    rt.writeVariable(packageSymbol, loadPackage, task);
    rt.writeVariable(readtableSymbol, loadReadtable, task);
    started.resolve(); return wait.promise;
  });
  runtime.defineBuiltin('ARCHITECTURE-LOAD-LATER', () => { seen.push('later entry'); return NIL; });
  const entry = index => ({ registers: 1, constants: [index], arity: { required: 0, maximum: 0 }, instructions: [{ Constant: [0, 0] }, { Call: { callee: { Value: 0 }, out: null, args: { start: 0, end: 0 }, at: 0 } }] });
  const controller = new AbortController(), reason = new Error('cancel loading');
  const completion = runtime.loadAsync({ target: 'js', nodes: [{ Primitive: 'ARCHITECTURE-LOAD-WAIT' }, { Primitive: 'ARCHITECTURE-LOAD-LATER' }], entries: [entry(0), entry(1)] }, { signal: controller.signal });
  await started.promise;
  assert.equal(runtime.currentPackage, originalPackage);
  assert.equal(runtime.readVariable(packageSymbol), originalPackage);
  assert.equal(runtime.readVariable(readtableSymbol), originalReadtable);
  controller.abort(reason);
  await assert.rejects(completion, error => error === reason);
  wait.resolve(NIL);
  await Promise.resolve(); await Promise.resolve();
  assert.deepEqual(seen, []);
  assert.equal(runtime.currentPackage, originalPackage);
  assert.equal(runtime.readVariable(packageSymbol), originalPackage);
  assert.equal(runtime.readVariable(readtableSymbol), originalReadtable);
});

test('load-time values use the primary value and initialize in the load environment', () => {
  const runtime = createRuntime();
  assert.deepEqual(evaluate(runtime, '(+ 1 (load-time-value (values 41 99)))'), [42]);
  evaluate(runtime, '(defparameter *architecture-load-count* 0)');
  const compiled = runtime.compileString(`
    (defun architecture-loaded ()
      (load-time-value (incf *architecture-load-count*)))`);
  assert.deepEqual(evaluate(runtime, '*architecture-load-count*'), [0]);
  runtime.call(compiled);
  assert.deepEqual(evaluate(runtime, '(values (architecture-loaded) (architecture-loaded) *architecture-load-count*)'), [1, 1, 1]);
  runtime.call(compiled);
  assert.deepEqual(evaluate(runtime, '(values (architecture-loaded) *architecture-load-count*)'), [2, 2]);
});

test('concurrent suspended tasks retain their own dynamic bindings during host reentry', async () => {
  const runtime = createRuntime(), waits = new Map([[1, deferred()], [2, deferred()]]);
  runtime.defineBuiltin('ARCHITECTURE-WAIT', args => waits.get(args[0]).promise);
  runtime.defineBuiltin('ARCHITECTURE-REENTER', (_args, rt, task) => rt.call('ARCHITECTURE-READ-SCOPE', [], task));
  evaluate(runtime, `
    (defparameter *architecture-scope* 0)
    (defun architecture-read-scope () *architecture-scope*)
    (defun architecture-task (value)
      (let ((*architecture-scope* value))
        (multiple-value-call #'values
          (architecture-wait value)
          (architecture-reenter))))`);
  const first = runtime.createTask('ARCHITECTURE-TASK', [1]), second = runtime.createTask('ARCHITECTURE-TASK', [2]);
  first.run(); second.run();
  assert.ok(first.suspended); assert.ok(second.suspended);
  assert.deepEqual(evaluate(runtime, '*architecture-scope*'), [0]);
  waits.get(2).resolve(new MultipleValues([20, 21]));
  assert.deepEqual((await second.runAsync()).values, [20, 21, 2]);
  assert.ok(first.suspended);
  waits.get(1).resolve(new MultipleValues([10, 11]));
  assert.deepEqual((await first.runAsync()).values, [10, 11, 1]);
  assert.equal(first.dynamic.size, 0); assert.equal(second.dynamic.size, 0);
});

test('cancellation can suspend in cleanup and ignores an abandoned host rejection', async () => {
  const runtime = createRuntime(), body = deferred(), cleanup = deferred(), cleanupStarted = deferred(), seen = [];
  runtime.defineBuiltin('ARCHITECTURE-BODY-WAIT', () => body.promise);
  runtime.defineBuiltin('ARCHITECTURE-CLEANUP-WAIT', args => { seen.push(args[0]); cleanupStarted.resolve(); return cleanup.promise; });
  evaluate(runtime, `
    (defparameter *architecture-cancel-scope* 0)
    (defun architecture-cancel-task ()
      (let ((*architecture-cancel-scope* 7))
        (unwind-protect (architecture-body-wait)
          (architecture-cleanup-wait *architecture-cancel-scope*))))`);
  const task = runtime.createTask('ARCHITECTURE-CANCEL-TASK'); task.run();
  const controller = new AbortController(), reason = new Error('architecture cancellation');
  const completion = task.runAsync({ signal: controller.signal });
  controller.abort(reason);
  await cleanupStarted.promise;
  assert.deepEqual(seen, [7]);
  assert.ok(task.suspended); assert.equal(task.done, false);
  body.reject(new Error('abandoned body result'));
  await Promise.resolve(); await Promise.resolve();
  assert.equal(task.done, false);
  cleanup.resolve(NIL);
  await assert.rejects(completion, error => error === reason);
  assert.equal(task.error, reason); assert.equal(task.dynamic.size, 0);
  assert.deepEqual(seen, [7]);
  assert.deepEqual(evaluate(runtime, '*architecture-cancel-scope*'), [0]);
});
