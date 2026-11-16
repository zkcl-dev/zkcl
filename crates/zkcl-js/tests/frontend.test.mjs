import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T, Cons, LispString, LispArray, toArray } from '../runtime/runtime.mjs';
import { ReaderError } from '../runtime/reader.mjs';
import { compileForm } from '../runtime/compiler.mjs';

const evaluate = (rt, source) => rt.evalString(source).values;

test('ordinary lambda defaults, supplied flags, rest and keyword identity', () => {
  const rt = createRuntime();
  const [fn] = evaluate(rt, '(lambda (x &optional (y (+ x 1) yp) &rest tail &key (z y zp) &allow-other-keys &aux (a (+ z 1))) (values x y yp tail z zp a))');
  const first = rt.call(fn, [4]).values;
  assert.deepEqual(first.slice(0, 3), [4, 5, NIL]);
  assert.equal(first[3], NIL); assert.deepEqual(first.slice(4), [5, NIL, 6]);
  const key = rt.intern('Z', 'KEYWORD'), unknown = rt.intern('OTHER', 'KEYWORD');
  const second = rt.call(fn, [4, 8, key, 11, unknown, 13]).values;
  assert.deepEqual(second.slice(0, 3), [4, 8, T]);
  assert.deepEqual(toArray(second[3]), [key, 11, unknown, 13]); assert.deepEqual(second.slice(4), [11, T, 12]);
  assert.throws(() => evaluate(rt, '((lambda (&key x) x) :missing 1)'), /keyword/i);
  for (const source of ['(lambda (x x) x)', '(lambda (&rest) nil)', '(lambda (&optional x &optional y) nil)', '(lambda (nil) nil)', '(lambda (&key x y &rest z) nil)']) assert.throws(() => evaluate(rt, source));
});

test('aux variables shadow earlier bindings and preserve parameter traces', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(funcall (lambda (x &aux (original (lambda () x)) (x (1+ x)) (x (* x 2))) (values (funcall original) x)) 10)'), [10, 22]);
  assert.deepEqual(evaluate(rt, '(funcall (lambda (&key ((:answer x)) &aux (x x)) x) :answer 42)'), [42]);
  assert.deepEqual(evaluate(rt, '(destructuring-bind ((x &aux (x (1+ x))) y) \'((10) 8) (values x y))'), [11, 8]);
  assert.deepEqual(evaluate(rt, '(macrolet ((m ((x &aux (x (1+ x))) y) (list \'quote (list x y)))) (m (40) 2))').map(toArray), [[41, 2]]);

  const [fn] = evaluate(rt, '(lambda (x &optional (y (+ x 1) yp) &rest tail &key ((:z z) (+ y (length tail)) zp) &aux (trace (list x y yp tail z zp)) (x (list trace x))) (values trace x))');
  const [trace, x] = rt.call(fn, [10, 20, rt.intern('Z', 'KEYWORD'), 40]).values;
  const traceItems = toArray(trace);
  assert.deepEqual(traceItems.slice(0, 3), [10, 20, T]);
  assert.deepEqual(toArray(traceItems[3]), [rt.intern('Z', 'KEYWORD'), 40]);
  assert.deepEqual(traceItems.slice(4), [40, T]);
  assert.deepEqual(toArray(x), [trace, 10]);

  assert.throws(() => compileForm(rt, rt.read('(destructuring-bind (&aux ((a b))) nil nil)')), /expected a variable/);
  assert.throws(() => evaluate(rt, '(lambda (&aux (x 1 supplied)) x)'), /invalid lambda parameter/);
});

test('macro destructuring, whole and lexical macro environments', () => {
  const rt = createRuntime();
  evaluate(rt, '(defmacro destructure ((x &optional (y 3)) &body body) `(list ,x ,y ,@body))');
  assert.deepEqual(toArray(evaluate(rt, '(destructure (1) 4 5)')[0]), [1, 3, 4, 5]);
  evaluate(rt, '(defmacro whole (&whole form x &environment env) (list \'quote (list (car form) x (not (null env)))))');
  const result = toArray(evaluate(rt, '(whole 9)')[0]); assert.equal(result[0], rt.intern('WHOLE')); assert.equal(result[1], 9); assert.equal(result[2], T);
  assert.deepEqual(evaluate(rt, '(macrolet ((twice (x) `(+ ,x ,x))) (twice 21))'), [42]);
  assert.deepEqual(evaluate(rt, '(let ((x 1)) (symbol-macrolet ((y x)) (setq y 42) x))'), [42]);
  assert.throws(() => evaluate(rt, '(destructure ())'), /destructur/i);
});

test('local functions use lexical namespace and recursive shared captures', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(let ((x 3)) (flet ((x (n) (+ n x))) (values x (x 39))))'), [3, 42]);
  assert.deepEqual(evaluate(rt, '(labels ((even-rec (n) (if (= n 0) t (odd-rec (1- n)))) (odd-rec (n) (if (= n 0) nil (even-rec (1- n))))) (even-rec 20))'), [T]);
  assert.deepEqual(evaluate(rt, '(block done (flet ((escape () (return-from done 42))) (escape)) 0)'), [42]);
});

test('tagbody lexical transfers, catch and unwind retain values', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(let ((x 0)) (tagbody again (incf x) (if (< x 10) (go again) nil)) x)'), [10]);
  assert.deepEqual(evaluate(rt, '(let ((trace nil)) (values (catch \'end (unwind-protect (throw \'end 42) (push 1 trace))) trace))').map(x => x instanceof Cons ? toArray(x) : x), [42, [1]]);
  assert.deepEqual(evaluate(rt, '(multiple-value-call #\'list (values 1 2) (values) (values 3))').map(toArray), [[1, 2, 3]]);
  assert.deepEqual(evaluate(rt, '(multiple-value-prog1 (values 1 2) (values 3 4))'), [1, 2]);
});

test('special declarations shadow outer lexical bindings', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(let ((x 1)) (progv \'(x) \'(42) (locally (declare (special x)) x)))'), [42]);
  evaluate(rt, '(defparameter *dynamic* 1)');
  assert.deepEqual(evaluate(rt, '(let ((*dynamic* 42)) (funcall (lambda () *dynamic*)))'), [42]);
  assert.deepEqual(evaluate(rt, '*dynamic*'), [1]);
});

test('reader keeps identity, escapes, exact numbers and circular graphs', () => {
  const rt = createRuntime();
  assert.equal(rt.read('cl:car'), rt.intern('CAR', 'COMMON-LISP'));
  assert.equal(rt.read('|mixed:case|').name, 'mixed:case');
  assert.equal(rt.read('foo\\:bar').name, 'FOO:BAR');
  assert.equal(rt.read('#x20000000000001'), 9007199254740993n);
  assert.equal(rt.read('#36rz'), 35);
  const [string, index] = rt.readFromString('"a\\\"b" trailing'); assert.equal(string.toString(), 'a"b'); assert.equal(index, 7);
  const circular = rt.read('#1=(a . #1#)'); assert.equal(circular.cdr, circular);
  const shared = toArray(rt.read('(#1=(a) #1#)')); assert.equal(shared[0], shared[1]);
  assert.equal(rt.read('#\\😀').toString(), '😀');
  assert.deepEqual(rt.readFromString('😀 42', { start: 2 }), [42, 4]);
  for (const source of ['(a . b c)', '#9#', '#1=#1#', '"unfinished', '|unfinished', '#unknown', 'missing-package:x']) assert.throws(() => rt.read(source), ReaderError);
});

test('reader conditionals suppress read-time evaluation and package lookup', () => {
  const rt = createRuntime();
  assert.equal(rt.read('#+javascript 42 #-javascript 0'), 42);
  assert.equal(rt.read('#-javascript #.(error "must not run") 42'), 42);
  assert.equal(rt.read('#-javascript missing-package:x 42'), 42);
  assert.equal(rt.read('#-javascript #+javascript ignored 42'), 42);
  assert.equal(rt.read('#-javascript #9# 42'), 42);
  assert.equal(rt.read('#.(values 42 43)'), 42);
  assert.throws(() => rt.read('#.42', { readEval: false }), /disabled/);
});

test('resident compiler produces canonical instructions and rejects malformed forms', () => {
  const rt = createRuntime();
  const code = compileForm(rt, rt.read('(let ((x 1)) (lambda () (incf x)))'));
  assert.ok(code.instructions.some(x => x.BoxLocal)); assert.ok(code.instructions.some(x => x.Closure));
  const child = code.constants.find(x => x?.entry?.captures?.length); assert.deepEqual(child.entry.captures, ['MutableCell']);
  for (const source of ['(if 1)', '(setq x)', '(return-from nowhere 1)', '(go nowhere)', '(declare (special x))', '(#(1) 2)']) assert.throws(() => evaluate(rt, source));
  assert.throws(() => evaluate(rt, '(unknown-function 42)'), /Undefined function/i);
});

test('generalized places evaluate each subform once and allow SETF function names', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(let ((a (vector 1 2)) (n 0)) (incf (aref a (progn (incf n) 0)) 41) (values (aref a 0) n))'), [42, 1]);
  evaluate(rt, '(defun selected (x) (car x)) (defun (setf selected) (value x) (setf (car x) value))');
  assert.deepEqual(evaluate(rt, '(let ((x (list 0))) (setf (selected x) 42) (selected x))'), [42]);
  assert.deepEqual(evaluate(rt, '(let ((x 1)) (incf x (setq x 21)))'), [42]);
});

test('handlers run within dynamic extent while handler-case runs after cleanup', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(handler-case (error "failure") (error (c) (values 42 (typep c \'error))))'), [42, T]);
  assert.deepEqual(evaluate(rt, '(handler-case (values 20 22) (:no-error (a b) (+ a b)))'), [42]);
  assert.deepEqual(evaluate(rt, '(block done (handler-bind ((error (lambda (c) (declare (ignore c)) (return-from done 42)))) (error "failure")))'), [42]);
  const result = evaluate(rt, '(ignore-errors (error "failure"))'); assert.equal(result[0], NIL); assert.equal(result[1].className, 'SIMPLE-ERROR');
});

test('restart functions and case clauses use the active task and ordinary lambda arity', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(restart-case (invoke-restart \'use-value 42) (use-value (value) value))'), [42]);
  assert.deepEqual(evaluate(rt, '(restart-bind ((use-value (lambda (value) (+ value 1)))) (invoke-restart \'use-value 41))'), [42]);
  assert.deepEqual(evaluate(rt, '(restart-case (invoke-restart \'use-value 20 22) (use-value (a &optional (b 1)) (+ a b)))'), [42]);
  assert.throws(() => evaluate(rt, '(restart-case (invoke-restart \'use-value) (use-value (a) a))'), /arguments/i);
});

test('compileString processes top-level compile and load phases once', () => {
  const rt = createRuntime();
  evaluate(rt, '(defparameter *phase-count* 0)');
  const compiled = rt.compileString(`
    (eval-when (:compile-toplevel) (incf *phase-count*))
    (eval-when (:load-toplevel) (incf *phase-count* 10))
    (eval-when (:execute) (incf *phase-count* 100))
    (defmacro compiled-twice (x) \`(+ ,x ,x))
    (defun compiled-answer () (compiled-twice 21))
    (macrolet ((phase-macro () '(incf *phase-count* 1000)))
      (eval-when (:compile-toplevel) (phase-macro)))`);
  assert.deepEqual(evaluate(rt, '*phase-count*'), [1001]);
  rt.call(compiled);
  assert.deepEqual(evaluate(rt, '*phase-count*'), [1011]);
  assert.deepEqual(evaluate(rt, '(compiled-answer)'), [42]);
  assert.deepEqual(evaluate(rt, '(progn (defmacro eval-twice (x) (list \'+ x x)) (eval-twice 21))'), [42]);
});

test('resident DEFPACKAGE establishes identities before subsequent reads', () => {
  const rt = createRuntime();
  const oldPackage = rt.currentPackage;
  const compiled = rt.compileString('(defpackage #:compiled-package (:use #:cl) (:export #:answer)) (in-package #:compiled-package) (defun answer () 42)');
  assert.equal(rt.currentPackage, oldPackage);
  assert.equal(rt.readVariable(rt.intern('*PACKAGE*', 'COMMON-LISP')), oldPackage);
  rt.call(compiled);
  assert.equal(rt.currentPackage.name, 'COMPILED-PACKAGE');
  assert.deepEqual(rt.call('COMPILED-PACKAGE:ANSWER').values, [42]);
  evaluate(rt, '(in-package #:cl-user)');
  assert.equal(rt.read('compiled-package:answer'), rt.findSymbol('ANSWER', 'COMPILED-PACKAGE'));
});

test('LOAD-TIME-VALUE uses its primary value and runs once for each compiled-file load', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(+ 1 (load-time-value (values 41 99)))'), [42]);
  evaluate(rt, '(defparameter *load-value-count* 0)');
  const compiled = rt.compileString('(defun load-value-reader () (load-time-value (incf *load-value-count*)))');
  assert.deepEqual(evaluate(rt, '*load-value-count*'), [0]);
  rt.call(compiled); assert.deepEqual(evaluate(rt, '*load-value-count*'), [1]);
  assert.deepEqual(evaluate(rt, '(values (load-value-reader) (load-value-reader))'), [1, 1]);
  rt.call(compiled); assert.deepEqual(evaluate(rt, '*load-value-count*'), [2]);
  assert.deepEqual(evaluate(rt, '(load-value-reader)'), [2]);
});

test('resident type and generalized-place definitions remain usable by compiled functions', () => {
  const rt = createRuntime();
  evaluate(rt, '(deftype small (&optional bound) `(integer 0 ,bound))');
  assert.deepEqual(evaluate(rt, '(values (typep 42 \'small) (typep 42 \'(small 50)) (typep 42 \'(small 40)))'), [T, T, NIL]);
  evaluate(rt, '(defun selected-cell (x) (car x)) (defun update-cell (x value) (rplaca x value) value) (defsetf selected-cell update-cell)');
  assert.deepEqual(evaluate(rt, '(let ((x (list 0))) (incf (selected-cell x) 42) (selected-cell x))'), [42]);
  evaluate(rt, '(defun long-cell (x) (car x)) (defsetf long-cell (x) (value) `(progn (rplaca ,x ,value) ,value))');
  assert.deepEqual(evaluate(rt, '(let ((x (list 0))) (setf (long-cell x) 42) (long-cell x))'), [42]);
  evaluate(rt, '(define-setf-expander custom-cell (x &environment env) (declare (ignore env)) (let ((temp (gensym)) (store (gensym))) (values (list temp) (list x) (list store) `(progn (rplaca ,temp ,store) ,store) `(car ,temp))))');
  assert.deepEqual(evaluate(rt, '(let ((x (list 0))) (setf (custom-cell x) 42) (car x))'), [42]);
});

test('standard and symbol macro expansion use the lexical environment', () => {
  const rt = createRuntime();
  const [expansion, expanded] = evaluate(rt, '(macroexpand-1 \'(when t 42))'); assert.equal(expanded, T); assert.equal(expansion.car, rt.intern('IF', 'COMMON-LISP'));
  assert.equal(evaluate(rt, '(functionp (macro-function \'when))')[0], T);
  evaluate(rt, '(defparameter *symbol-target* 40) (define-symbol-macro symbol-view *symbol-target*)');
  assert.deepEqual(evaluate(rt, '(incf symbol-view 2)'), [42]);
  assert.deepEqual(evaluate(rt, '(let ((symbol-view 3)) symbol-view)'), [3]);
  assert.deepEqual(evaluate(rt, '(symbol-macrolet ((x 1)) (let ((x 42)) x))'), [42]);
  assert.deepEqual(evaluate(rt, '(macroexpand \'symbol-view)'), [rt.intern('*SYMBOL-TARGET*'), T]);
});

test('COMPILE lowers canonical functions to JavaScript without replacing captured cells', () => {
  const rt = createRuntime();
  const [counter, reader] = evaluate(rt, '(let ((x 40)) (values (lambda () (incf x)) (lambda () x)))');
  const compiled = rt.compile(counter);
  assert.equal(compiled, counter); assert.equal(typeof compiled.code.entry.step, 'function');
  assert.deepEqual(rt.call(compiled).values, [41]); assert.deepEqual(rt.call(reader).values, [41]);
  assert.deepEqual(rt.call(rt.compile(rt.read('(lambda (x) (+ x 2))')), [40]).values, [42]);
  assert.deepEqual(evaluate(rt, '(funcall (compile nil (lambda (x) (+ x 2))) 40)'), [42]);
  const outer = rt.compile(rt.read('(lambda () (lambda (x) (+ x 2)))'));
  const child = rt.call(outer).values[0]; assert.equal(typeof child.code.entry.step, 'function');
  assert.deepEqual(rt.call(child, [40]).values, [42]);
});

test('nested macro expansion EVAL retains a locally handled child call result', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, `(macrolet ((local-function () nil)
      (expanded (&environment env)
        (let ((function (macro-function 'local-function env)))
          (list 'quote
            (eval \`(handler-case (funcall ,function) (program-error () t)))))))
    (expanded))`), [T]);
});
