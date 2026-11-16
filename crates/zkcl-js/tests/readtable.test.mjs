import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T, LispArray, toArray, Cons, DoubleFloat, SingleFloat } from '../runtime/runtime.mjs';
import { ReaderError } from '../runtime/reader.mjs';
const evaluate = (rt, source) => rt.evalString(source).values;

// CLHS 23.1.3 / READ-FROM-STRING: reader macros execute in the reader's
// dynamic environment and recursive READ shares its current input cursor.
test('Lisp reader macros preserve task dynamic bindings and recursive stream', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, `(let ((*readtable* (copy-readtable nil)) (answer 31))
    (declare (special answer))
    (set-macro-character #\\! (lambda (stream char) (declare (ignore stream char)) answer))
    (read-from-string "!"))`), [31, 1]);
  assert.deepEqual(evaluate(rt, `(let ((*readtable* (copy-readtable nil)))
    (set-macro-character #\\! (lambda (stream char) (declare (ignore char)) (read stream t nil t)))
    (read-from-string "!42"))`), [42, 3]);
  assert.deepEqual(evaluate(rt, `(let ((*readtable* (copy-readtable nil)))
    (set-macro-character #\\! (lambda (stream char) (declare (ignore stream char)) (values)))
    (read-from-string "(! 42 ; comment\n)"))`).map((v, i) => i === 0 ? toArray(v) : v), [[42], 17]);
  assert.deepEqual(evaluate(rt, `(let ((*readtable* (copy-readtable nil)))
    (set-macro-character #\\! (lambda (stream char) (declare (ignore char)) (char-code (read-char stream))))
    (read-from-string "!x"))`), [120, 2]);
});

test('dispatch reader macros receive argument, case folded subcharacter and same task', () => {
  const rt = createRuntime();
  const values = evaluate(rt, `(let ((*readtable* (copy-readtable nil)) (x 5)) (declare (special x))
    (make-dispatch-macro-character #\\!)
    (set-dispatch-macro-character #\\! #\\q (lambda (stream char arg) (declare (ignore stream char)) (+ x arg)))
    (read-from-string "!12q"))`);
  assert.deepEqual(values, [17, 4]);
});

// CLHS READTABLE-CASE examples plus escaping: only unescaped letters fold.
test('readtable case, syntax copies and readtable copies have independent state', () => {
  const rt = createRuntime();
  for (const [mode, input, expected] of [['upcase', 'foo', 'FOO'], ['downcase', 'FOO', 'foo'], ['preserve', 'Foo', 'Foo'], ['invert', 'foo', 'FOO'], ['invert', 'FOO', 'foo'], ['invert', 'Foo', 'Foo'], ['invert', '|x|foo', 'xFOO']]) {
    const [value] = evaluate(rt, `(let ((*readtable* (copy-readtable nil))) (setf (readtable-case *readtable*) :${mode}) (read-from-string "${input}"))`);
    assert.equal(value.name, expected);
  }
  assert.deepEqual(evaluate(rt, `(let ((*readtable* (copy-readtable nil)))
    (set-syntax-from-char #\\7 #\\;)
    (read-from-string "123579"))`), [1235, 4]);
  const [value] = evaluate(rt, `(let ((*readtable* (copy-readtable nil)))
    (set-syntax-from-char #\\! #\\") (read-from-string "!hello!"))`);
  assert.equal(value.toString(), 'hello');
  assert.deepEqual(evaluate(rt, `(let* ((a (copy-readtable nil)) (b (copy-readtable a))) (setf (readtable-case b) :downcase) (values (readtable-case a) (readtable-case b) (readtablep b)))`).map(v => v.name ?? v), ['UPCASE', 'DOWNCASE', 'T']);
});

test('reader specials affect number radix, packages, read evaluation and whitespace', () => {
  const rt = createRuntime();
  assert.deepEqual(evaluate(rt, '(let ((*read-base* 16)) (read-from-string "ff"))'), [255, 2]);
  assert.equal(evaluate(rt, '(let ((*package* (find-package :keyword))) (read-from-string "hello"))')[0], rt.intern('HELLO', 'KEYWORD'));
  assert.deepEqual(evaluate(rt, '(let ((x 41)) (declare (special x)) (read-from-string "#.(+ x 1)"))'), [42, 9]);
  assert.deepEqual(evaluate(rt, '(let ((*read-eval* nil)) (handler-case (read-from-string "#.42") (reader-error () :disabled)))'), [rt.intern('DISABLED', 'KEYWORD')]);
  assert.deepEqual(evaluate(rt, '(read-from-string "42  x" nil nil :preserve-whitespace t)'), [42, 2]);
  assert.deepEqual(evaluate(rt, '(read-from-string "42  x")'), [42, 3]);
  assert.equal(evaluate(rt, '(handler-case (read-from-string "") (end-of-file () :eof))')[0], rt.intern('EOF', 'KEYWORD'));
});

// CLHS *READ-SUPPRESS* examples: invalid object types and token syntax are
// ignored, dispatch macros still run, #= is whitespace, ## produces NIL.
test('suppression consumes invalid standard operands and dynamically informs custom macros', () => {
  const rt = createRuntime();
  for (const source of ['#C1.2', '#3R444', '#Ralpha', '#*ABC', '#\\GARBAGE', '#S(INTEGER)', '#3AHELLO', '#P(:TYPE :LISP)', 'NO-SUCH-PACKAGE:FOO', '#1=#1#']) assert.equal(rt.readFromString(source, { suppress: true })[0], NIL, source);
  assert.equal(evaluate(rt, `(let ((*readtable* (copy-readtable nil)))
    (set-macro-character #\\! (lambda (s c) (declare (ignore s c)) (if *read-suppress* nil (error "wrong suppression"))))
    (read-from-string "#+missing ! 42"))`)[0], 42);
  assert.equal(evaluate(rt, '(let ((*read-suppress* t)) (read-from-string "#.(error \\"not run\\")"))')[0], NIL);
  assert.equal(rt.findSymbol('FOO', 'NO-SUCH-PACKAGE'), null);
});

test('graph labels preserve sharing and circular vectors; Unicode input uses scalar bounds', () => {
  const rt = createRuntime();
  const [vector] = rt.readFromString('#1=#(#1#)');
  assert.ok(vector instanceof LispArray); assert.equal(vector.get(0), vector);
  const [list] = rt.readFromString('(#1=(a b) #1#)'); assert.equal(list.car, list.cdr.car);
  assert.deepEqual(rt.readFromString('😀 42', { start: 2 }), [42, 4]);
  assert.throws(() => rt.readFromString('\ud800'), /Unicode scalar/);
});


test('original ANSI READ-FROM-STRING keyword precedence and bounds cases', () => {
  const rt = createRuntime();
  // ANSI-TEST b9536e286d, reader/read-from-string.lsp .5-.17.
  for (const [form, expected] of [
    ['(read-from-string "71235" t nil :start 1 :end 4)', [123, 4]],
    ['(read-from-string "7123  " t nil :start 1)', [123, 5]],
    ['(read-from-string "7123  " t nil :end 4)', [7123, 4]],
    ['(read-from-string "7123" nil :foo :start 2 :end 2)', [rt.intern('FOO', 'KEYWORD'), 2]],
    ['(read-from-string "7123  " t nil :start 1 :start 2)', [123, 5]],
    ['(read-from-string "7123  " t nil :end 4 :end 2)', [7123, 4]],
    ['(read-from-string "123   " t nil :foo :bar :allow-other-keys t)', [123, 4]],
    ['(read-from-string "123   " t nil :allow-other-keys t :allow-other-keys nil :foo :bar)', [123, 4]],
  ]) assert.deepEqual(evaluate(rt, form), expected, form);
});

test('reader float defaults and radix ambiguity match the native reader', () => {
  const rt = createRuntime();
  assert.equal(evaluate(rt, '(let ((*read-base* 16)) (read-from-string "1e2"))')[0], 482);
  assert.equal(evaluate(rt, '(let ((*read-base* 16)) (read-from-string "123."))')[0], 123);
  assert.ok(evaluate(rt, '(let ((*read-default-float-format* \'double-float)) (read-from-string "1.5"))')[0] instanceof DoubleFloat);
  assert.ok(evaluate(rt, '(let ((*read-default-float-format* \'double-float)) (read-from-string "1.5s0"))')[0] instanceof SingleFloat);
  for (const source of ['#2B10', '#X1.5', '#C(x y)', '1.2.3']) assert.throws(() => rt.readFromString(source), ReaderError, source);
});

test('standard array and structure readers construct resident runtime objects', () => {
  const rt = createRuntime();
  const [matrix] = rt.readFromString('#2A((1 2)(3 4))'); assert.deepEqual(matrix.dimensions, [2, 2]); assert.deepEqual(matrix.storage, [1, 2, 3, 4]);
  const [scalar] = rt.readFromString('#0A42'); assert.deepEqual(scalar.dimensions, []); assert.equal(scalar.get(0), 42);
  assert.deepEqual(evaluate(rt, '(defstruct reader-point x (y 7)) (let ((p (read-from-string "#S(reader-point :x 3)"))) (values (reader-point-x p) (reader-point-y p)))'), [3, 7]);
  assert.deepEqual(evaluate(rt, `(let ((*readtable* (copy-readtable nil)))
    (set-macro-character #\\! (lambda (stream char) (declare (ignore char)) (read-delimited-list #\\] stream t)))
    (read-from-string "!1 2 ]"))`).map((v, i) => i === 0 ? toArray(v) : v), [[1, 2], 6]);
  assert.throws(() => rt.readFromString('#2A((1)(2 3))'), ReaderError);
});
