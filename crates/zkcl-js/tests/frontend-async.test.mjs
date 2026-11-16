import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, LispCharacter, NIL, T, SuspensionError, toArray } from '../runtime/runtime.mjs';

function deferred() {
  let resolve;
  const promise = new Promise(yes => { resolve = yes; });
  return { promise, resolve };
}

test('recursive reader macros and read-time evaluation resume their own cursor and bindings', async () => {
  const runtime = createRuntime(), waiting = new Map(), seen = [];
  runtime.registerFunction('READER-GATE', ([radix]) => {
    seen.push(radix);
    if (!waiting.has(radix)) waiting.set(radix, deferred());
    return waiting.get(radix).promise;
  });
  runtime.registerFunction('READER-VALUE', ([value]) => Promise.resolve(value));
  runtime.evalString(`
    (defun read-suspended (radix)
      (let ((*read-base* radix) (*readtable* (copy-readtable nil)))
        (set-macro-character #\\! (lambda (stream ch)
          (declare (ignore ch))
          (reader-gate *read-base*)
          (read stream t nil t)))
        (read-from-string "(!10 #.(reader-value 7) !11) tail")))
  `);

  const hex = runtime.callAsync('READ-SUSPENDED', [16]);
  const octal = runtime.callAsync('READ-SUSPENDED', [8]);
  assert.deepEqual(seen, [16, 8]);
  assert.equal(runtime.readVariable(runtime.intern('*READ-BASE*', 'COMMON-LISP')), 10);
  waiting.get(8).resolve(NIL);
  const [octalList, octalPosition] = (await octal).values;
  const position = '(!10 #.(reader-value 7) !11) '.length;
  assert.deepEqual([toArray(octalList), octalPosition], [[8, 7, 9], position]);
  waiting.get(16).resolve(NIL);
  const [hexList, hexPosition] = (await hex).values;
  assert.deepEqual([toArray(hexList), hexPosition], [[16, 7, 17], position]);
  assert.deepEqual(seen, [16, 8, 8, 16]);
});

test('a synchronous reader can hand its suspended cursor to the async scheduler', async () => {
  const runtime = createRuntime(), wait = deferred();
  runtime.registerFunction('CURSOR-GATE', () => wait.promise);
  runtime.evalString(`(set-macro-character #\\! (lambda (stream ch)
    (declare (ignore ch))
    (cursor-gate)
    (read-char stream)))`);
  let task;
  assert.throws(() => runtime.readFromString('!λ next'), error => {
    if (!(error instanceof SuspensionError)) return false;
    task = error.task;
    return true;
  });
  wait.resolve(NIL);
  const [character, position] = (await task.runAsync()).values;
  assert.ok(character instanceof LispCharacter);
  assert.equal(character.toString(), 'λ');
  assert.equal(position, 3);
});

test('Lisp COMPILE and MACROEXPAND honor a suspending macro and dynamic expansion hook', async () => {
  const runtime = createRuntime(), wait = deferred(), seen = [];
  runtime.registerFunction('EXPANSION-GATE', ([tag, value]) => {
    seen.push([tag.name, value]);
    return tag.name === 'MACRO' ? wait.promise : Promise.resolve(NIL);
  });
  runtime.evalString(`
    (defvar *expansion-value* 0)
    (defmacro async-add (x)
      (expansion-gate :macro *expansion-value*)
      \`(+ ,x ,*expansion-value*))
    (defun compile-under-hook (value)
      (let ((*expansion-value* value)
            (*macroexpand-hook*
              (lambda (expander whole environment)
                (expansion-gate :hook *expansion-value*)
                (funcall expander whole environment))))
        (compile nil '(lambda (&optional (x (async-add 10))) x))))
  `);
  const compiled = runtime.callAsync('COMPILE-UNDER-HOOK', [32]);
  assert.deepEqual(seen, [['HOOK', 32]]);
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(seen, [['HOOK', 32], ['MACRO', 32]]);
  assert.equal(runtime.evalString('*expansion-value*').values[0], 0);
  wait.resolve(NIL);
  const [fn, warnings, failure] = (await compiled).values;
  assert.deepEqual([warnings, failure], [NIL, NIL]);
  assert.equal(typeof fn.code.entry.step, 'function');
  assert.deepEqual(runtime.call(fn).values, [42]);
  assert.deepEqual(seen, [['HOOK', 32], ['MACRO', 32]]);

  const [expansion, expanded] = (await runtime.evalStringAsync("(macroexpand '(async-add 10))")).values;
  assert.equal(expanded, T);
  assert.deepEqual(toArray(expansion).map(value => value.name ?? value), ['+', 10, 0]);
  assert.deepEqual(seen.at(-1), ['MACRO', 0]);
});

test('compile-time calls preserve file bindings and do not repeat forms after suspension', async () => {
  const runtime = createRuntime(), wait = deferred(), seen = [];
  runtime.registerFunction('COMPILER-GATE', ([tag], rt, task) => {
    seen.push([tag.name, rt.readVariable(rt.intern('*PACKAGE*', 'COMMON-LISP'), task).name]);
    return wait.promise;
  });
  const original = runtime.currentPackage;
  const compiled = runtime.compileStringAsync(`
    (defpackage #:async-file (:use #:cl))
    (in-package #:async-file)
    (eval-when (:compile-toplevel) (compiler-gate :compile))
    (defmacro phase-package () (package-name *package*))
    (values (phase-package) (load-time-value (values (package-name *package*) :ignored)))
  `);
  assert.deepEqual(seen, [['COMPILE', 'ASYNC-FILE']]);
  assert.equal(runtime.currentPackage, original);
  assert.equal(runtime.read('identity-check').package, original);
  wait.resolve(NIL);
  const loaded = await compiled;
  assert.equal(runtime.currentPackage, original);
  const sourcePackage = runtime.intern('*PACKAGE*', 'COMMON-LISP');
  const invoke = runtime.registerFunction('CALL-IN-FILE-CONTEXT', function* (_, rt, task) {
    task.bind(task.currentFrame, [[sourcePackage, original]]);
    return yield rt.requestCall(loaded);
  });
  assert.deepEqual(runtime.call(invoke).values.map(value => value.toString()), ['ASYNC-FILE', 'ASYNC-FILE']);
  assert.deepEqual(seen, [['COMPILE', 'ASYNC-FILE']]);
  assert.equal(runtime.currentPackage, original);
});

test('custom SETF and type expanders can suspend through their ordinary Lisp entry points', async () => {
  const runtime = createRuntime(), seen = [];
  runtime.registerFunction('FRONTEND-NOTE', ([tag]) => {
    seen.push(tag.name);
    return Promise.resolve(NIL);
  });
  const result = await runtime.evalStringAsync(`
    (define-setf-expander first-slot (x)
      (frontend-note :setf)
      (get-setf-expansion (list 'car x)))
    (defmacro first-place (x)
      (frontend-note :place)
      (list 'first-slot x))
    (deftype async-integer () (frontend-note :type) 'integer)
    (let ((box (list 2)))
      (incf (first-place box) 3)
      (values (car box) (typep box '(cons async-integer null))
              (subtypep 'async-integer 'integer)))
  `);
  assert.deepEqual(result.values, [5, T, T]);
  assert.deepEqual(seen, ['PLACE', 'SETF', 'TYPE', 'TYPE']);
});

test('VALUES places take one store value per member and bind nested extra stores to NIL', () => {
  const runtime = createRuntime();
  const result = runtime.evalString(`
    (let ((a :old) (b :old) (c :old) (order nil))
      (multiple-value-bind (first second third)
          (setf (values (car (progn (push :place order) (list a)))
                       (values b c))
                (progn (push :value order) (values 1 2 3)))
        (values first second third b c (reverse order))))
  `);
  assert.deepEqual(result.values.slice(0, 5), [1, 2, NIL, 2, NIL]);
  assert.deepEqual(toArray(result.values[5]).map(value => value.name), ['PLACE', 'VALUE']);
  assert.deepEqual(runtime.evalString('(setf (values) (values 1 2))').values, []);
});
