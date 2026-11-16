import test from "node:test";
import assert from "node:assert/strict";
import { createRuntime, NIL, toArray } from "../runtime/runtime.mjs";
import { installMacros } from "../runtime/macros.mjs";

function runtime() {
  return installMacros(createRuntime());
}
function printed(rt, source) {
  return rt
    .evalString(source)
    .values.map((x) => rt.call("WRITE-TO-STRING", [x]).values[0].toString());
}
function check(rt, source, ...expected) {
  assert.deepEqual(printed(rt, source), expected, source);
}

test("PUSHNEW evaluates the item, place subforms, and options once in order", () => {
  const rt = runtime();
  check(rt, `(let ((track nil) (slot (vector (list '(1 . old)))))
    (let* ((old (aref slot 0))
           (returned (pushnew (progn (push :item track) '(1 . new))
                              (aref slot (progn (push :place track) 0))
                              :key (progn (push :key track) #'car))))
      (values (eq old returned) (eq returned (aref slot 0)) (reverse track))))`,
  "T", "T", "(:ITEM :PLACE :KEY)");
});

test("LOOP arithmetic and parallel groups preserve source evaluation and commit order", () => {
  const rt = runtime();
  check(
    rt,
    "(loop for x from 1 to 4 for y = (+ x 10) collect (list x y))",
    "((1 11) (2 12) (3 13) (4 14))",
  );
  check(
    rt,
    "(loop for x = 1 then y and y = 2 then x repeat 3 collect (list x y))",
    "((1 2) (2 1) (1 2))",
  );
  check(
    rt,
    "(loop for i from 0 below 3 and j from 1 finally (return (list i j)))",
    "(2 3)",
  );
  check(rt, "(loop for i downfrom 7 above 1 by 2 collect i)", "(7 5 3)");
  check(
    rt,
    "(loop for a in '(1 2) and b = a collect (list a b))",
    "((1 NIL) (2 1))",
  );
  check(
    rt,
    "(let ((effects nil)) (values (loop for i from (progn (push :start effects) 1) by (progn (push :step effects) 2) to (progn (push :limit effects) 5) collect i) effects))",
    "(1 3 5)",
    "(:LIMIT :STEP :START)",
  );
  check(
    rt,
    "(let ((effects 0)) (values (loop for x in nil for y = (incf effects) collect y) effects))",
    "NIL",
    "0",
  );
});

test("LOOP binds WITH groups, type defaults, and recursive destructuring", () => {
  const rt = runtime();
  check(
    rt,
    "(let ((a 9)) (values (loop with a = 1 and b = a return b) (loop with a = 1 with b = a return b)))",
    "9",
    "1",
  );
  check(
    rt,
    "(loop with i fixnum and j of-type number and k return (values i j k))",
    "0",
    "0",
    "NIL",
  );
  check(
    rt,
    "(loop with (a (b c)) of-type (fixnum (float t)) return (list a b c))",
    "(0 0.0 NIL)",
  );
  check(
    rt,
    "(loop for (a (b c)) in '((1 (2 3))) collect (list a b c))",
    "((1 2 3))",
  );
  check(
    rt,
    "(loop for (a . b) on '(1 2 . 3) and c = 10 then (1+ c) collect (list a b c) into result finally (return (list result a b c)))",
    "(((1 (2 . 3) 10) (2 3 11)) 2 3 11)",
  );
  check(rt, "(loop for tail on '(1 . 2) finally (return tail))", "2");
});

test("LOOP list, vector and string traversal stop before invalid element access", () => {
  const rt = runtime();
  check(rt, "(loop for x in '(1 2 3 4 5) by #'cddr collect x)", "(1 3 5)");
  check(rt, "(loop for x across #(1 2 3) sum x)", "6");
  check(rt, "(loop for x across #() collect x)", "NIL");
  check(rt, '(loop for x across "abc" collect x)', "(#\\a #\\b #\\c)");
  check(
    rt,
    "(loop for i from 1 to 2 for x across #(10 20) collect (list i x))",
    "((1 10) (2 20))",
  );
  assert.throws(
    () => rt.evalString("(loop for x in '(1 . 2) collect x)"),
    /list|cons|ENDP/i,
  );
});

test("LOOP accumulation shares compatible results and keeps destructive semantics", () => {
  const rt = runtime();
  check(
    rt,
    "(values (loop for n from 1 to 3 sum n count (< n 3)) (loop for n in '(-7 -2 -5) maximize n) (loop repeat 0 sum 4 of-type float))",
    "8",
    "-2",
    "0.0",
  );
  check(
    rt,
    "(loop for n in '(1 2 3) collect n into xs sum n into total finally (return (values xs total)))",
    "(1 2 3)",
    "6",
  );
  check(rt, "(loop for x in '((1 2) (3) nil (4 5)) append x)", "(1 2 3 4 5)");
  check(
    rt,
    "(let ((x (list 1 2))) (values (loop repeat 1 append x) x))",
    "(1 2)",
    "(1 2)",
  );
  check(
    rt,
    "(let ((x (list 1 2))) (loop for v in (list x (list 3)) nconc v) x)",
    "(1 2 3)",
  );
  assert.throws(
    () => rt.evalString("(loop repeat 3 collect 1 sum 2)"),
    /Incompatible/,
  );
});

test("LOOP conditionals support nested selection, IT and explicit END", () => {
  const rt = runtime();
  check(
    rt,
    "(loop for n in '(1 2 4) if (if (< n 3) (* n 10) nil) collect it else append (list n :end) end)",
    "(10 20 4 :END)",
  );
  check(
    rt,
    "(loop for n below 5 if (oddp n) if (> n 2) collect n else collect (- n) end else collect 0 end)",
    "(0 -1 0 3 0)",
  );
  check(
    rt,
    "(loop repeat 2 unless nil collect it and collect :yes end)",
    "(NIL :YES NIL :YES)",
  );
});

test("LOOP exits preserve multiple values, lexical names, cleanup, and FINALLY", () => {
  const rt = runtime();
  check(rt, "(loop (return (values 40 2)))", "40", "2");
  check(
    rt,
    "(loop :named outer :for i :from 1 :to 99 :when (= i 3) :return (values i 42))",
    "3",
    "42",
  );
  check(
    rt,
    "(let ((log nil)) (values (loop for n from 1 do (push n log) until (= n 3) collect n finally (push :finished log)) log))",
    "(1 2)",
    "(:FINISHED 3 2 1)",
  );
  check(
    rt,
    "(let ((effects nil)) (values (loop for n in '(1 2 3) do (push n effects) thereis (if (= n 2) 42) finally (push :finally effects)) effects))",
    "42",
    "(2 1)",
  );
  check(
    rt,
    "(loop for n in '(1 2 3 4) when (= n 3) do (loop-finish) sum n)",
    "3",
  );
  check(
    rt,
    "(loop for x from 1 collect (loop for y from 1 do (loop-finish) finally (return x)) when (= x 2) do (loop-finish))",
    "(1 2)",
  );
  check(
    rt,
    "(let ((effects nil)) (values (loop for i from 0 do (unwind-protect (loop-finish) (push :cleanup effects)) finally (push :finally effects)) effects))",
    "NIL",
    "(:FINALLY :CLEANUP)",
  );
  check(
    rt,
    '(values (loop repeat -1 always (error "unexpected")) (loop for n from 1 to 3 never (= n 2)))',
    "T",
    "NIL",
  );
  assert.throws(() => rt.evalString("(loop-finish)"), /extended LOOP/);
});

test("LOOP hash-table and package iteration preserve symbol and entry identities", () => {
  const rt = runtime();
  check(
    rt,
    "(let ((h (make-hash-table))) (setf (gethash :a h) 1 (gethash :b h) 2) (loop for k being the hash-keys of h using (hash-value v) collect (list k v)))",
    "((:A 1) (:B 2))",
  );
  check(
    rt,
    "(let ((h (make-hash-table))) (setf (gethash :a h) (list 1 2)) (loop for (a b) being each hash-value in h using (hash-key k) return (list a b k)))",
    "(1 2 :A)",
  );
  const pkg = rt.makePackage("LOOP-PACKAGE"),
    sym = rt.intern("ONE", pkg);
  rt.exportSymbol(sym, pkg);
  const result = rt.evalString(
    '(loop for s being the external-symbols of "LOOP-PACKAGE" collect s)',
  ).values[0];
  assert.deepEqual(toArray(result), [sym]);
});

test("DO family scopes declarations and retains termination values", () => {
  const rt = runtime();
  check(
    rt,
    "(do ((x 1 y) (y 2 x) (i 0 (1+ i))) ((= i 3) (values x y)) (declare (ignorable x y)))",
    "2",
    "1",
  );
  check(
    rt,
    "(do* ((x 1 y) (y 2 x) (i 0 (1+ i))) ((= i 3) (values x y)))",
    "2",
    "2",
  );
  check(
    rt,
    "(dolist (x (list 1 2) (values x 42)) (declare (ignorable x)))",
    "NIL",
    "42",
  );
  check(rt, "(dotimes (i 3 (values i 42)) (declare (ignorable i)))", "3", "42");
  check(
    rt,
    "(block done (dolist (x (list 1 2)) (flet ((leave () (return-from done (values x 42)))) (leave))))",
    "1",
    "42",
  );
  check(
    rt,
    "(let ((x 7)) (dolist (x (list x) x) (declare (special x))))",
    "NIL",
  );
});

test("standard macro functions reject every nonstandard calling arity", () => {
  const rt = runtime();
  for (const name of [
    "LOOP",
    "DO",
    "DO*",
    "DOLIST",
    "DOTIMES",
    "WITH-STANDARD-IO-SYNTAX",
  ]) {
    const macro = rt.macroFunction(rt.intern(name, "COMMON-LISP"));
    const form = rt.read(`(${name})`);
    for (const args of [[], [form], [form, NIL, NIL]])
      assert.throws(() => rt.call(macro, args), /two arguments/);
  }
});

test("WITH-STANDARD-IO-SYNTAX binds genuine reader and printer state and preserves values", () => {
  const rt = runtime();
  check(rt, "(with-standard-io-syntax (values 1 2))", "1", "2");
  check(
    rt,
    '(let ((*read-base* 16) (*print-base* 16) (*print-escape* nil)) (values (with-standard-io-syntax (list (write-to-string 42) (read-from-string "10") (write-to-string "hello"))) (read-from-string "10")))',
    '("42" 10 "\\\"hello\\\"")',
    "16",
  );
  check(
    rt,
    '(with-standard-io-syntax (values *read-base* *print-base* (eq *package* (find-package "CL-USER")) *read-default-float-format*))',
    "10",
    "10",
    "T",
    "SINGLE-FLOAT",
  );
});

test("LOOP hash iteration permits removing each current entry", () => {
  const rt = runtime();
  check(
    rt,
    "(let ((h (make-hash-table))) (setf (gethash :a h) 1 (gethash :b h) 2 (gethash :c h) 3) (values (loop for k being the hash-keys of h using (hash-value v) do (remhash k h) sum v) (hash-table-count h)))",
    "6",
    "0",
  );
});

test("ASSERT retries its canonical test and permits restart replacement places", () => {
  const rt = runtime();
  check(rt, "(assert t)", "NIL");
  check(
    rt,
    "(let ((n 0)) (handler-bind ((error (lambda (c) (declare (ignore c)) (incf n) (invoke-restart 'continue)))) (assert (> n 1))) n)",
    "2",
  );
  check(
    rt,
    "(let ((x 0)) (handler-bind ((error (lambda (c) (declare (ignore c)) (invoke-restart 'continue 42)))) (assert (= x 42) (x))) x)",
    "42",
  );
  assert.throws(
    () => rt.evalString('(assert nil () "assertion failure ~A" 42)'),
    /assertion failure/,
  );
});

test("compiled LOOP retains lexical induction and cleanup across host suspension", async () => {
  const rt = runtime();
  rt.registerFunction("HOST-VALUE", async (args) => args[0] * 2);
  const fn = rt.compile(
    rt.read(
      "(lambda () (let ((cleanups 0)) (values (loop for i below 4 collect (unwind-protect (host-value i) (incf cleanups))) cleanups)))",
    ),
  );
  const result = await rt.callAsync(fn);
  assert.deepEqual(toArray(result.values[0]), [0, 2, 4, 6]);
  assert.equal(result.values[1], 4);
});
