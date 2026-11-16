import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T, toArray } from '../runtime/runtime.mjs';

test('compiler macro expansion functions preserve whole form, FUNCALL syntax, and their documentation', async () => {
  const runtime = createRuntime();
  const seen = [];
  runtime.registerFunction('NOTE-COMPILER-MACRO', ([value]) => { seen.push(value); return Promise.resolve(value); });
  const result = await runtime.evalStringAsync(`
    (define-compiler-macro compiler-target
        (&whole form &optional (arg 17 supplied-p) &environment environment)
      "Compiler target doc."
      (declare (type list form))
      (values (note-compiler-macro arg) supplied-p form environment))
    (let* ((expander (compiler-macro-function 'compiler-target))
           (form '(funcall #'compiler-target 42)))
      (multiple-value-bind (arg supplied-p whole environment) (funcall expander form nil)
        (values arg supplied-p (eq whole form) environment
                (documentation expander t)
                (documentation 'compiler-target 'compiler-macro)
                (macro-function 'compiler-target))))
  `);
  assert.deepEqual(result.values.slice(0, 4), [42, T, T, NIL]);
  assert.deepEqual(result.values.slice(4, 6).map(value => value.toString()), ['Compiler target doc.', 'Compiler target doc.']);
  assert.equal(result.values[6], NIL);
  const direct = await runtime.evalStringAsync("(funcall (compiler-macro-function 'compiler-target) '(compiler-target) nil)");
  assert.deepEqual(direct.values.slice(0, 2), [17, NIL]);
  assert.deepEqual(toArray(direct.values[2]).map(value => value.name), ['COMPILER-TARGET']);
  const lexical = await runtime.evalStringAsync(`
    (defmacro preserved-compiler-environment (&environment environment)
      (eq environment (nth-value 3
        (funcall (compiler-macro-function 'compiler-target environment)
                 '(compiler-target 31) environment))))
    (preserved-compiler-environment)
  `);
  assert.deepEqual(lexical.values, [T]);
  assert.deepEqual(seen, [42, 17, 31]);
});

test('compiler macro lookup follows FLET and MACROLET scopes, including SETF names', () => {
  const runtime = createRuntime();
  const result = runtime.evalString(`
    (defun compiler-target (x) x)
    (define-compiler-macro compiler-target (&whole form x)
      (declare (ignore x))
      (error "The resident compiler must not apply this compiler macro automatically."))
    (define-compiler-macro (setf compiler-target) (new x) (list new x))
    (defmacro available (name &environment env)
      (not (null (compiler-macro-function name env))))
    (list (available compiler-target)
          (flet ((compiler-target (x) (+ x 10)))
            (list (available compiler-target) (compiler-target 1)
                  (available (setf compiler-target))))
          (macrolet ((compiler-target (x) x))
            (list (available compiler-target) (compiler-target 2)))
          (labels ((compiler-target (x) (list (available compiler-target) x)))
            (compiler-target 12))
          (flet (((setf compiler-target) (new x) (declare (ignore x)) new))
            (list (available compiler-target) (available (setf compiler-target))))
          (locally (declare (notinline compiler-target))
            (list (available compiler-target) (compiler-target 3)))
          (progn (setf (compiler-macro-function '(setf compiler-target)) nil)
                 (compiler-macro-function '(setf compiler-target)))
          (available compiler-target))
  `);
  const values = toArray(result.values[0]);
  assert.deepEqual(values.map(value => value?.car === undefined ? value : toArray(value)), [
    T, [NIL, 11, T], [NIL, 2], [NIL, 12], [T, NIL], [T, 3], NIL, T,
  ]);
  const caller = runtime.compile(runtime.read('(lambda (x) (declare (notinline compiler-target)) (compiler-target x))'));
  assert.deepEqual(runtime.call(caller, [23]).values, [23]);
  assert.throws(() => runtime.evalString('(compiler-macro-function 7)'), error => error.className === 'TYPE-ERROR');
  assert.throws(() => runtime.evalString("(setf (compiler-macro-function 'compiler-target) 'identity)"), error => error.className === 'TYPE-ERROR');
});

test('a top-level compiler macro is installed for subsequent compile-time forms', () => {
  const runtime = createRuntime();
  const compiled = runtime.compileString(`
    (define-compiler-macro compiler-phase (x) (list 'quote x))
    (eval-when (:compile-toplevel)
      (defparameter *compiler-phase-expansion*
        (funcall (compiler-macro-function 'compiler-phase) '(compiler-phase 42) nil)))
    *compiler-phase-expansion*
  `);
  assert.deepEqual(toArray(runtime.call(compiled).values[0]).map(value => value.name ?? value), ['QUOTE', 42]);
});
