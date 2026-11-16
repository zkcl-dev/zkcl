import assert from 'node:assert/strict';
import test from 'node:test';
import { pathToFileURL } from 'node:url';

// The CLI test supplies a source-built, self-contained application. Invoking
// this file directly also exercises the same runtime's interactive frontend.
const application = process.env.ZKCL_JS_APPLICATION
  ? await import(pathToFileURL(process.env.ZKCL_JS_APPLICATION))
  : null;
const backend = application ?? await import('../runtime/runtime.mjs');
const values = result => {
  assert.ok(Array.isArray(result?.values), 'calls return an explicit multiple-value result');
  return result.values;
};
const evaluate = (runtime, source) => values(runtime.evalString(source));
const call = (runtime, name, args = []) => values(runtime.call(name, args));
const truth = value => value?.name === 'T';
function list(value) {
  const result = [];
  const seen = new Set();
  while (value?.name !== 'NIL') {
    assert.ok(value && 'car' in value && 'cdr' in value, 'a proper Lisp list');
    assert.ok(!seen.has(value), 'an acyclic result list');
    seen.add(value);
    result.push(value.car);
    value = value.cdr;
  }
  return result;
}
function runtime() { return backend.createRuntime(); }

test('interactive EVAL, DEFMACRO and COMPILE share one session', () => {
  const rt = runtime();
  evaluate(rt, '(defmacro session-twice (form) (list \'+ form form))');
  assert.deepEqual(evaluate(rt, '(eval \'(session-twice 21))'), [42]);
  const [compiled, warnings, failure] = evaluate(rt,
    '(compile nil \'(lambda (x) (session-twice x)))');
  assert.ok(compiled);
  assert.equal(warnings?.name, 'NIL');
  assert.equal(failure?.name, 'NIL');
  assert.deepEqual(call(rt, compiled, [21]), [42]);
  evaluate(rt, '(defun session-current () 11)');
  const [old] = evaluate(rt, "#'session-current");
  evaluate(rt, '(defun session-current () 22)');
  assert.deepEqual(call(rt, old), [11]);
  assert.deepEqual(call(rt, 'CL-USER::SESSION-CURRENT'), [22]);
});

test('lexical captures share mutations and separate factory activations', () => {
  const rt = runtime();
  const [factory] = evaluate(rt,
    '(lambda (value) (values (lambda (delta) (incf value delta)) (lambda () value)))');
  const [incrementA, readA] = call(rt, factory, [10]);
  const [incrementB, readB] = call(rt, factory, [100]);
  assert.deepEqual(call(rt, incrementA, [2]), [12]);
  assert.deepEqual(call(rt, readA), [12]);
  assert.deepEqual(call(rt, readB), [100]);
  assert.deepEqual(call(rt, incrementB, [3]), [103]);
  assert.deepEqual(call(rt, readA), [12]);
});

test('lambda initializers retain lexical scope before free SPECIAL declarations', () => {
  const rt = runtime();
  for (const kind of ['&optional', '&key', '&aux']) {
    const [result] = evaluate(rt, `
      (let ((x :bad))
        (declare (special x))
        (let ((x :good))
          ((lambda (${kind} (y x)) (declare (special x)) y))))`);
    assert.equal(result.name, 'GOOD', kind);
  }
  assert.deepEqual(evaluate(rt, `
    (let ((x 0))
      (declare (special x))
      (values (progv '(x y) '(1) (values (boundp 'y))) x))`).map(value => value?.name ?? value), ['NIL', 0]);
});

test('multiple values survive nonlocal exits and cleanup dynamic scope', () => {
  const rt = runtime();
  assert.deepEqual(evaluate(rt, '(values)'), []);
  assert.deepEqual(evaluate(rt, '(values 1 2 3)'), [1, 2, 3]);
  evaluate(rt, '(defparameter *cleanup-scope* :outer)');
  evaluate(rt, '(defparameter *cleanup-trace* nil)');
  const result = evaluate(rt, `
    (multiple-value-bind (a b)
        (block done
          (let ((*cleanup-scope* :inner))
            (unwind-protect
                (funcall (lambda () (return-from done (values 41 42))))
              (push *cleanup-scope* *cleanup-trace*))))
      (values a b *cleanup-trace* *cleanup-scope*))`);
  assert.deepEqual(result.slice(0, 2), [41, 42]);
  assert.deepEqual(list(result[2]).map(value => value.name), ['INNER']);
  assert.equal(result[3].name, 'OUTER');
  assert.deepEqual(evaluate(rt,
    '(catch \'done (unwind-protect (throw \'done (values)) (values 9 10)))'), []);
});

test('symbol, mutable aggregate and host reference identity is retained', () => {
  const rt = runtime();
  const [left, same, other] = evaluate(rt,
    "(let ((x (list 1))) (values x x (list 1)))");
  assert.strictEqual(left, same);
  assert.notStrictEqual(left, other);
  const [identity] = evaluate(rt, '(lambda (value) (values value (eq value value)))');
  const host = { marker: 'host object' };
  const [returned, equal] = call(rt, identity, [rt.fromJS(host)]);
  assert.strictEqual(rt.toJS(returned), host);
  assert.ok(truth(equal));
  assert.ok(truth(evaluate(rt, "(eq 'same-symbol 'same-symbol)")[0]));
  assert.equal(evaluate(rt, '(eq (make-symbol "SAME") (make-symbol "SAME"))')[0].name, 'NIL');
  assert.deepEqual(evaluate(rt,
    '(let ((a (vector 1 2))) (setf (aref a 0) 42) (values (aref a 0) (length a)))'), [42, 2]);
});

test('integers, ratios and float formats preserve exact distinctions', () => {
  const rt = runtime();
  const [large, sum, ratio, single, double, negativeZero] = evaluate(rt,
    '(values 9007199254740993 (+ 9007199254740993 2) (/ 3 7) 1.25f0 1.25d0 -0.0d0)');
  assert.equal(large, 9007199254740993n);
  assert.equal(sum, 9007199254740995n);
  assert.equal(ratio.numerator, 3);
  assert.equal(ratio.denominator, 7);
  assert.equal(single.constructor.name, 'SingleFloat');
  assert.equal(double.constructor.name, 'DoubleFloat');
  assert.equal(single.value, 1.25);
  assert.equal(double.value, 1.25);
  assert.ok(Object.is(negativeZero.value, -0));
  assert.equal(evaluate(rt, '(eql 1.0f0 1.0d0)')[0].name, 'NIL');
  assert.equal(evaluate(rt, '(= 9007199254740993 9007199254740992d0)')[0].name, 'NIL');
  assert.equal(rt.toJS(rt.fromJS(9007199254740993n)), 9007199254740993n);
});

test('JavaScript callbacks reenter Lisp with the active dynamic context', () => {
  const rt = runtime();
  evaluate(rt, '(defparameter *host-scope* :outer)');
  const [readScope] = evaluate(rt, '(lambda () *host-scope*)');
  const observations = [];
  rt.registerFunction('CL-USER::HOST-CALLBACK', (args, current, task) => {
    const [closure, value] = args;
    observations.push(values(current.call(readScope, [], task)));
    return current.call(closure, [value], task);
  });
  assert.deepEqual(evaluate(rt, `
    (let ((*host-scope* :inner))
      (host-callback (lambda (n) (values (+ n 2) *host-scope*)) 40))`).map(value => value?.name ?? value), [42, 'INNER']);
  assert.equal(evaluate(rt, '*host-scope*')[0].name, 'OUTER');
  assert.deepEqual(observations.map(result => result[0].name), ['INNER']);
});

test('host exceptions execute Lisp cleanup and restore dynamic bindings', () => {
  const rt = runtime();
  evaluate(rt, '(defparameter *failure-scope* :outer)');
  evaluate(rt, '(defparameter *failure-cleaned* nil)');
  rt.registerFunction('CL-USER::HOST-FAILURE', () => { throw new Error('expected host failure'); });
  assert.throws(() => evaluate(rt, `
    (let ((*failure-scope* :inner))
      (unwind-protect (host-failure)
        (setq *failure-cleaned* *failure-scope*)))`), /expected host failure/);
  assert.equal(evaluate(rt, '*failure-cleaned*')[0].name, 'INNER');
  assert.equal(evaluate(rt, '*failure-scope*')[0].name, 'OUTER');
});

test('Lisp condition handlers receive errors after intervening cleanup', () => {
  const rt = runtime();
  const result = evaluate(rt, `
    (let ((trace nil))
      (handler-case
          (flet ((fail () (error 'type-error :datum 'foo :expected-type nil)))
            (unwind-protect (fail) (push :cleaned trace)))
        (type-error () (values :handled trace))))`);
  assert.equal(result[0].name, 'HANDLED');
  assert.deepEqual(list(result[1]).map(value => value.name), ['CLEANED']);
  assert.deepEqual(list(evaluate(rt, `
    (let ((trace nil))
      (ignore-errors (unwind-protect (error "expected") (push :cleaned trace)))
      trace)`)[0]).map(value => value.name), ['CLEANED']);
});

if (application) {
  test('Rust-compiled source runs through emitted JavaScript function entries', () => {
    assert.ok(application.program.entries.length > 0);
    const executable = [
      ...application.program.entries,
      ...application.program.nodes.flatMap(node => node.Function ? [node.Function] : []),
    ];
    for (const entry of executable) {
      assert.equal(typeof entry.step, 'function');
      assert.equal(entry.instructions, undefined);
    }
    const rt = application.instantiate();
    assert.equal(rt.currentPackage.name, 'CL-USER');
    assert.ok(truth(evaluate(rt, '(eq *package* (find-package "CL-USER"))')[0]));
    assert.strictEqual(evaluate(rt, "'caller-package-probe")[0].package, rt.findPackage('CL-USER'));
    assert.deepEqual(call(rt, 'JS-APPLICATION:ANSWER'), [42]);
    assert.equal(rt.toJS(evaluate(rt, "(documentation #'js-application:answer 'function)")[0]),
      'Return the application answer.');
    assert.deepEqual(call(rt, 'JS-APPLICATION:TARGET-FEATURE'), [42]);
    assert.deepEqual(call(rt, 'JS-APPLICATION:COMPILE-PHASE-ANSWER'), [42]);
    assert.deepEqual(evaluate(rt, '(js-application::add-two 40)'), [42]);
    assert.equal(evaluate(rt,
      "(macro-function 'js-application::compile-only-answer)")[0].name, 'NIL');
    assert.deepEqual(call(rt, 'JS-APPLICATION:OBJECT-ANSWER'), [42, 42]);
    assert.deepEqual(call(rt, 'JS-APPLICATION:STRUCTURE-ANSWER'), [42]);
    assert.deepEqual(call(rt, 'JS-APPLICATION:CONDITION-ANSWER').map(value => value?.name ?? rt.toJS(value)),
      [42, 'APPLICATION', 'application 42']);
    assert.deepEqual(call(rt, 'JS-APPLICATION:RESTART-ANSWER'), [42]);
    const [increment, read] = call(rt, 'JS-APPLICATION:MAKE-COUNTER', [40]);
    assert.deepEqual(call(rt, increment, [2]), [42]);
    assert.deepEqual(call(rt, read), [42]);
    evaluate(rt, '(defun js-application:current () 22)');
    assert.deepEqual(call(rt, 'JS-APPLICATION:ORIGINAL'), [11]);
    assert.deepEqual(call(rt, 'JS-APPLICATION:CURRENT'), [22]);
    const host = { marker: 'compiled identity' };
    const identity = call(rt, 'JS-APPLICATION:IDENTITY', [rt.fromJS(host)]);
    assert.strictEqual(rt.toJS(identity[0]), host);
    assert.ok(truth(identity[1]));
    let callbacks = 0;
    rt.registerFunction('CL-USER::COMPILED-CALLBACK', ([closure, value], current, task) => {
      callbacks++;
      assert.equal(value, 40);
      return current.call(closure, [2], task);
    });
    const [callback] = evaluate(rt, "#'cl-user::compiled-callback");
    assert.deepEqual(call(rt, 'JS-APPLICATION:HOST-ROUNDTRIP', [callback, 40]), [42]);
    assert.equal(callbacks, 1);
    const numbers = call(rt, 'JS-APPLICATION:NUMBERS');
    assert.equal(numbers[0], 9007199254740993n);
    assert.equal(numbers[1].numerator, 3);
    assert.equal(numbers[1].denominator, 7);
    assert.equal(numbers[2].constructor.name, 'SingleFloat');
    assert.equal(numbers[3].constructor.name, 'DoubleFloat');
    assert.ok(Object.is(numbers[4].value, -0));
    const cleanup = call(rt, 'JS-APPLICATION:CLEANUP-VALUES');
    assert.deepEqual(cleanup.slice(0, 2), [41, 42]);
    assert.deepEqual(list(cleanup[2]).map(value => value.name), ['INNER']);
    assert.equal(cleanup[3].name, 'OUTER');
    const arity = call(rt, 'JS-APPLICATION:ARITY', [1]);
    assert.deepEqual(arity.slice(0, 2), [1, 5]);
    assert.equal(arity[2].name, 'NIL');
    assert.deepEqual(list(arity[3]), []);
    const supplied = call(rt, 'JS-APPLICATION:ARITY', [1, 2, 3, 4]);
    assert.deepEqual(supplied.slice(0, 2), [1, 2]);
    assert.ok(truth(supplied[2]));
    assert.deepEqual(list(supplied[3]), [3, 4]);
    assert.throws(() => call(rt, 'JS-APPLICATION:ARITY'), /argument/i);
    const second = application.instantiate();
    assert.deepEqual(call(second, 'JS-APPLICATION:CURRENT'), [11]);
    assert.deepEqual(call(second, 'JS-APPLICATION:ORIGINAL'), [11]);
  });

  test('a failure in an emitted function propagates without an alternate executor', () => {
    const entry = application.program.entries.at(-1);
    const original = entry.step;
    entry.step = () => { throw new Error('emitted JavaScript entry reached'); };
    const rt = application.createRuntime();
    try {
      assert.throws(() => rt.load(application.program), /emitted JavaScript entry reached/);
      assert.equal(rt.currentPackage.name, 'CL-USER');
      assert.ok(truth(evaluate(rt, '(eq *package* (find-package "CL-USER"))')[0]));
    } finally {
      entry.step = original;
    }
  });
}
