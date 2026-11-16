import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T } from '../runtime/runtime.mjs';

test('MAKE-LOAD-FORM saves initialized local slots without running their initforms', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defvar *initialized* 0)
    (defclass saved ()
      ((initialized :initform (incf *initialized*))
       (self) (unbound) (shared :allocation :class :initform :shared)))
    (defmethod make-load-form ((object saved) &optional environment)
      (make-load-form-saving-slots object :environment environment))
    (let ((source (make-instance 'saved)))
      (setf (slot-value source 'self) source)
      (multiple-value-bind (create initialize) (make-load-form source)
        (let ((target (eval create)))
          (eval (subst target source initialize))
          (values *initialized* (slot-value target 'initialized)
                  (eq (slot-value target 'self) target) (slot-boundp target 'unbound)
                  (slot-value target 'shared) (eq (eval (make-load-form (class-of target))) (class-of target))))))
  `);
  assert.deepEqual(result.values, [1, 1, T, NIL, rt.read(':shared'), T]);
});

test('saving a structure preserves read-only slots without exposing a writer', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defstruct readonly (value 0 :read-only t))
    (let ((source (make-readonly :value 42)))
      (multiple-value-bind (create initialize) (make-load-form-saving-slots source)
        (let ((target (eval create)))
          (eval (subst target source initialize))
          (values (readonly-value target) (eq (class-of target) (class-of source))
                  (fboundp '(setf readonly-value))))))
  `);
  assert.deepEqual(result.values, [42, T, NIL]);
});
