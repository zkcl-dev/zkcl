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

test('compound condition types reuse a matching class or inherit an anonymous layout', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (define-condition condition-p () ())
    (define-condition condition-q () ())
    (define-condition p-first (condition-p condition-q) ())
    (define-condition q-first (condition-q condition-p) ())
    (define-condition alternate () ())
    (let* ((spec '(and (or p-first alternate) q-first))
           (combined (make-condition spec))
           (unnamed (make-condition '(and simple-error type-error)
                      :datum 17 :expected-type 'string :format-control "bad ~A" :format-arguments '(17)))
           (class (class-of unnamed)))
      (values
        (typep (make-condition '(and simple-condition type-error)) 'simple-type-error)
        (typep combined spec)
        (typep combined 'alternate)
        (eq (class-of combined) (class-of (make-condition spec)))
        (eq class (class-of (make-condition '(and type-error simple-error))))
        (null (class-name class))
        (typep unnamed (type-of unnamed))
        (not (null (member class (zk-mop:class-direct-subclasses (find-class 'type-error)))))
        (eql (type-error-datum unnamed) 17)
        (string= (princ-to-string unnamed) "bad 17")))`);
  assert.deepEqual(result.values, Array(10).fill(T));
});

test('condition type expanders run in the current task and class objects keep their identity', async () => {
  const rt = createRuntime();
  rt.registerFunction('CL-USER::WAIT-IN-TYPE', async () => NIL);
  const result = await rt.evalStringAsync(`
    (defvar *condition-kind* 'error)
    (define-condition some-problem (warning) ((code :initarg :code :reader problem-code)))
    (define-condition sub-problem (some-problem) ())
    (deftype selected-condition (&optional (extra nil))
      (wait-in-type)
      (if extra (list 'and *condition-kind* extra) *condition-kind*))
    (let* ((*condition-kind* 'some-problem)
           (class (find-class 'some-problem)))
      (setf (class-name class) 'type-error
            (find-class 'problem-alias) class)
      (values
        (eq (class-of (make-condition class :code 1)) class)
        (eq (class-of (make-condition 'problem-alias :code 2)) class)
        (handler-case (error 'selected-condition :code 3)
          (some-problem (condition) (problem-code condition)))
        (typep (make-condition '(or (selected-condition sub-problem) error)) 'sub-problem)))`);
  assert.deepEqual(result.values, [T, T, 3, T]);
  assert.throws(() => rt.evalString("(error (make-condition 'problem-alias))"),
    error => error.className === 'WARNING' && error.classObject === rt.findClass(rt.intern('SOME-PROBLEM')));
  assert.equal(rt.evalString("(write-to-string (make-condition 'problem-alias))").values[0].toString(), '#<TYPE-ERROR>');
});

test('CERROR preserves an existing condition and formats its restart with extra arguments', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (let ((original (make-condition 'simple-error :format-control "Original")))
      (handler-bind ((error (lambda (condition)
                             (assert (eq condition original))
                             (assert (string= "Continue 42" (princ-to-string (find-restart 'continue condition))))
                             (continue condition))))
        (cerror "Continue ~D" original 42)
        t))`);
  assert.deepEqual(result.values, [T]);
});

test('invalid condition inputs and cell errors expose Lisp datum and type information', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defpackage :condition-other (:use :cl) (:shadow :or))
    (let ((bad '(condition-other::or error warning)))
      (values
        (handler-case (make-condition bad)
          (type-error (c) (and (eq (type-error-datum c) bad) (eq (type-error-expected-type c) 'condition))))
        (handler-case (make-condition (find-class 'standard-object))
          (type-error (c) (eq (type-error-expected-type c) 'condition)))
        (handler-case (warn (make-condition 'warning) :extra)
          (type-error (c) (and (equal (type-error-datum c) '(:extra)) (eq (type-error-expected-type c) 'null))))
        (handler-case (stream-error-stream 12)
          (type-error (c) (and (eql (type-error-datum c) 12) (eq (type-error-expected-type c) 'stream-error))))
        (handler-case unavailable-variable
          (unbound-variable (c) (eq (cell-error-name c) 'unavailable-variable)))
        (handler-case (unavailable-function)
          (undefined-function (c) (eq (cell-error-name c) 'unavailable-function)))))`);
  assert.deepEqual(result.values, Array(6).fill(T));
});

test('debugger hooks precede the host debugger and normal returns propagate the original condition', () => {
  for (const executionStrategy of ['javascript', 'bytecode']) {
    const entries = [];
    const rt = createRuntime({ executionStrategy, debugger(condition, runtime, task) {
      entries.push(['host', condition, runtime.variableValue(runtime.intern('*DEBUGGER-HOOK*'), task)]);
      return T;
    } });
    rt.registerFunction('CL-USER::RECORD-DEBUGGER-ENTRY', ([stage, ...args]) => {
      entries.push([stage.name.toLowerCase(), ...args]);
      return NIL;
    });
    rt.evalString(`
      (defvar *debugger-cleanups* 0)
      (defvar *debugger-condition* (make-condition 'simple-error :format-control "Original"))
      (defun recording-hook (condition previous)
        (record-debugger-entry :hook condition previous *debugger-hook*))`);
    const original = rt.evalString('*debugger-condition*').values[0];
    const hook = rt.intern('RECORDING-HOOK');
    for (const [operator, expected] of [['ERROR', [['handler', original]]], ['INVOKE-DEBUGGER', []]]) {
      entries.length = 0;
      assert.throws(() => rt.evalString(`
        (let ((*debugger-hook* 'recording-hook))
          (unwind-protect
            (handler-bind ((condition (lambda (c) (record-debugger-entry :handler c))))
              (${operator} *debugger-condition*))
            (incf *debugger-cleanups*)))`), error => error === original);
      assert.deepEqual(entries, [...expected, ['hook', original, hook, NIL], ['host', original, hook]]);
    }
    assert.deepEqual(rt.evalString('*debugger-cleanups*').values, [2]);
    rt.setDebugger();
    assert.throws(() => rt.evalString('(error *debugger-condition*)'), error => error === original);
    assert.deepEqual(entries, [['hook', original, hook, NIL], ['host', original, hook]]);
  }
});

test('break-on-signals suppresses recursive breaks and continues the same condition with its own restart', async () => {
  const rt = createRuntime();
  rt.registerFunction('CL-USER::WAIT-IN-DEBUGGER', async () => NIL);
  const result = await rt.evalStringAsync(`
    (defun debugger-break-type (condition)
      (assert (null *break-on-signals*))
      (signal 'condition)
      (typep condition 'warning))
    (let ((original (make-condition 'warning))
          (other (make-condition 'condition))
          (type '(satisfies debugger-break-type))
          (break-restart nil)
          (nested 0)
          (handled 0))
      (let ((*break-on-signals* type)
            (*debugger-hook*
              (lambda (condition previous)
                (declare (ignore previous))
                (wait-in-debugger)
                (unless (eq condition original) (invoke-debugger condition))
                (assert (null *break-on-signals*))
                (assert (null *debugger-hook*))
                (assert (null (find-restart 'continue other)))
                (setf break-restart (find-restart 'continue condition))
                (assert (string= "Continue signaling." (princ-to-string break-restart)))
                (invoke-restart break-restart))))
        (handler-bind ((warning
                        (lambda (condition)
                          (assert (eq condition original))
                          (assert (eq *break-on-signals* type))
                          (let ((restart (find-restart 'continue condition)))
                            (assert (not (eq restart break-restart)))
                            (assert (string= "Return 42" (princ-to-string restart)))
                            (incf handled)
                            (invoke-restart restart))))
                       (condition (lambda (condition)
                                    (unless (eq condition original) (incf nested)))))
          (cerror "Return ~D" original 42))
        (values handled nested (find-restart 'continue original))))`);
  assert.deepEqual(result.values, [1, 1, NIL]);
});

test('BREAK enters the host debugger directly and CONTINUE returns one NIL', () => {
  const reports = [];
  const rt = createRuntime({ debugger(condition, runtime, task) {
    const [message] = runtime.call('PRINC-TO-STRING', [condition], task).values;
    const [restart] = runtime.call('FIND-RESTART', [runtime.intern('CONTINUE'), condition], task).values;
    const [report] = runtime.call('PRINC-TO-STRING', [restart], task).values;
    reports.push([message.toString(), report.toString()]);
    return runtime.call('CONTINUE', [condition], task);
  } });
  const result = rt.evalString(`
    (catch 'intercepted
      (let ((*break-on-signals* t)
            (*debugger-hook* (lambda (&rest args) (throw 'intercepted args))))
        (handler-bind ((condition (lambda (c) (throw 'intercepted c))))
          (break "Stopped at ~D" 42))))`);
  assert.deepEqual(result.values, [NIL]);
  assert.deepEqual(reports, [['Stopped at 42', 'Return from BREAK.']]);
});
