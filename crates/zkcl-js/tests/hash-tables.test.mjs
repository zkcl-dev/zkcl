import assert from 'node:assert/strict';
import test from 'node:test';
import { NIL, T, createRuntime } from '../runtime/runtime.mjs';

test('hash tests use Lisp equality across distinct objects, numeric types, and cyclic keys', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (let ((equal (make-hash-table :test 'equal)) (eql (make-hash-table))
          (equalp (make-hash-table :test 'equalp)))
      (let ((cycle '#1=(a . #1#)) (copy '#2=(a a . #2#)))
        (setf (gethash cycle equal) 42 (gethash #\\A eql) :character
              (gethash 2 equalp) :number (gethash "a🐱" equalp) :array)
        (values (gethash copy equal) (gethash #\\A eql) (gethash 2.0d0 equalp)
                (gethash (vector #\\A #\\🐱) equalp) (= (sxhash cycle) (sxhash copy))
                (remhash copy equal) (hash-table-count equal))))`).values;
  assert.deepEqual(result.map(x => x?.name ?? x), [42, 'CHARACTER', 'NUMBER', 'ARRAY', 'T', 'T', 0]);
});

test('hash iterator is lexical and permits deleting its current entry', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (let ((table (make-hash-table)))
      (setf (gethash :a table) 2 (gethash :b table) 40)
      (with-hash-table-iterator (next table)
        (macrolet ((iterator-expanded-p (&environment env)
                     (nth-value 1 (macroexpand-1 '(next) env))))
          (let ((sum 0))
            (loop (multiple-value-bind (more key value) (next)
                    (unless more (return))
                    (remhash key table)
                    (incf sum value)))
            (values sum (hash-table-count table) (next) (iterator-expanded-p))))))`).values;
  assert.deepEqual(result, [42, 0, NIL, T]);
});

test('compiled literal tables retain identity across delayed graph patches', () => {
  const rt = createRuntime();
  const code = (instructions, constants, registers = 1) => ({ instructions, constants, registers, arity: { required: 0, maximum: 0 } });
  const nodes = [
    { HashTable: { test: 'Equal', weakness: null, rehash_size: '1', rehash_threshold: '1', entries: [[1, 0]] } },
    { Pair: [2, 3] }, { LoadForm: { creation: 1, initialization: null } }, 'Nil',
  ];
  rt.registerFunction('HASH-EARLY-LOOKUP', ([table]) => table.get(NIL));
  nodes.push({ Primitive: 'HASH-EARLY-LOOKUP' });
  const entries = [
    code([{ Constant: [0, 0] }, { Constant: [1, 1] },
      { Call: { callee: { Value: 0 }, args: { start: 1, end: 2 }, out: null } }], [4, 0], 2),
    code([{ Load: [0, { Fixnum: 42 }] }, { SetValues: { start: 0, end: 1 } }], []),
    code([{ Constant: [0, 0] }, { SetValues: { start: 0, end: 1 } }], [0]),
  ];
  const [table] = rt.load({ target: 'js', nodes, entries }).values;
  assert.deepEqual(table.get(rt.read('(42)')), [table, T]);
});

test('package iterators preserve declarations and report symbols in the receiving package', () => {
  const rt = createRuntime();
  const source = `
    (defpackage :iterator-source (:use) (:export :public))
    (defpackage :iterator-client (:use :iterator-source) (:intern :private))
    (let ((names nil))
      (do-symbols (symbol :iterator-client (assert (null symbol)))
        (declare (type symbol symbol))
        (push (symbol-name symbol) names))
      (with-package-iterator (next :iterator-client :inherited)
        (multiple-value-bind (more symbol status package) (next)
          (values (length names) more (symbol-name symbol) status
                  (package-name package) (next)))))`;
  assert.deepEqual(rt.call(rt.compileString(source)).values.map(x => x?.name ?? rt.toJS(x)),
    [2, 'T', 'PUBLIC', 'INHERITED', 'ITERATOR-CLIENT', 'NIL']);
});
