import assert from "node:assert/strict";
import test from "node:test";
import { NIL, Runtime, toArray } from "../runtime/runtime.mjs";

const strings = (runtime, source) => runtime.evalString(source).values.map((value) => {
  if (value.elementKind === "character" && value.rank === 1)
    return Array.from({ length: value.length }, (_, index) => String(value.get(index))).join("");
  return value.name ?? String(value);
});

test("FORMAT parses nested clauses and preserves the correct argument cursor", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, String.raw`
    (values
      (format nil "~:[empty~;~@(~{~D~^, ~}~)~]|~[zero~;one~:;many~]" t '(1 2) 7)
      (format nil "~:{~A~^=~A~:^; ~} / ~2@{~D~}~D" '((x) (y 2)) 3 4 5)
      (format nil "~?/~@?" "~D~*~D" '(1 2 3) "~D~:*~D" 4)
      (format nil "[~V,2,1,'🐚A]|~#[no args~;one arg~]" 5 "雪" 7)
      (format nil "~S~:[~;~2:*~A~]" "雪" t)
      (eql (code-char 0) (read-from-string (format nil "~@C" (code-char 0)))))
  `), ["1, 2|many", "XY=2 / 345", "13/44", "[雪🐚🐚🐚🐚🐚]|one arg", '\"雪\"雪', "T"]);
});

test("FORMATTER returns unused arguments and indirection consumes the reported tail", () => {
  const rt = new Runtime();
  const [tail, text, empty] = rt.evalString(`
    (let ((outer (formatter "~@?~A"))
          (inner (formatter "<~A>"))
          (stream (make-string-output-stream)))
      (let ((tail (funcall outer stream inner 1 2 3 4)))
        (values tail (get-output-stream-string stream)
          (format nil "~?~A" (formatter "unused") '(discarded) :done))))
  `).values;
  assert.deepEqual(toArray(tail), [3, 4]);
  assert.equal(String(text), "<1>2");
  assert.equal(String(empty), "unusedDONE");
});

test("FORMAT callbacks see the original stream, modifiers, and evaluated parameters", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, String.raw`
    (defun redirect-format-output (stream out colon atsign &rest params)
      (format stream "~6T~A~A~S" colon atsign params)
      (setq *standard-output* out))
    (let ((out (make-string-output-stream)) (other (make-string-output-stream)))
      (let ((*standard-output* out))
        (write-string "ab" out)
        (format t "~V,#,'🐚:@/redirect-format-output/!~&tail" 5 other))
      (values (get-output-stream-string out) (get-output-stream-string other)))
  `), ["ab    TT(5 1 #\\🐚)!\ntail", ""]);
});

test("FORMAT resumes a suspended callback and preserves nonlocal Lisp transfers", async () => {
  const rt = new Runtime();
  let resume, intercepted = false;
  rt.registerFunction("CL-USER::PAUSE-FORMAT", () => new Promise(resolve => { resume = resolve; }));
  rt.registerFunction("CL-USER::ASSERT-SAME-STREAM", ([actual, expected]) => {
    assert.equal(actual, expected);
    intercepted = true;
    return NIL;
  });
  rt.evalString(`
    (defun async-format-callback (stream expected colon atsign)
      (declare (ignore colon atsign))
      (assert-same-stream stream expected)
      (format stream "[~A]" (pause-format)))
    (defun escape-format-callback (stream object colon atsign)
      (declare (ignore colon atsign))
      (write-string "!" stream)
      (throw 'format-done object))
  `);
  const pending = rt.evalStringAsync(`
    (with-output-to-string (stream)
      (format stream "start~/async-format-callback/finish~D" stream 7))
  `);
  assert.ok(intercepted);
  resume(42);
  assert.deepEqual((await pending).values.map(String), ["start[42]finish7"]);
  assert.deepEqual(strings(rt, `
    (let ((stream (make-string-output-stream)))
      (values (catch 'format-done (format stream "before~/escape-format-callback/ignored" 17))
              (get-output-stream-string stream)))
  `), ["17", "before!"]);
});

test("FORMAT fields use exact integer and decimal arithmetic", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, `
    (values
      (format nil "~:D ~5,'0@X ~3R ~R ~:R ~@R ~:@R" 12345678901234567890 255 25 1001 321 14 14)
      (format nil "~7,2F|~,2E|~9,1,2,0@E|~2,3,9,'0:@$" 1/8 637.5 99.9 -1/8)
      (format nil "~5E|~6E|~6,,2E|~8,3G" 1 1 1 12.5))
  `), ["12,345,678,901,234,567,890 00+FF 221 one thousand one three hundred twenty-first XIV XIIII",
    "   0.12|6.38e+2| +0.1e+03|-00000.12", "1.e+0|1.0e+0|1.e+00|12.5    "]);
});

test("Float printing and FORMAT share shortest round-trip decimal spelling", () => {
  const rt = new Runtime({ readEval: false });
  assert.deepEqual(strings(rt, `
    (let ((*read-default-float-format* 'double-float))
      (let* ((x 0.1f0) (small 1.0d-13) (negative-zero -0.0d0)
             (readable (write-to-string x :readably t)))
        (values (prin1-to-string x) (prin1-to-string small)
                (prin1-to-string negative-zero) (format nil "~F ~E" x small)
                readable (eql x (read-from-string readable)))))
  `), ["0.1f0", "1.0e-13", "-0.0", "0.1 1.0e-13", "0.1f0", "T"]);
});

test("FORMAT waits for asynchronous output writes before continuing", async () => {
  const written = [];
  const rt = new Runtime({ output: { async write(text) {
    await Promise.resolve();
    written.push(text);
  } } });
  await rt.evalStringAsync('(format t "start ~D ~? end" 3 "~A" (list "inner"))');
  assert.equal(written.join(""), "start 3 inner end");
});

test("FORMAT logical blocks share layout with Lisp callbacks and honor print limits", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, String.raw`
    (defun format-break (stream text colon atsign)
      (declare (ignore colon atsign))
      (write-string text stream)
      (pprint-newline :mandatory stream)
      (write-string "yy" stream))
    (let ((*print-pretty* t) (*print-right-margin* 6))
      (values
        (format nil "pre~@<~/format-break/~:>" "xx")
        (format nil "zz~@<~@<aa ~_bb~:>~:>")
        (format nil "~:<~@{~A~^ ~}~:>" '(a b . c))
        (let ((*print-length* 2)) (format nil "~:<~@{~A~^ ~}~:>" '(a b c)))
        (let ((*print-level* 1)) (format nil "~:<~:<~W~:>~:>|~W" '((x)) '(x)))
        (with-output-to-string (s)
          (pprint-logical-block (s '(a b) :prefix "[" :suffix "]")
            (loop (pprint-exit-if-list-exhausted) (princ (pprint-pop) s))))))
  `), ["prexx\n   yy", "zzaa\n  bb", "(A B . C)", "(A B ...)", "(#)|(X)", "[AB]"]);
});

test("FORMAT and printer callbacks use one graph for nested shared objects", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, String.raw`
    (let* ((*print-circle* t) (*print-pretty* t)
           (shared (list 0)) (arguments (list (list shared) shared)))
      (values
        (format nil "~:<~@{~S~^ ~}~:>" arguments)
        (with-output-to-string (s)
          (pprint-logical-block (s arguments :prefix "(" :suffix ")")
            (prin1 (pprint-pop) s)
            (write-char #\Space s)
            (prin1 (pprint-pop) s)))
        (let ((text (format nil "~:<~@{~W~^ ~}~:>" arguments)))
          (let ((parsed (read-from-string text)))
            (eq (caar parsed) (cadr parsed))))))
  `), ["((#1=(0)) #1#)", "((#1=(0)) #1#)", "T"]);
});

test("FORMAT pretty printing of lists and custom printer output shares line layout", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, `
    (defclass format-field () ())
    (defmethod print-object ((value format-field) stream)
      (pprint-logical-block (stream nil) (format stream "a~:@_b")))
    (let ((*print-right-margin* 6) (*print-length* 1))
      (let ((object (make-instance 'format-field)))
        (values (format nil "~W" '(one two three))
                (format nil "~:@W" '(one two three))
                (let ((*print-pretty* t) (*print-length* nil))
                  (format nil "xy~S" (list object)))
                (let ((*print-pretty* t))
                  (format nil "one~A~%xy~@<~W~:>" object object)))))
  `), ["(ONE ...)", "(ONE\n TWO\n THREE)", "xy(a\n   b)", "onea\n   b\nxya\n  b"]);
});

test("Pretty printing uses section tabs and preserves pending suffixes at a line limit", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, `
    (let ((*print-pretty* t))
      (values
        (let ((*print-right-margin* 25))
          (with-output-to-string (stream)
            (write-string "Roads " stream)
            (pprint-tabular stream '(elm main maple center) nil nil 8)))
        (let ((*print-right-margin* 5) (*print-lines* 1))
          (format nil "~:W" '((abc def ghi) jkl)))
        (let ((*print-lines* 1)) (format nil "~:@<~A ~:@_~A~:>" 1 2))))
  `), ["Roads ELM     MAIN\n      MAPLE   CENTER", "((ABC ..))", "(1 ..)"]);
});

test("FORMAT appends to adjustable and displaced fill-pointer strings", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, String.raw`
    (let* ((string (make-array 2 :element-type 'character :initial-contents "ab"
                  :fill-pointer 2 :adjustable t))
           (backing (copy-seq "_ab____z"))
           (displaced (make-array 6 :element-type 'character :displaced-to backing
                      :displaced-index-offset 1 :fill-pointer 2)))
      (format string "~5T~A" "雪")
      (format displaced (lambda (stream &rest args) (write-char (first args) stream)) #\C)
      (values string displaced (fill-pointer string) (fill-pointer displaced) backing))
  `), ["ab   雪", "abC", "6", "3", "_abC___z"]);
});

test("FORMAT signals malformed controls before producing output", () => {
  const rt = new Runtime();
  assert.deepEqual(strings(rt, `
    (let ((stream (make-string-output-stream)))
      (values (handler-case (format stream "discard~{~A~]") (program-error () :caught))
              (get-output-stream-string stream)
              (handler-case (format nil "~?" 0 nil) (type-error () :bad-control))))
  `), ["CAUGHT", "", "BAD-CONTROL"]);
  assert.throws(() => rt.evalString('(format nil "~{constant~}" (list 1))'),
    { className: "FORMAT-ERROR" });
});
