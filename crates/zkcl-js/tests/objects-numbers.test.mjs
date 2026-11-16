import test from 'node:test';
import assert from 'node:assert/strict';
import { NIL, T, UNBOUND, LispSymbol, Cons, Cell, LispCode, Closure, LispCharacter, LispString, LispArray, list, toArray, eq, eql, equal } from '../runtime/objects.mjs';
import { FIXNUM_MIN, FIXNUM_MAX, integer, SingleFloat, DoubleFloat, Ratio, Complex, makeRatio, makeComplex, add, subtract, multiply, divide, negate, compare, numericEqual, rationalParts, coerceFloat, singleFromBits, doubleFromBits, quotientRemainder, NumericError } from '../runtime/numbers.mjs';

test('NIL, T and unbound are distinct Lisp objects; symbol slots are separate', () => {
  assert.notEqual(NIL, null);
  assert.notEqual(NIL, false);
  assert.notEqual(NIL, T);
  assert.notEqual(UNBOUND, NIL);
  assert.ok(NIL instanceof LispSymbol);
  assert.equal(NIL.value, NIL);
  assert.equal(T.value, T);
  const a = new LispSymbol('A'), b = new LispSymbol('A');
  assert.equal(a.plist, NIL);
  assert.equal(a.value, UNBOUND);
  assert.equal(a.function, UNBOUND);
  a.value = 7;
  a.function = b;
  assert.equal(a.value, 7);
  assert.equal(a.function, b);
  assert.equal(b.value, UNBOUND);
  assert.equal(eq(a, b), false);
});

test('shared mutable lexical cells survive closure capture by identity', () => {
  const cell = new Cell(1);
  const code = new LispCode({ step() {} });
  const closure = new Closure(code, [cell]);
  const other = new Closure(code, [cell]);
  cell.value = 9;
  assert.equal(closure.environment[0].value, 9);
  assert.equal(other.environment[0], cell);
  assert.equal(eq(closure, other), false);
  assert.equal(closure.code, other.code);
  assert.equal(new LispCode({ instructions: [] }).entry.instructions.length, 0);
});

test('cons identity, list validation, and cyclic structural equality', () => {
  const a = list(1, 2, 3), b = list(1, 2, 3);
  assert.deepEqual(toArray(a), [1, 2, 3]);
  assert.equal(eql(a, b), false);
  assert.equal(equal(a, b), true);
  assert.throws(() => toArray(new Cons(1, 2)), /proper list/);
  const x = new Cons(1), y = new Cons(1);
  x.cdr = x;
  y.cdr = y;
  assert.equal(equal(x, y), true);
  y.car = 2;
  assert.equal(equal(x, y), false);
  assert.throws(() => toArray(x), /acyclic/);
});

test('characters and mutable strings index Unicode scalar values', () => {
  const text = new LispString('A😀B');
  assert.equal(text.length, 3);
  assert.equal(text.get(1).codePoint, 0x1f600);
  text.set(1, new LispCharacter('猫'));
  assert.equal(text.toString(), 'A猫B');
  assert.equal(eql(new LispCharacter('猫'), new LispCharacter(0x732b)), true);
  assert.equal(equal(text, new LispString('A猫B')), true);
  assert.equal(eql(text, new LispString('A猫B')), false);
  assert.throws(() => new LispCharacter(0xd800), /scalar/);
  assert.throws(() => new LispString('\ud800'), /scalar/);
  assert.throws(() => text.get(3), /bounds/);
  assert.throws(() => text.set(0, 'x'), /character/);
});

test('arrays provide checked row-major storage, rank zero, and displacement', () => {
  const array = new LispArray([2, 3], { initialElement: NIL });
  assert.equal(array.totalSize, 6);
  array.aset(42, 1, 2);
  assert.equal(array.get(5), 42);
  assert.equal(array.aref(1, 2), 42);
  assert.throws(() => array.aref(1), /subscripts/);
  assert.throws(() => array.aref(1, 3), /bounds/);
  const scalar = new LispArray([], { initialElement: 7 });
  assert.equal(scalar.rank, 0);
  assert.equal(scalar.aref(), 7);
  const backing = new LispArray([5], { initialContents: [0, 1, 2, 3, 4] });
  const view = new LispArray([2], { displacedTo: backing, displacedIndexOffset: 2, fillPointer: 1 });
  assert.equal(view.length, 1);
  assert.equal(view.get(1), 3); // AREF ignores the fill pointer.
  view.set(0, 99);
  assert.equal(backing.get(2), 99);
  assert.throws(() => new LispArray([4], { displacedTo: backing, displacedIndexOffset: 2 }), /displaced/);
  assert.throws(() => new LispArray([2, 2], { fillPointer: 1 }), /fill pointer/);
});

test('array rank, dimension and total size limits use exact products', () => {
  assert.throws(() => new LispArray(Array(128).fill(0)), error => error.kind === 'TYPE-ERROR');
  assert.throws(() => new LispArray([0, 2 ** 32]), error => error.kind === 'TYPE-ERROR');
  assert.throws(() => new LispArray([65536, 65536]), error => error.kind === 'TYPE-ERROR');
  const empty = new LispArray([...Array(126).fill(2 ** 32 - 1), 0]);
  assert.equal(empty.rank, 127);
  assert.equal(empty.totalSize, 0);
});

test('integer normalization and multiplication never lose an integer bit', () => {
  assert.equal(integer(BigInt(FIXNUM_MIN)), FIXNUM_MIN);
  assert.equal(integer(BigInt(FIXNUM_MAX)), FIXNUM_MAX);
  assert.equal(integer(FIXNUM_MIN - 1), BigInt(FIXNUM_MIN) - 1n);
  assert.equal(integer(FIXNUM_MAX + 1), BigInt(FIXNUM_MAX) + 1n);
  assert.equal(integer('9007199254740993'), 9007199254740993n);
  assert.throws(() => integer(9007199254740993), NumericError);
  const product = BigInt(FIXNUM_MAX) * BigInt(FIXNUM_MAX);
  assert.equal(multiply(FIXNUM_MAX, FIXNUM_MAX), product);
  assert.equal(subtract(add(product, 1), product), 1);
  assert.equal(add(FIXNUM_MAX, 1), 268435456n);
  assert.equal(subtract(268435456n, 1), FIXNUM_MAX);
  assert.equal(negate(FIXNUM_MIN), 268435456n);
});

test('random states copy the full sequence and survive readable printing', async () => {
  const { createRuntime } = await import('../runtime/runtime.mjs');
  for (const executionStrategy of ['javascript', 'bytecode']) {
    const runtime = createRuntime({ executionStrategy });
    const independent = createRuntime({ executionStrategy });
    const initial = runtime.evalString('*random-state*').values[0];
    assert.notEqual(initial, independent.evalString('*random-state*').values[0]);
    const source = `(let* ((a (make-random-state t))
                           (*random-state* a)
                           (b (make-random-state nil))
                           (serialized (write-to-string (list a a) :readably t :circle t))
                           (restored (let ((*read-eval* nil) (*read-base* 36))
                                       (read-from-string serialized)))
                           (c (car restored)))
                      (and (random-state-p c) (typep c 'random-state)
                           (eq (type-of c) 'random-state)
                           (eq (class-of c) (find-class 'random-state))
                           (eq c (cadr restored)) (not (eq a b)) (not (eq b c))
                           (loop repeat 24 always
                             (let* ((limit (1+ (ash 1 137)))
                                    (n (random limit))
                                    (single (random most-positive-single-float a))
                                    (double (random least-positive-double-float a))
                                    (fixnum (random 37 a)))
                               (and (<= 0 n) (< n limit) (= n (random limit b) (random limit c))
                                    (typep single 'single-float)
                                    (< single most-positive-single-float)
                                    (eql single (random most-positive-single-float b))
                                    (eql single (random most-positive-single-float c))
                                    (eql double 0d0)
                                    (eql double (random least-positive-double-float b))
                                    (eql double (random least-positive-double-float c))
                                    (typep fixnum 'fixnum)
                                    (= fixnum (random 37 b) (random 37 c)))))))`;
    assert.equal(runtime.evalString(source).values[0], T);
    assert.equal(runtime.evalString('*random-state*').values[0], initial);
    assert.equal(runtime.evalString('(random (ash 1 137))').values[0],
      independent.evalString('(random (ash 1 137))').values[0]);
    for (const invalid of ['(random 0)', '(random 1/2)', '(random 1 nil)', '(make-random-state :invalid)'])
      assert.throws(() => runtime.evalString(invalid), error => error.className === 'TYPE-ERROR');
    assert.throws(() => runtime.evalString(`#Y"${'0'.repeat(64)}"`), error => error.className === 'READER-ERROR');
  }
});

test('float decomposition preserves signed zero, subnormal precision, and bignum exponents', async () => {
  const { createRuntime } = await import('../runtime/runtime.mjs');
  const runtime = createRuntime();
  const source = `(and (eq (type-of (float 0d0)) 'double-float)
                      (eql (float-sign -0.0f0 0d0) -0.0d0)
                      (= (float-precision -0d0) 0)
                      (= (float-precision least-positive-double-float) 1)
                      (= (float-digits least-positive-double-float) 53)
                      (equal (multiple-value-list (integer-decode-float least-positive-double-float))
                             '(4503599627370496 -1126 1))
                      (equal (multiple-value-list (decode-float -0.125f0)) '(0.5f0 -2 -1.0f0))
                      (eql (scale-float -1.0f0 (- (ash 1 128))) -0.0f0)
                      (eql (scale-float least-positive-double-float 1074) 1d0)
                      (eql (scale-float least-positive-single-float -1) 0f0)
                      (eql (scale-float (* 3 least-positive-single-float) -1)
                           (* 2 least-positive-single-float))
                      (= (rationalize 0.1d0) 1/10)
                      (eql (float (rationalize least-positive-double-float) 0d0)
                           least-positive-double-float)
                      (handler-case (scale-float 1d0 (ash 1 128))
                        (floating-point-overflow (c)
                          (eq (arithmetic-error-operation c) 'scale-float))))`;
  assert.equal(runtime.evalString(source).values[0], T);
});

test('byte field places update the original integer with one evaluation per subform', async () => {
  const { createRuntime } = await import('../runtime/runtime.mjs');
  const runtime = createRuntime();
  const source = `(let ((numbers (vector -1)) (visits '()) (huge (ash 1 80)))
                    (and (= 32 (setf (ldb (progn (push :byte visits) (byte 4 2))
                                          (aref numbers (progn (push :index visits) 0)))
                                     (progn (push :new visits) 32)))
                         (= (aref numbers 0) -61)
                         (equal visits '(:new :index :byte))
                         (= #x18 (setf (mask-field (byte 4 2) (aref numbers 0)) #x18))
                         (= (aref numbers 0) -37)
                         (= (boole boole-andc2 -37 78) (logandc2 -37 78))
                         (= (ldb (byte huge huge) 5) 0)
                         (ldb-test (byte huge huge) -1)
                         (logbitp huge -1)))`;
  assert.equal(runtime.evalString(source).values[0], T);
});

test('transcendental functions select principal complex branches and preserve numeric formats', async () => {
  const { createRuntime } = await import('../runtime/runtime.mjs');
  const runtime = createRuntime();
  const source = `(let* ((tiny (/ (ash 1 1100)))
                         (root (sqrt #c(-4.0d0 -0.0d0)))
                         (inverse (asin 2.0d0))
                         (sin-i (sin #c(0.0d0 1.0d0))))
                    (and (eql root #c(0.0d0 2.0d0))
                         (eql (sqrt -4) #c(0.0f0 2.0f0))
                         (minusp (imagpart inverse))
                         (< (abs (- (sin inverse) 2.0d0)) 1d-14)
                         (= (realpart sin-i) 0)
                         (eql (imagpart sin-i) (sinh 1.0d0))
                         (= (log (ash 1 5000) 2) 5000)
                         (= (log (+ 1 tiny) (+ 1 (/ tiny 2))) 2)
                         (= (abs #c(3 4)) 5)
                         (eql (signum #c(3 4)) #c(3/5 4/5))
                         (eql (expt 3.0d0 0) 1.0d0)
                         (eql (expt most-positive-double-float 1) most-positive-double-float)
                         (eql (expt 2 -5) 1/32)
                         (handler-case (log 0)
                           (division-by-zero (c)
                             (equal (arithmetic-error-operands c) '(0))))))`;
  assert.equal(runtime.evalString(source).values[0], T);
});

test('complex type specifiers follow the upgraded component representation', async () => {
  const { createRuntime } = await import('../runtime/runtime.mjs');
  const runtime = createRuntime();
  const source = `(and (eq (upgraded-complex-part-type '(integer 0 7)) 'rational)
                      (eq (upgraded-complex-part-type '(and real (not rational))) 'float)
                      (eq (upgraded-complex-part-type (find-class 'double-float)) 'double-float)
                      (eq (upgraded-complex-part-type '(or short-float long-float)) 'float)
                      (handler-case (upgraded-complex-part-type '#:single-float)
                        (error () t))
                      (typep #c(3/5 4/5) '(complex integer))
                      (not (typep #c(3d0 4d0) '(complex single-float)))
                      (typep #c(3d0 4d0) (type-of #c(3d0 4d0)))
                      (eq (type-of most-negative-fixnum) 'fixnum)
                      (eq (type-of (1- most-negative-fixnum)) 'bignum)
                      (eq (class-of (1- most-negative-fixnum)) (find-class 'integer))
                      (equal (multiple-value-list (subtypep '(complex long-float) '(complex float))) '(t t)))`;
  assert.equal(runtime.evalString(source).values[0], T);
});

test('specialized arrays validate initial values and writes at their boundary', () => {
  const bits = new LispArray([3], { elementType: 'Bit', initialContents: [1, 0, 1] });
  assert.equal(bits.elementType, 'BIT');
  assert.throws(() => bits.set(1, 2), error => error.kind === 'TYPE-ERROR');
  assert.throws(() => new LispArray([1], { elementType: 'BIT', initialElement: NIL }), error => error.kind === 'TYPE-ERROR');
  assert.throws(() => new LispArray([2], { elementType: 'Octet', initialContents: [0, 256] }), error => error.kind === 'TYPE-ERROR');
  const single = new LispArray([2], { elementType: 'SingleFloat' });
  assert.ok(single.get(0) instanceof SingleFloat);
  assert.throws(() => single.set(0, new DoubleFloat(1)), error => error.kind === 'TYPE-ERROR');
  const octets = new LispArray([1], { elementType: list(new LispSymbol('UNSIGNED-BYTE'), 8), initialElement: 255 });
  assert.equal(octets.elementKind, 'octet');
  const text = new LispString('AB猫');
  const view = new LispArray([2], { elementType: 'CHARACTER', displacedTo: text, displacedIndexOffset: 1 });
  view.set(0, new LispCharacter('C'));
  assert.equal(text.toString(), 'AC猫');
  assert.throws(() => new LispArray([1], { elementType: 'BIT', displacedTo: text }), /same upgraded/);
});

test('object boundary errors reach Lisp type handlers through builtin and host calls', async () => {
  const { createRuntime } = await import('../runtime/runtime.mjs');
  const runtime = createRuntime();
  runtime.defineBuiltin('INVALID-OBJECT-WRITE', () => new LispString('x').set(0, 7));
  for (const source of [
    "(handler-case (values-list '(1 . 2)) (type-error () t))",
    '(handler-case (make-array 2 :fill-pointer 3) (type-error () t))',
    '(handler-case (invalid-object-write) (type-error () t))',
  ]) {
    assert.equal(runtime.evalString(source).values[0], T, source);
  }
  for (const operation of [
    () => new LispCharacter(0xd800),
    () => new LispString('x').get(1),
    () => toArray(new Cons(1, 2)),
    () => coerceFloat(1, 'invalid'),
  ]) assert.throws(operation, error => error instanceof NumericError && error.kind === 'TYPE-ERROR');
});

test('EQUAL compares visible bit-vector contents and string array characters', () => {
  const bits = new LispArray([4], { elementType: 'BIT', initialContents: [1, 0, 1, 0], fillPointer: 3 });
  const other = new LispArray([3], { elementType: 'BIT', initialContents: [1, 0, 1] });
  assert.equal(equal(bits, other), true);
  assert.equal(eql(bits, other), false);
  other.set(2, 0);
  assert.equal(equal(bits, other), false);
  assert.equal(equal(new LispArray([1], { initialContents: [1] }), new LispArray([1], { initialContents: [1] })), false);
  const stringArray = new LispArray([2], { elementType: 'CHARACTER', initialContents: [new LispCharacter('A'), new LispCharacter('猫')] });
  assert.equal(equal(stringArray, new LispString('A猫')), true);
});

test('ratios reduce signs and promote arithmetic exactly', () => {
  const ratio = makeRatio(6, -8);
  assert.ok(ratio instanceof Ratio);
  assert.equal(ratio.numerator, -3);
  assert.equal(ratio.denominator, 4);
  assert.equal(makeRatio(0, 55), 0);
  assert.equal(makeRatio(12, 4), 3);
  assert.equal(add(makeRatio(1, 3), makeRatio(2, 3)), 1);
  assert.equal(multiply(makeRatio(2, 3), makeRatio(3, 2)), 1);
  assert.equal(divide(8, 2), 4);
  assert.ok(eql(divide(2, 3), makeRatio(2, 3)));
  assert.throws(() => makeRatio(1, 0), error => error.kind === 'DIVISION-BY-ZERO');
  assert.throws(() => divide(new SingleFloat(1), new SingleFloat(-0)), error => error.kind === 'DIVISION-BY-ZERO');
});

test('float format and signed-zero identity remain distinct under EQL', () => {
  const single = new SingleFloat(0.1), double = new DoubleFloat(0.1);
  assert.equal(single.value, Math.fround(0.1));
  assert.equal(eql(single, double), false);
  assert.equal(eql(new SingleFloat(1), new SingleFloat(1)), true);
  assert.equal(eql(new SingleFloat(0), new SingleFloat(-0)), false);
  assert.equal(numericEqual(new SingleFloat(0), new DoubleFloat(-0)), true);
  assert.equal(singleFromBits(single.bits).bits, single.bits);
  assert.equal(doubleFromBits(double.bits.toString()).bits, double.bits);
  assert.throws(() => new SingleFloat(2 ** 128), error => error.kind === 'FLOATING-POINT-OVERFLOW');
  assert.throws(() => new DoubleFloat(NaN), error => error.kind === 'FLOATING-POINT-INVALID-OPERATION');
});

test('numeric comparison uses exact represented floats rather than lossy coercion', () => {
  assert.equal(numericEqual(9007199254740993n, new DoubleFloat(9007199254740992)), false);
  assert.equal(compare(9007199254740993n, new DoubleFloat(9007199254740992)), 1);
  assert.equal(compare(-9007199254740993n, new DoubleFloat(-9007199254740992)), -1);
  assert.equal(numericEqual(makeRatio(1, 10), new DoubleFloat(0.1)), false);
  assert.equal(numericEqual(makeRatio(1, 2), new SingleFloat(0.5)), true);
  assert.equal(compare(makeRatio(1, 10), new DoubleFloat(0.1)), -1);
  assert.deepEqual(rationalParts(new DoubleFloat(Number.MIN_VALUE)), [1n, 1n << 1074n]);
});

test('rational to float conversion handles huge ratios, ties, and subnormals directly', () => {
  const huge = 1n << 2000n;
  assert.equal(coerceFloat(makeRatio(huge + 1n, huge), 'double').value, 1);
  const denominator = 1n << 80n;
  // Above a binary32 midpoint by a bit binary64 would discard.
  const above = makeRatio(denominator + (1n << 56n) + 1n, denominator);
  assert.equal(coerceFloat(above, 'single').value, 1 + 2 ** -23);
  assert.equal(coerceFloat(makeRatio((1n << 24n) + 1n, 1n << 24n), 'single').value, 1);
  assert.equal(coerceFloat(makeRatio(1n, 1n << 149n), 'single').bits, 1);
  assert.equal(coerceFloat(makeRatio(1n, 1n << 150n), 'single').bits, 0);
  assert.equal(coerceFloat(makeRatio(-1n, 1n << 150n), 'single').bits, 0x80000000);
  assert.equal(coerceFloat(makeRatio(1n, 1n << 1074n), 'double').value, Number.MIN_VALUE);
  assert.equal(coerceFloat(makeRatio((1n << 53n) + 1n, 1n << 53n), 'double').value, 1);
});

test('single arithmetic rounds each step and double contagion changes format', () => {
  const result = subtract(add(new SingleFloat(2 ** 24), new SingleFloat(1)), new SingleFloat(2 ** 24));
  assert.ok(result instanceof SingleFloat);
  assert.equal(result.value, 0);
  assert.ok(add(new SingleFloat(1), new DoubleFloat(2)) instanceof DoubleFloat);
  assert.equal(add(new SingleFloat(1), makeRatio(1, 2)).value, 1.5);
  assert.equal(divide(new SingleFloat(1), new SingleFloat(3)).value, Math.fround(1 / 3));
});

test('complex construction, contagion and arithmetic preserve CL canonical types', () => {
  assert.equal(makeComplex(5, 0), 5);
  const floating = makeComplex(5, new SingleFloat(-0));
  assert.ok(floating instanceof Complex);
  assert.ok(floating.real instanceof SingleFloat);
  assert.equal(floating.imaginary.bits, 0x80000000);
  assert.equal(eql(floating, new SingleFloat(5)), false);
  assert.equal(numericEqual(floating, 5), true);
  const a = makeComplex(1, 2), b = makeComplex(3, 4);
  assert.ok(eql(multiply(a, b), makeComplex(-5, 10)));
  assert.ok(eql(divide(a, b), makeComplex(makeRatio(11, 25), makeRatio(2, 25))));
  assert.equal(multiply(makeComplex(0, 1), makeComplex(0, 1)), -1);
  const mixed = add(a, new DoubleFloat(1));
  assert.ok(mixed.real instanceof DoubleFloat);
  assert.ok(mixed.imaginary instanceof DoubleFloat);
  assert.equal(eql(makeComplex(new SingleFloat(1), 2), makeComplex(1, new DoubleFloat(2))), false);
  assert.throws(() => compare(a, b), error => error.kind === 'TYPE-ERROR');
});

test('complex division remains finite when denominator squares overflow or underflow', () => {
  for (const magnitude of [1e200, 1e-200, Number.MIN_VALUE]) {
    const value = makeComplex(new DoubleFloat(magnitude), new DoubleFloat(magnitude));
    const result = divide(value, value);
    assert.equal(result.real.value, 1);
    assert.equal(result.imaginary.value, 0);
  }
  for (const magnitude of [Math.fround(1e30), Math.fround(1e-30), 2 ** -149]) {
    const value = makeComplex(new SingleFloat(magnitude), new SingleFloat(magnitude));
    const result = divide(value, value);
    assert.ok(result.real instanceof SingleFloat);
    assert.equal(result.real.value, 1);
    assert.equal(result.imaginary.value, 0);
  }
});

test('quotient/remainder operations use ties to even and exact remainders', () => {
  assert.deepEqual(quotientRemainder(-7, 3, 'floor'), [-3, 2]);
  assert.deepEqual(quotientRemainder(-7, 3, 'ceiling'), [-2, -1]);
  assert.deepEqual(quotientRemainder(-7, 3, 'truncate'), [-2, -1]);
  assert.deepEqual(quotientRemainder(5, 2, 'round'), [2, 1]);
  assert.deepEqual(quotientRemainder(7, 2, 'round'), [4, -1]);
  assert.deepEqual(quotientRemainder(-5, 2, 'round'), [-2, -1]);
  assert.deepEqual(quotientRemainder(-7, 2, 'round'), [-4, 1]);
  const [q, r] = quotientRemainder(new SingleFloat(5.5), 2, 'floor', true);
  assert.ok(q instanceof SingleFloat);
  assert.ok(r instanceof SingleFloat);
  assert.equal(q.value, 2);
  assert.equal(r.value, 1.5);
});
