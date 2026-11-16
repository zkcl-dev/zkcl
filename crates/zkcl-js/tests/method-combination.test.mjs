import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T, toArray } from '../runtime/runtime.mjs';

test('short method combination expands its operator and preserves qualifier identities', () => {
  const rt = createRuntime();
  const values = rt.evalString(`
    (defpackage :other-qualifiers (:export :gate))
    (defmacro all-values (&rest forms) \`(and ,@forms))
    (define-method-combination gate :operator all-values :documentation "Methods evaluated until false.")
    (defvar *seen* nil)
    (defgeneric check (value) (:method-combination gate :most-specific-last)
      (:method gate ((value integer)) (push :integer *seen*) (values 42 43))
      (:method gate ((value number)) (push :number *seen*) t)
      (:method :around ((value integer)) (call-next-method (1+ value))))
    (defmethod check other-qualifiers:gate ((value symbol)) t)
    (values (multiple-value-list (check 0)) *seen*
            (handler-case (check 'bad) (program-error () :wrong-qualifier))
            (method-qualifiers (find-method #'check '(gate) (list (find-class 'integer))))
            (documentation 'gate 'method-combination))
  `).values;
  assert.deepEqual(toArray(values[0]), [42, 43]);
  assert.deepEqual(toArray(values[1]).map(symbol => symbol.name), ['INTEGER', 'NUMBER']);
  assert.equal(values[2], rt.read(':wrong-qualifier'));
  assert.equal(toArray(values[3])[0], rt.read('gate'));
  assert.equal(values[4].toString(), 'Methods evaluated until false.');
});

test('long method combination gives synthetic methods the forwarded argument list', () => {
  const rt = createRuntime();
  const [whole, optional, supplied, tail, behavior, noNext] = rt.evalString(`
    (define-method-combination collect-values (&optional (direction :most-specific-first))
        ((outer (:around)) (values-group (:read . *) :required t :order direction) (unqualified nil))
      (:arguments &whole arguments input excess &optional (item :missing item-p) &rest tail &key flag)
      (:generic-function generic)
      (unless (eq generic (fdefinition 'collect-it)) (method-combination-error "Unexpected generic function"))
      (let ((body \`(values ,arguments ,item ,item-p ,tail
                           (list ,input ,excess ,flag
                                 ,@(mapcar (lambda (method) \`(call-method ,method)) values-group)))))
        \`(call-method ,(first outer) ((make-method ,body)))))
    (defgeneric collect-it (value &optional optional &key flag)
      (:method-combination collect-values :most-specific-last))
    (defmethod collect-it :around (value &optional optional &key flag)
      (declare (ignore optional flag))
      (call-next-method (1+ value) nil :flag :changed))
    (defmethod collect-it :read ((value number) &optional optional &key flag) (declare (ignore optional flag)) value)
    (defmethod collect-it :read 77 ((value integer) &optional optional &key flag) (declare (ignore value optional flag)) 77)
    (define-method-combination lone () ((methods *)) \`(call-method ,(first methods)))
    (defgeneric lone-it (x) (:method-combination lone) (:method (x) (call-next-method)))
    (multiple-value-call #'values (collect-it 20) (handler-case (lone-it nil) (control-error () :no-next)))
  `).values;
  const flag = rt.read(':flag'), changed = rt.read(':changed');
  assert.deepEqual(toArray(whole), [21, NIL, flag, changed]);
  assert.equal(optional, NIL);
  assert.equal(supplied, T);
  assert.deepEqual(toArray(tail), [flag, changed]);
  assert.deepEqual(toArray(behavior), [21, NIL, changed, 21, 77]);
  assert.equal(noNext, rt.read(':no-next'));
});

test('MAKE-METHOD compiles its form in the null lexical environment', () => {
  const rt = createRuntime();
  const [outside, inside, result] = rt.evalString(`
    (defmacro pick () :global)
    (define-method-combination choice () ((methods *))
      \`(macrolet ((pick () :local))
          (let ((before (pick)))
            (multiple-value-bind (after value)
                (call-method (make-method (values (pick) (call-method ,(first methods)))))
              (values before after value)))))
    (defgeneric choose (x) (:method-combination choice) (:method (x) x))
    (choose 42)
  `).values;
  assert.deepEqual([outside, inside, result], [rt.read(':local'), rt.read(':global'), 42]);
});

test('long combination predicates, builders and method calls can suspend in their task', async () => {
  const rt = createRuntime(), seen = [], waiting = new Map();
  const marker = rt.intern('*MARKER*');
  rt.registerFunction('COMBINATION-GATE', ([phase], runtime, task) => {
    const value = runtime.readVariable(marker, task);
    seen.push([phase.name, value]);
    if (phase.name !== 'PREDICATE') return Promise.resolve(NIL);
    return new Promise(resolve => waiting.set(value, resolve));
  });
  rt.evalString(`
    (defvar *marker* nil)
    (defun selected (qualifiers) (combination-gate :predicate) (equal qualifiers '(3)))
    (define-method-combination choose () ((selected selected :required t) (ignored *))
      (combination-gate :builder)
      \`(call-method ,(first selected)))
    (defgeneric chosen (value) (:method-combination choose))
    (defmethod chosen 3 (value) (combination-gate :method) (values value *marker*))
    (defun run-choice (value) (let ((*marker* value)) (chosen value)))
  `);
  const first = rt.callAsync('RUN-CHOICE', [10]), second = rt.callAsync('RUN-CHOICE', [20]);
  assert.deepEqual(seen, [['PREDICATE', 10], ['PREDICATE', 20]]);
  waiting.get(20)(NIL);
  assert.deepEqual((await second).values, [20, 20]);
  waiting.get(10)(NIL);
  assert.deepEqual((await first).values, [10, 10]);
  assert.deepEqual(seen, [['PREDICATE', 10], ['PREDICATE', 20], ['BUILDER', 20], ['METHOD', 20], ['BUILDER', 10], ['METHOD', 10]]);
});
