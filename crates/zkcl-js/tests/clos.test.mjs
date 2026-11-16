import test from 'node:test';
import assert from 'node:assert/strict';
import { NIL, T, Cons, LispSymbol, fromArray, toArray } from '../runtime/objects.mjs';
import { MultipleValues, CallRequest } from '../runtime/execution.mjs';
import { LispCondition } from '../runtime/execution.mjs';
import { SingleFloat, DoubleFloat } from '../runtime/numbers.mjs';
import { installClos, StandardInstance, EqlSpecializer } from '../runtime/clos.mjs';
import { createRuntime } from '../runtime/runtime.mjs';

// The protocol fixture exercises CLOS independently of the source compiler.
// Integration tests use the same defineBuiltin/call/intern protocol.
function runtime() {
  const symbols = new Map([['COMMON-LISP:NIL', NIL], ['COMMON-LISP:T', T]]);
  const rt = {
    functions: new Map(),
    setfFunctions: new Map(),
    findPackage(name) { return name; },
    exportSymbol(symbol) { return symbol; },
    multipleValues(...values) { return new MultipleValues(values); },
    requestCall(callee, args = []) { return new CallRequest(callee, args); },
    intern(name, pkg = 'CL-USER') {
      const key = `${pkg}:${name}`;
      if (!symbols.has(key)) symbols.set(key, new LispSymbol(name, pkg));
      return symbols.get(key);
    },
    defineBuiltin(name, fn) { this.defineFunction(this.intern(name, 'COMMON-LISP'), { invoke: fn }); },
    setfName(name) { return name instanceof Cons && name.car === this.intern('SETF', 'COMMON-LISP') ? name.cdr.car : null; },
    readFunction(name) {
      const setf = this.setfName(name);
      const fn = setf ? this.setfFunctions.get(setf) : this.functions.get(name);
      if (!fn) throw new LispCondition('UNDEFINED-FUNCTION', 'Undefined function');
      return fn;
    },
    defineFunction(name, fn) {
      const setf = this.setfName(name);
      if (setf) this.setfFunctions.set(setf, fn);
      else this.functions.set(name, fn);
    },
    call(fn, args = [], task = { closMethodContexts: [] }) {
      if (fn instanceof LispSymbol || fn instanceof Cons) fn = this.readFunction(fn);
      let result = fn.invoke(args, this, task);
      if (result instanceof CallRequest) return this.call(result.callee, result.args, task);
      if (typeof result?.next !== 'function') return result;
      const iterator = result;
      try {
        let step = iterator.next();
        while (!step.done) step = iterator.next(this.call(step.value.callee, step.value.args, task));
        return step.value;
      } catch (error) { iterator.return(); throw error; }
    },
    fn(name, ...args) { const symbol = this.intern(name, 'COMMON-LISP'); return this.call(this.functions.has(symbol) ? symbol : this.intern(name, 'ZK-MOP'), args); },
  };
  return installClos(rt);
}
const native = fn => ({ invoke: fn });

test('classes are Lisp identities, inherited slots initialize and unbound slots signal', () => {
  const rt = runtime(), x = rt.intern('X'), y = rt.intern('Y'), key = rt.intern('X', 'KEYWORD');
  const a = rt.ensureClass(rt.intern('A'), [], [{ name: x, initargs: [key], initfunction: native(() => 11) }]);
  const b = rt.ensureClass(rt.intern('B'), [a], [y]);
  const object = rt.fn('MAKE-INSTANCE', b, key, 27);
  assert.ok(object instanceof StandardInstance);
  assert.equal(rt.fn('CLASS-OF', object), b);
  assert.equal(rt.fn('SLOT-VALUE', object, x), 27);
  assert.equal(rt.fn('SLOT-BOUNDP', object, y), NIL);
  assert.throws(() => rt.fn('SLOT-VALUE', object, y), /Unbound slot/);
  assert.throws(() => rt.fn('SLOT-VALUE', object, rt.intern('MISSING')), /Missing slot/);
  assert.equal(rt.isInstanceOf(object, a), true);
  assert.equal(rt.classOf(new SingleFloat(2)).name.name, 'SINGLE-FLOAT');
  assert.equal(rt.classOf(new DoubleFloat(2)).name.name, 'DOUBLE-FLOAT');
});

test('shared slots belong to their defining class and retain their cell on redefinition', () => {
  const rt = runtime(), count = rt.intern('COUNT'); let calls = 0;
  const a = rt.ensureClass(rt.intern('A'), [], [{ name: count, allocation: 'CLASS', initfunction: native(() => ++calls) }]);
  const b = rt.ensureClass(rt.intern('B'), [a], []);
  const first = rt.fn('MAKE-INSTANCE', a), second = rt.fn('MAKE-INSTANCE', b);
  assert.equal(calls, 1);
  rt.setSlotValue(first, count, 9);
  assert.equal(rt.slotValue(second, count), 9);
  assert.equal(rt.ensureClass(a.name, [], [{ name: count, allocation: 'CLASS', initfunction: native(() => 99) }]), a);
  assert.equal(rt.slotValue(first, count), 9);
  assert.equal(rt.slotValue(second, count), 9);
});

test('class redefinition migrates existing instances lazily without changing identity', () => {
  const rt = runtime(), keep = rt.intern('KEEP'), gone = rt.intern('GONE'), added = rt.intern('ADDED');
  const cls = rt.ensureClass(rt.intern('C'), [], [keep, gone]);
  const object = rt.fn('MAKE-INSTANCE', cls); rt.setSlotValue(object, keep, 31); rt.setSlotValue(object, gone, 44);
  const oldVersion = object.storage.version, seen = [];
  rt.ensureMethod(rt.intern('UPDATE-INSTANCE-FOR-REDEFINED-CLASS', 'COMMON-LISP'), {
    lambdaList: fromArray(['O', 'A', 'D', 'P', '&REST', 'INITARGS'].map(n => rt.intern(n))),
    qualifiers: ['BEFORE'], specializers: [cls, T, T, T],
  }, native(args => { seen.push(args); return NIL; }));
  rt.ensureClass(cls.name, [], [keep, { name: added, initfunction: native(() => 17) }]);
  assert.equal(object.storage.version, oldVersion);
  assert.equal(rt.slotValue(object, keep), 31);
  assert.equal(rt.slotValue(object, added), 17);
  assert.equal(seen.length, 1);
  assert.equal(seen[0][0], object);
  assert.deepEqual(toArray(seen[0][1]), [added]);
  assert.deepEqual(toArray(seen[0][2]), [gone]);
  assert.deepEqual(toArray(seen[0][3]), [gone, 44]);
  assert.equal(object.classObject, cls);
});

test('ANSI local precedence constraints resolve multiple inheritance and reject cycles', () => {
  const rt = runtime();
  const a = rt.ensureClass(rt.intern('A')), b = rt.ensureClass(rt.intern('B'));
  const left = rt.ensureClass(rt.intern('LEFT'), [a, b]);
  const right = rt.ensureClass(rt.intern('RIGHT'), [b]);
  const leaf = rt.ensureClass(rt.intern('LEAF'), [left, right]);
  assert.deepEqual(leaf.cpl.slice(0, 5), [leaf, left, a, right, b]);
  assert.throws(() => rt.ensureClass(a.name, [leaf]), /Circular/);
  const reverse = rt.ensureClass(rt.intern('REVERSE'), [b, a]);
  assert.throws(() => rt.ensureClass(rt.intern('BAD'), [left, reverse]), /Inconsistent/);
});

test('generic functions specialize all required arguments, support EQL, and replace methods', () => {
  const rt = runtime(), a = rt.ensureClass(rt.intern('A')), b = rt.ensureClass(rt.intern('B'), [a]);
  const params = fromArray([rt.intern('X'), rt.intern('Y')]);
  const gf = rt.ensureGenericFunction(rt.intern('G'), { lambdaList: params });
  const method = (specs, fn) => rt.ensureMethod(gf, { lambdaList: params, specializers: specs }, native(fn));
  method([a, a], () => 'aa'); method([b, a], () => 'ba'); method([a, b], () => 'ab');
  const av = rt.fn('MAKE-INSTANCE', a), bv = rt.fn('MAKE-INSTANCE', b);
  assert.equal(rt.call(gf, [bv, bv]), 'ba');
  gf.argumentPrecedenceOrder = [1, 0]; assert.equal(rt.call(gf, [bv, bv]), 'ab');
  method([new EqlSpecializer(bv), b], () => 'eql'); assert.equal(rt.call(gf, [bv, bv]), 'eql');
  method([new EqlSpecializer(bv), b], () => 'replacement');
  assert.equal(rt.call(gf, [bv, bv]), 'replacement');
  assert.equal(gf.methods.length, 4);
  assert.equal(rt.call(gf, [av, bv]), 'ab');
});

test('standard combination orders around/before/primary/after and preserves multiple values', () => {
  const rt = runtime(), a = rt.ensureClass(rt.intern('A')), b = rt.ensureClass(rt.intern('B'), [a]);
  const params = fromArray([rt.intern('OBJECT')]), events = [];
  const gf = rt.ensureGenericFunction(rt.intern('G'), { lambdaList: params });
  const method = (spec, qualifier, fn) => rt.ensureMethod(gf, { lambdaList: params, specializers: [spec], qualifiers: qualifier ? [qualifier] : [] }, native(fn));
  method(a, 'BEFORE', () => events.push('before-a')); method(b, 'BEFORE', () => events.push('before-b'));
  method(a, 'AFTER', () => events.push('after-a')); method(b, 'AFTER', () => events.push('after-b'));
  method(a, null, () => { events.push('primary-a'); return new MultipleValues([41, 42]); });
  method(b, null, (_args, _rt, task) => { events.push('primary-b'); assert.equal(rt.call(rt.intern('NEXT-METHOD-P', 'COMMON-LISP'), [], task), T); return rt.call(rt.intern('CALL-NEXT-METHOD', 'COMMON-LISP'), [], task); });
  method(a, 'AROUND', (_args, _rt, task) => { events.push('around-enter'); const v = rt.call(rt.intern('CALL-NEXT-METHOD', 'COMMON-LISP'), [], task); events.push('around-exit'); return v; });
  const task = { closMethodContexts: [] }, result = rt.call(gf, [rt.fn('MAKE-INSTANCE', b)], task);
  assert.deepEqual(result.values, [41, 42]);
  assert.deepEqual(events, ['around-enter', 'before-b', 'before-a', 'primary-b', 'primary-a', 'after-a', 'after-b', 'around-exit']);
  assert.deepEqual(task.closMethodContexts, []);
  assert.throws(() => rt.call(rt.intern('CALL-NEXT-METHOD', 'COMMON-LISP'), [], task), /No next/);
});

test('change-class preserves object identity and common local slots; initializes new slots', () => {
  const rt = runtime(), common = rt.intern('COMMON'), added = rt.intern('ADDED');
  const a = rt.ensureClass(rt.intern('A'), [], [common]);
  const b = rt.ensureClass(rt.intern('B'), [], [common, { name: added, initfunction: native(() => 8) }]);
  const object = rt.fn('MAKE-INSTANCE', a); rt.setSlotValue(object, common, 7);
  assert.equal(rt.fn('CHANGE-CLASS', object, b), object);
  assert.equal(rt.classOf(object), b); assert.equal(rt.slotValue(object, common), 7); assert.equal(rt.slotValue(object, added), 8);
});

test('CALL-NEXT-METHOD rejects argument changes that change applicability', () => {
  const rt = runtime(), a = rt.ensureClass(rt.intern('A')), b = rt.ensureClass(rt.intern('B'), [a]);
  const params = fromArray([rt.intern('X')]), gf = rt.ensureGenericFunction(rt.intern('G'), { lambdaList: params });
  rt.ensureMethod(gf, { lambdaList: params, specializers: [a] }, native(() => 1));
  rt.ensureMethod(gf, { lambdaList: params, specializers: [b] }, native((_args, _rt, task) => rt.call(rt.intern('CALL-NEXT-METHOD', 'COMMON-LISP'), [rt.fn('MAKE-INSTANCE', a)], task)));
  assert.throws(() => rt.call(gf, [rt.fn('MAKE-INSTANCE', b)]), /change the ordered applicable methods/);
});

test('Lisp source DEFCLASS/DEFMETHOD/accessor SETF use the real reader/compiler/task path', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (let ((initial 4))
      (defclass a () ((x :initarg :x :initform initial :accessor ax))))
    (defclass b (a) ())
    (defmethod score ((object a)) (ax object))
    (defmethod score ((object b)) (+ 10 (call-next-method)))
    (let ((object (make-instance 'b :x 7)))
      (list (score object) (setf (ax object) 8) (score object)
            (ax (make-instance 'a)) (typep object 'a)))
  `);
  assert.deepEqual(toArray(result.values[0]), [17, 8, 18, 4, T]);
});

test('Lisp source method combination preserves ordering and all primary values', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defparameter *events* nil)
    (defclass a () ())
    (defclass b (a) ())
    (defgeneric score (object))
    (defmethod score :before ((object a)) (push 2 *events*))
    (defmethod score :before ((object b)) (push 1 *events*))
    (defmethod score ((object a)) (push 4 *events*) (values 41 42))
    (defmethod score ((object b)) (push 3 *events*) (call-next-method))
    (defmethod score :after ((object a)) (push 5 *events*))
    (defmethod score :after ((object b)) (push 6 *events*))
    (defmethod score :around ((object a))
      (push 0 *events*)
      (multiple-value-prog1 (call-next-method) (push 7 *events*)))
    (multiple-value-bind (first second) (score (make-instance 'b))
      (list first second (reverse *events*)))
  `);
  const [first, second, events] = toArray(result.values[0]);
  assert.deepEqual([first, second], [41, 42]);
  assert.deepEqual(toArray(events), [0, 1, 2, 3, 4, 5, 6, 7]);
});

test('Lisp source initializer hooks and EQL specializers keep lexical state', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass a () ((x :initform 1 :accessor ax)) (:default-initargs :tag 5))
    (defmethod initialize-instance :after ((object a) &key tag)
      (setf (ax object) (+ (ax object) tag)))
    (defmethod shared-initialize :before ((object a) slots &rest initargs)
      (declare (ignore slots initargs)) (setf (ax object) 10))
    (defgeneric score (object value))
    (defmethod score ((object a) value) (+ (ax object) value))
    (let ((selected 3))
      (defmethod score ((object a) (value (eql selected))) (+ 100 (call-next-method))))
    (let ((object (make-instance 'a))) (list (ax object) (score object 3) (score object 4)))
  `);
  assert.deepEqual(toArray(result.values[0]), [15, 118, 19]);
});

test('forward class identities resolve and class redefinition is visible from Lisp source', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass child (parent) ((x :initform 1 :accessor x)))
    (defparameter *forward* (find-class 'parent))
    (defclass parent () ((y :initform 2 :accessor y)))
    (defparameter *object* (make-instance 'child))
    (setf (x *object*) 9)
    (defclass parent () ((z :initform 3 :accessor z)))
    (list (eq *forward* (find-class 'parent)) (x *object*) (z *object*)
          (slot-exists-p *object* 'y) (class-name (class-of *object*)))
  `);
  const values = toArray(result.values[0]);
  assert.deepEqual(values.slice(0, 4), [T, 9, 3, NIL]);
  assert.equal(values[4], rt.intern('CHILD'));
});

test('compiled installer ABI forwards next-method functions and canonical class records', () => {
  const rt = createRuntime(), name = rt.intern('COMPILED'), value = rt.intern('VALUE');
  const key = n => rt.intern(n, 'KEYWORD');
  const slot = fromArray([key('NAME'), value, key('INITARGS'), NIL, key('INITFUNCTION'), native(() => 5), key('INITFORM'), 5]);
  const cls = rt.call('ZK-MOP:ENSURE-CLASS', [name, key('DIRECT-SLOTS'), fromArray([slot])]).values[0];
  assert.equal(rt.call('SLOT-VALUE', [rt.call('MAKE-INSTANCE', [cls]).values[0], value]).values[0], 5);
  const genericName = rt.intern('COMPILED-GENERIC'), params = fromArray([value]);
  const install = (id, args) => rt.call(native(rt.installers.get(id)), args).values[0];
  const gf = install(9, [genericName, params, NIL, fromArray([rt.intern('STANDARD', 'COMMON-LISP')]), NIL]);
  install(10, [genericName, NIL, fromArray([T]), params, native(() => rt.multipleValues(8, 9)), NIL, NIL]);
  install(10, [genericName, NIL, fromArray([name]), params, native((args, r, task) => {
    assert.equal(args[1], T); return r.call(args[0], [], task);
  }), NIL, NIL]);
  assert.deepEqual(rt.call(gf, [rt.call('MAKE-INSTANCE', [cls]).values[0]]).values, [8, 9]);
});

test('WITH-SLOTS and WITH-ACCESSORS expand into assignable lexical symbol macros', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass a () ((x :initform 1 :accessor ax)))
    (let ((object (make-instance 'a)) (evaluations 0))
      (with-slots ((local x)) (progn (incf evaluations) object)
        (incf local 3)
        (with-accessors ((another ax)) object
          (setf another (+ another 5))))
      (list evaluations (ax object) (slot-exists-p 'a 'x)))
  `);
  assert.deepEqual(toArray(result.values[0]), [1, 9, NIL]);
});

test('next-method closures have lexical indefinite extent and methods have implicit blocks', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass a () ()) (defclass b (a) ())
    (defmethod save-next ((object a)) (return-from save-next (values 7 8)))
    (defmethod save-next ((object b)) (lambda () (call-next-method)))
    (let ((saved (save-next (make-instance 'b)))) (funcall saved))
  `);
  assert.deepEqual(result.values, [7, 8]);
});

test('DEFSTRUCT creates independent structure classes, constructors, shallow copies and writers', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (let ((initial 3)) (defstruct point (x initial) (y 4)))
    (let* ((object (make-point :x 8)) (copy (copy-point object)))
      (setf (point-y object) 9)
      (list (point-x object) (point-y object) (point-y copy)
            (point-x (make-point)) (point-p copy)
            (typep object 'point) (typep object 'structure-object)
            (typep object 'standard-object)
            (class-name (class-of (class-of object)))))
  `);
  const values = toArray(result.values[0]);
  assert.deepEqual(values.slice(0, 8), [8, 9, 4, 3, T, T, T, NIL]);
  assert.equal(values[8].name, 'STRUCTURE-CLASS');
});

test('DEFSTRUCT inclusion, BOA constructors and inherited read-only slots', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defstruct base (x 1) (tag 'fixed :read-only t))
    (defstruct (child (:include base (x 5))
                      (:constructor child-boa (x &optional y &aux (extra (+ x y))))
                      (:constructor make-child))
      (y 2) extra)
    (let* ((object (child-boa 7)) (copy (copy-base object)))
      (setf (base-x object) 11)
      (list (child-x object) (child-y object) (child-extra object)
            (base-x copy) (base-p object) (child-p object)
            (child-x (make-child))))
  `);
  assert.deepEqual(toArray(result.values[0]), [11, 2, 9, 7, T, T, 5]);
  assert.throws(() => rt.evalString(`(setf (base-tag (make-child)) 'changed)`), /Undefined function|read-only/);
});

test('typed structures use their declared list/vector representations and offsets', () => {
  const rt = createRuntime();
  const listResult = rt.evalString(`
    (defstruct (base (:type list) :named) (x 1))
    (defstruct (child (:include base (x 4)) (:type list) :named (:initial-offset 1)) (y 2))
    (let ((object (make-child)))
      (setf (base-x object) 9)
      (list object (child-x object) (child-y object) (base-p object) (child-p object)))
  `);
  const values = toArray(listResult.values[0]);
  const layout = toArray(values[0]);
  assert.equal(layout[0], rt.intern('BASE')); assert.equal(layout[3], rt.intern('CHILD'));
  assert.deepEqual([layout[1], layout[2], layout[4]], [9, NIL, 2]);
  assert.deepEqual(values.slice(1), [9, 2, T, T]);
  const vectorResult = rt.evalString(`
    (defstruct (coordinate (:type vector) :named (:initial-offset 2)) (x 10))
    (let ((object (make-coordinate)))
      (setf (coordinate-x object) 12)
      (list (length object) (coordinate-x object) (coordinate-p object)
            (coordinate-x (copy-coordinate object)) (vectorp object)))
  `);
  assert.deepEqual(toArray(vectorResult.values[0]), [4, 12, T, 12, T]);
});

test('compiled structure installer ABI preserves slot indices and read-only metadata', () => {
  const rt = createRuntime(), name = rt.intern('NATIVE-STRUCT'), x = rt.intern('X'), reader = rt.intern('NATIVE-X');
  const metadata = fromArray([fromArray([x, 1, T, reader, NIL, T])]);
  rt.installers.get(3)([name, NIL, metadata, NIL, NIL]);
  const object = rt.installers.get(4)([name, 4]);
  assert.equal(rt.installers.get(5)([name, object]), T);
  assert.equal(rt.installers.get(7)([object, name, 0]), 4);
  assert.equal(rt.installers.get(8)([object, name, 0, 9]), 9);
  const copy = rt.installers.get(6)([name, object]);
  rt.installers.get(8)([object, name, 0, 10]);
  assert.equal(rt.installers.get(7)([copy, name, 0]), 9);
  assert.equal(rt.classOf(rt.classOf(object)).name.name, 'STRUCTURE-CLASS');
});

test('implicit generics accept different method keyword sets and reject ordinary function collisions', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defmethod choose ((object integer) &key first) (+ object first))
    (defmethod choose ((object string) &key second) (+ (length object) second))
    (list (choose 4 :first 3) (choose "abc" :second 8))
  `);
  assert.deepEqual(toArray(result.values[0]), [7, 11]);
  assert.throws(() => rt.evalString('(defmethod car ((object integer)) object)'), /ordinary function/);
});

test('class bindings can change while old instances and superclass edges retain class identity', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass parent () ((x :initform 1)))
    (defclass child (parent) ())
    (let ((old (find-class 'parent)) (object (make-instance 'child)))
      (setf (find-class 'parent) nil)
      (defclass parent () ((x :initform 2)))
      (list (eq old (find-class 'parent))
            (typep object old) (typep object (find-class 'parent))
            (slot-value object 'x)
            (class-name (class-of (make-instance 'standard-object)))))
  `);
  const values = toArray(result.values[0]);
  assert.deepEqual(values.slice(0, 4), [NIL, T, NIL, 1]);
  assert.equal(values[4].name, 'STANDARD-OBJECT');
});

test('leftmost supplied initarg wins across aliases of the same slot', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass aliases () ((value :initarg :first :initarg :second :accessor value)))
    (value (make-instance 'aliases :second 7 :first 8))
  `);
  assert.deepEqual(result.values, [7]);
});

test('metaclass and effective slot methods control layout, initialization and accessors', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass tracked-meta (standard-class)
      ((annotation :initarg :annotation :reader class-annotation)))
    (defmethod zk-mop:validate-superclass ((class tracked-meta) (parent standard-class)) t)
    (defclass tracked-direct (zk-mop:standard-direct-slot-definition)
      ((marker :initarg :marker :reader direct-marker)))
    (defclass tracked-effective (zk-mop:standard-effective-slot-definition) ())
    (defmethod zk-mop:direct-slot-definition-class ((class tracked-meta) &rest args)
      (find-class 'tracked-direct))
    (defmethod zk-mop:effective-slot-definition-class ((class tracked-meta) &rest args)
      (find-class 'tracked-effective))
    (defmethod zk-mop:compute-slots ((class tracked-meta))
      (reverse (call-next-method)))
    (defmethod (setf zk-mop:slot-value-using-class) :around
        (value (class tracked-meta) object (slot tracked-effective))
      (if (eq (zk-mop:slot-definition-name slot) 'left-slot)
          (call-next-method (+ value 10) class object slot)
          (call-next-method)))
    (defclass tracked ()
      ((left-slot :initarg :left :type integer :marker :a :marker :b :accessor tracked-left)
       (right-slot :initform 7))
      (:metaclass tracked-meta) (:annotation . 19) (:default-initargs :left 5)
      (:documentation "Metadata is available in the class object."))
    (let* ((class (find-class 'tracked)) (object (make-instance class))
           (direct (first (zk-mop:class-direct-slots class)))
           (effective (second (zk-mop:class-slots class)))
           (reader (find-method #'tracked-left nil (list class))))
      (values
        (eq (class-of class) (find-class 'tracked-meta))
        (class-annotation class)
        (equal (direct-marker direct) '(:a :b))
        (eq (zk-mop:accessor-method-slot-definition reader) direct)
        (eq (class-of effective) (find-class 'tracked-effective))
        (eq (zk-mop:slot-definition-type effective) 'integer)
        (zk-mop:slot-definition-location effective)
        (zk-mop:standard-instance-access object 0)
        (tracked-left object)
        (progn (setf (tracked-left object) 8) (slot-value object 'left-slot))
        (documentation 'tracked 'type)))
  `);
  assert.deepEqual(result.values.slice(0, 10), [T, 19, T, T, T, T, 1, 7, 15, 18]);
  assert.equal(rt.toJS(result.values[10]), 'Metadata is available in the class object.');
});

test('a suspended finalizer exposes its draft to the prototype while keeping published instances intact', async () => {
  const rt = createRuntime();
  let entered, resume;
  const waiting = new Promise(resolve => { entered = resolve; });
  const gate = new Promise(resolve => { resume = resolve; });
  rt.registerFunction('PAUSE-FINALIZATION', () => { entered(); return gate; });
  rt.evalString(`
    (defvar *pause-finalization* nil)
    (defclass pausing-meta (standard-class) ())
    (defmethod zk-mop:validate-superclass ((class pausing-meta) (parent standard-class)) t)
    (defmethod zk-mop:finalize-inheritance :after ((class pausing-meta))
      (when *pause-finalization*
        (unless (slot-exists-p (zk-mop:class-prototype class) 'new-slot)
          (error "Prototype still has the old layout"))
        (pause-finalization)
        (throw 'cancel-change :cancelled)))
    (defclass pausing () ((old-slot :initarg :old)) (:metaclass pausing-meta))
    (defparameter *existing* (make-instance 'pausing :old 42))
  `);
  const changing = rt.evalStringAsync(`
    (let ((*pause-finalization* t))
      (catch 'cancel-change
        (defclass pausing () ((new-slot :initform 17)) (:metaclass pausing-meta))))
  `);
  try {
    await Promise.race([waiting, changing.then(() => { throw new Error('Finalizer did not suspend'); })]);
    assert.deepEqual(rt.evalString("(values (slot-value *existing* 'old-slot) (slot-exists-p *existing* 'new-slot))").values, [42, NIL]);
  } finally { resume(NIL); }
  assert.equal((await changing).values[0], rt.read(':cancelled'));
  assert.deepEqual(rt.evalString("(values (slot-value *existing* 'old-slot) (slot-exists-p *existing* 'new-slot))").values, [42, NIL]);
  assert.deepEqual(rt.evalString(`
    (defclass pausing () ((old-slot) (new-slot :initform 17)) (:metaclass pausing-meta))
    (values (slot-value *existing* 'old-slot) (slot-value *existing* 'new-slot))
  `).values, [42, 17]);
});

test('method lambda and effective method protocols carry custom initialization and call arguments', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass decorated-generic (standard-generic-function)
      ((decoration :initform 90 :reader generic-decoration))
      (:metaclass zk-mop:funcallable-standard-class))
    (defclass decorated-method (standard-method) ((tag :initarg :tag :reader method-tag)))
    (defgeneric decorated (value)
      (:generic-function-class decorated-generic) (:method-class decorated-method))
    (defmethod zk-mop:make-method-lambda ((gf decorated-generic) (method decorated-method) expression environment)
      (declare (ignore environment))
      (values \`(lambda (arguments next extra)
                (declare (ignore next))
                (values (apply (function ,expression) arguments) extra))
              '(:tag :transformed)))
    (defmethod zk-mop:compute-effective-method ((gf decorated-generic) combination methods)
      (declare (ignore combination))
      (list 'call-method (first methods) nil (generic-decoration gf)))
    (let ((captured 3)) (defmethod decorated ((value integer)) (+ value captured)))
    (let* ((gf #'decorated) (method (first (zk-mop:generic-function-methods gf))))
      (multiple-value-bind (value extra) (decorated 5)
        (values value extra (method-tag method)
                (eq (class-of method) (find-class 'decorated-method))
                (eq (zk-mop:method-generic-function method) gf)
                (eq (find-method gf nil (list (find-class 'integer))) method))))
  `);
  assert.deepEqual(result.values, [8, 90, rt.read(':transformed'), T, T, T]);
});

test('class-only applicability reports uncertainty only for possible EQL matches and uses the discriminator', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass dispatch-meta (standard-generic-function) ()
      (:metaclass zk-mop:funcallable-standard-class))
    (defmethod zk-mop:compute-discriminating-function ((gf dispatch-meta))
      (let ((standard (call-next-method)))
        (lambda (x y) (if (eq x :override) y (funcall standard x y)))))
    (defgeneric dispatch-probe (x y) (:generic-function-class dispatch-meta))
    (defmethod dispatch-probe (x y) :default)
    (defmethod dispatch-probe ((x (eql 2)) (y symbol)) :specific)
    (let ((classes (list (find-class 'integer) (find-class 'integer)))
          (gf #'dispatch-probe))
      (multiple-value-bind (methods known) (zk-mop:compute-applicable-methods-using-classes gf classes)
        (multiple-value-bind (possible uncertain)
            (zk-mop:compute-applicable-methods-using-classes gf (list (find-class 'integer) (find-class 'symbol)))
          (let* ((spec (zk-mop:intern-eql-specializer 2))
                 (method (find-method gf nil (list spec (find-class 'symbol))))
                 (indexed (member method (zk-mop:specializer-direct-methods spec))))
            (values (length methods) known possible uncertain (not (null indexed))
                    (dispatch-probe 2 nil) (dispatch-probe :override 81)
                    (progn (remove-method gf method) (zk-mop:specializer-direct-methods spec)))))))
  `);
  assert.deepEqual(result.values, [1, T, NIL, NIL, T, rt.read(':specific'), 81, NIL]);
});

test('TYPE-OF follows proper class bindings and structure slots use the reflected storage', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defstruct named-record (value 0))
    (let* ((object (make-named-record :value 42)) (class (class-of object))
           (slot (first (zk-mop:class-slots class))))
      (setf (class-name class) 'renamed-record)
      (let ((anonymous (type-of object)))
        (setf (find-class 'renamed-record) class)
        (values (eq anonymous class) (typep object anonymous)
                (eq (type-of object) 'renamed-record)
                (zk-mop:slot-value-using-class class object slot)
                (progn (slot-makunbound object 'value) (slot-boundp object 'value))
                (progn (setf (slot-value object 'value) 13) (named-record-value object)))))
  `);
  assert.deepEqual(result.values, [T, T, T, 42, NIL, 13]);
});

test('TYPEP and SUBTYPEP resolve class names by symbol identity', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defpackage :own (:use :cl) (:shadow :vector :t))
    (defclass own::vector () ())
    (defclass own::t () ())
    (let ((object (make-instance 'own::vector)))
      (values (typep object 'own::vector) (typep object 'cl:vector)
              (typep #() (find-class 'cl:vector))
              (typep #() 'own::vector) (subtypep 'own::vector 'own::t)))
  `);
  assert.deepEqual(result.values, [T, NIL, T, NIL, NIL]);
});

test('DEFSTRUCT printer options create inheritable PRINT-OBJECT methods', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defstruct (painted (:print-function (lambda (object stream depth)
                                        (format stream "~D/~D" (painted-value object) depth)))) (value 12))
    (defstruct (inherited (:include painted)) extra)
    (defstruct (stock (:include painted) (:print-object)) extra)
    (values (prin1-to-string (make-painted)) (prin1-to-string (make-inherited))
            (prin1-to-string (make-stock))
            (let ((*print-level* 1)) (prin1-to-string (list (make-painted)))))
  `);
  assert.deepEqual(result.values.map(value => rt.toJS(value)), ['12/0', '12/0', '#S(STOCK :VALUE 12 :EXTRA NIL)', '(#)']);
});

test('top-level DEFCLASS is visible in the file environment before its hooks run', () => {
  const rt = createRuntime();
  rt.evalString('(defparameter *class-slot-init* 0)');
  const compiled = rt.compileString(`
    (defclass compile-base () ((slot :allocation :class :initform (incf *class-slot-init*))))
    (defclass compile-child (compile-base) ())
    (defmacro known-superclass (&environment env)
      (let ((super (first (zk-mop:class-direct-superclasses (find-class 'compile-child t env)))))
        (list 'quote (list (class-name super) (typep nil 'compile-child env)
                          (subtypep 'compile-child 'compile-base env)))))
    (values-list (append (known-superclass) (list *class-slot-init*)))
  `);
  assert.deepEqual(rt.evalString("(values (find-class 'compile-child nil) *class-slot-init*)").values, [NIL, 0]);
  assert.deepEqual(rt.call(compiled).values, [rt.intern('COMPILE-BASE'), NIL, T, 1]);
});

test('compatible generic function classes migrate in place and refresh the discriminator', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass decorated-function (standard-generic-function)
      ((increment :initform 10 :reader increment)) (:metaclass zk-mop:funcallable-standard-class))
    (defmethod zk-mop:compute-discriminating-function ((gf decorated-function))
      (let ((default (call-next-method)))
        (lambda (x) (+ (funcall default x) (increment gf)))))
    (defgeneric redecorated (x))
    (defmethod redecorated ((x integer)) (1+ x))
    (let* ((gf #'redecorated) (method (first (zk-mop:generic-function-methods gf))))
      (defgeneric redecorated (x) (:generic-function-class decorated-function))
      (values (eq gf #'redecorated) (eq method (first (zk-mop:generic-function-methods gf)))
              (eq (class-of gf) (find-class 'decorated-function)) (redecorated 7)))
  `);
  assert.deepEqual(result.values, [T, T, T, 18]);
});

test('custom specializers and combination objects participate in generic invocation', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass parity-specializer (zk-mop:specializer) ())
    (defclass parity-generic (standard-generic-function) ()
      (:metaclass zk-mop:funcallable-standard-class))
    (defclass parity-combination (method-combination) ())
    (defmethod zk-mop:find-method-combination ((gf parity-generic) name options)
      (if (eq name 'alternating) (make-instance 'parity-combination) (call-next-method)))
    (defmethod zk-mop:compute-applicable-methods-using-classes ((gf parity-generic) classes)
      (values nil nil))
    (defmethod compute-applicable-methods ((gf parity-generic) args)
      (remove-if-not (lambda (method)
                       (let ((specializer (first (zk-mop:method-specializers method))))
                         (if (typep specializer 'parity-specializer)
                             (evenp (first args))
                             (typep (first args) specializer))))
                     (zk-mop:generic-function-methods gf)))
    (defmethod zk-mop:compute-effective-method ((gf parity-generic) (combination parity-combination) methods)
      (list 'list ''adapted (list 'call-method (car (last methods)))))
    (defgeneric parity (value) (:generic-function-class parity-generic) (:method-combination alternating))
    (defmethod parity (value) :odd)
    (let* ((specializer (make-instance 'parity-specializer))
           (method (make-instance 'standard-method :lambda-list '(value) :specializers (list specializer)
                     :function (lambda (args next) :even))))
      (add-method #'parity method)
      (values (equal (parity 2) '(adapted :even)) (equal (parity 3) '(adapted :odd))
              (eq (find-method #'parity nil (list specializer)) method)
              (equal (zk-mop:specializer-direct-generic-functions specializer) (list #'parity))
              (progn (remove-method #'parity method) (zk-mop:specializer-direct-methods specializer))))
  `);
  assert.deepEqual(result.values, [T, T, T, T, NIL]);
});

test('replacing an inline method function preserves the method definition owner', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass replacing-method (standard-method) ())
    (defmethod initialize-instance :around ((method replacing-method) &rest args)
      (apply #'call-next-method method :function (lambda (args next) :substitute) args))
    (defgeneric replace-inline (x) (:method-class replacing-method) (:method (x) :body))
    (let ((before (replace-inline 7)) (old (first (zk-mop:generic-function-methods #'replace-inline))))
      (defgeneric replace-inline (x y))
      (values before (zk-mop:method-generic-function old)
              (zk-mop:generic-function-methods #'replace-inline)))
  `);
  assert.deepEqual(result.values, [rt.read(':substitute'), NIL, NIL]);
});

test('generic function options, class and EQL designators and generalized APPLY places agree', () => {
  const rt = createRuntime();
  const result = rt.evalString(`
    (defclass found () ())
    (defgeneric (setf survey) (value target &rest indices)
      (:method (value (target array) &rest indices)
        (declare (optimize speed)) "Writes through a named place." (declare (optimize safety))
        (setf (apply #'aref target indices) value)))
    (defmethod matches ((value (eql 1234567890))) t)
    (let* ((class (find-class 'found)) (array (make-array 3 :initial-element nil))
           (gf (ensure-generic-function '(setf survey) :declare '((optimize space))))
           (method (find-method gf nil '(t array))))
      (values (setf (survey array 1) 78) (aref array 1)
              (eq class (setf (find-class 'found t nil) class))
              (eq method (first (zk-mop:generic-function-methods gf)))
              (eq (find-method #'matches nil '((eql 1234567890)))
                  (first (zk-mop:generic-function-methods #'matches)))
              (documentation method t)))
  `);
  assert.deepEqual(result.values.slice(0, 5), [78, 78, T, T, T]);
  assert.equal(rt.toJS(result.values[5]), 'Writes through a named place.');
});
