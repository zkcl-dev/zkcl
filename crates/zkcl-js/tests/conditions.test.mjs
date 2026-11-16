import assert from 'node:assert/strict';
import test from 'node:test';
import { createRuntime, NIL, T } from '../runtime/runtime.mjs';

test('condition class declarations, inherited slots, and reports agree in EVAL and COMPILE', () => {
  const source = `
    (define-condition invalid-item (type-error)
      ((code :initarg :code :reader item-code :initform 17))
      (:default-initargs :datum :item :expected-type 'string)
      (:report (lambda (condition stream)
                 (format stream "code ~D for ~S" (item-code condition) (type-error-datum condition)))))
    (define-condition missing-item (invalid-item) ())
    (handler-case (error 'missing-item :code 42 :code 0)
      ((or reader-error invalid-item) (condition)
       (values (eq (class-of condition) (find-class 'missing-item))
               (eq (type-of condition) 'missing-item)
               (item-code condition)
               (type-error-datum condition)
               (type-error-expected-type condition)
               (princ-to-string condition)
               (format nil "~A" (list condition)))))`;
  for (const mode of ['eval', 'compile']) {
    const rt = createRuntime();
    const result = mode === 'eval' ? rt.evalString(source) : rt.call(rt.compileString(source));
    assert.deepEqual(result.values.slice(0, 3), [T, T, 42]);
    assert.deepEqual(result.values.slice(3).map(x => x.name ?? x.toString()),
      ['ITEM', 'STRING', 'code 42 for :ITEM', '(code 42 for :ITEM)']);
  }
});

test('compile-time condition declarations do not evaluate shared slot initializers', () => {
  const rt = createRuntime();
  rt.evalString('(defparameter *class-initializations* 0)');
  const unit = rt.compileString(`
    (define-condition base-event ()
      ((stamp :allocation :class :reader event-stamp :initform (incf *class-initializations*))))
    (define-condition derived-event (base-event) ())
    (values (event-stamp (make-condition 'derived-event)) *class-initializations*)`);
  assert.deepEqual(rt.evalString("(values (class-name (find-class 'base-event)) *class-initializations*)").values,
    [rt.intern('BASE-EVENT'), 0]);
  assert.deepEqual(rt.call(unit).values, [1, 1]);
  assert.deepEqual(rt.call(unit).values, [1, 1]);
});

test('condition matching uses package identity and reports may suspend in the active task', async () => {
  const rt = createRuntime();
  rt.registerFunction('CL-USER::WAIT-FOR-REPORT', async () => 42);
  const result = await rt.evalStringAsync(`
    (defpackage :report-a (:use :cl))
    (defpackage :report-b (:use :cl))
    (define-condition report-a::failure (error) ())
    (define-condition report-b::failure (error) ()
      (:report (lambda (c s) (declare (ignore c))
                 (format s "~D/~S" (wait-for-report) *print-escape*))))
    (handler-case (error 'report-b::failure)
      (report-a::failure () :wrong-class)
      (report-b::failure (c) (values (format nil "~A" c) *print-escape*)))`);
  assert.deepEqual(result.values.map(x => x.name ?? x.toString()), ['42/NIL', 'T']);
});

test('continuable errors and muffled warnings return through their own active restarts', () => {
  for (const executionStrategy of ['javascript', 'bytecode']) {
    const output = [];
    const rt = createRuntime({ executionStrategy, output: { write: text => output.push(text) } });
    const source = `
      (values
        (handler-bind ((error (lambda (c)
                               (unless (string= "Resume 42" (princ-to-string (find-restart 'continue c)))
                                 (throw 'wrong-report nil))
                               (continue c))))
          (cerror "Resume ~D" "Error ~D" 42)
          42)
        (handler-bind ((warning #'muffle-warning)) (warn "muffled ~D" 42))
        (restart-bind ((choose #'identity :interactive-function (lambda () '(42))))
          (invoke-restart-interactively 'choose)))`;
    assert.deepEqual(rt.call(rt.compileString(source)).values, [42, NIL, 42]);
    assert.deepEqual(output, []);
    assert.deepEqual(rt.evalString('(warn "visible ~D" 42)').values, [NIL]);
    assert.match(output.join(''), /Warning: visible 42/);
  }
});

test('RESTART-CASE associates a signaling call with just the condition it constructs', () => {
  const rt = createRuntime();
  assert.deepEqual(rt.evalString(`
    (let ((other (make-condition 'simple-error :format-control "other")))
      (handler-bind ((error (lambda (c)
                             (when (and (find-restart 'recover c)
                                        (not (find-restart 'recover other)))
                               (invoke-restart (find-restart 'recover c))))))
        (restart-case (error "first") (recover () 42))))`).values, [42]);
});
