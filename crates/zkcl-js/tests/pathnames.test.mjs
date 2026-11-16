import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, LispPathname, NIL, T } from '../runtime/runtime.mjs';

const values = (rt, source) => rt.evalString(source).values;
const hostValues = (rt, items) => items.map(value => rt.toJS(value));

// CLHS 2.4.8, 19.2 and 22.1.3: a pathname literal is self-evaluating,
// respects reader suppression, and can be written readably when representable.
test('pathname literals, predicates, classes and readable printing agree across resident entry points', () => {
  const rt = createRuntime();
  const source = String.raw`(let ((path #P"/work/测试.v1.lisp"))
    (values path (pathnamep path) (typep path 'pathname) (type-of path)
            (eq path (pathname path)) (equal path (pathname "/work/测试.v1.lisp"))
            (equalp path #P"/work/测试.V1.lisp")
            (equal path (read-from-string (write-to-string path :readably t)))))`;
  for (const result of [values(rt, source), rt.call(rt.compileString(source)).values]) {
    const [path, predicate, typed, type, identical, same, different, roundtrip] = result;
    assert.ok(path instanceof LispPathname);
    assert.equal(rt.pathnameNamestring(path), '/work/测试.v1.lisp');
    assert.deepEqual([predicate, typed, type.name, identical, same, different, roundtrip], [T, T, 'PATHNAME', T, T, NIL, T]);
  }
  assert.deepEqual(values(rt, '#-javascript #3P(ignored) 42'), [42]);
  assert.throws(() => rt.read('#3P"file"'), error => error.className === 'READER-ERROR');
  assert.throws(() => rt.read('#P(ignored)'), error => error.className === 'READER-ERROR');
  assert.throws(() => values(rt, '(write-to-string (make-pathname :name "file" :version 3) :readably t)'), error => error.className === 'PRINT-NOT-READABLE');
  assert.equal(values(rt, String.raw`(let ((path #P"/work/\\*.lisp"))
    (unintern :absolute :keyword)
    (and (not (wild-pathname-p path)) (namestring path)))`)[0].toString(), '/work/\\*.lisp');
});

// CLHS MAKE-PATHNAME, MERGE-PATHNAMES, PARSE-NAMESTRING, and ENOUGH-NAMESTRING.
test('pathname components merge with explicit NIL and parse scalar bounds', () => {
  const rt = createRuntime();
  const source = String.raw`(let ((*default-pathname-defaults* #P"/work/src/base.lisp"))
    (multiple-value-bind (pathname index)
        (parse-namestring "前ab/测.def.trailing后" nil nil :start 1 :end 9)
      (values (namestring pathname) index
              (namestring (merge-pathnames "foo/bar"))
              (namestring (compile-file-pathname "app.lisp" :verbose t))
              (namestring (compile-file-pathname "app.lisp" :output-file "obj/out"))
              (namestring (make-pathname :name "new" :type nil :defaults "a.lisp"))
              (pathname-type "mixed.lisp" :case :common)
              (namestring (make-pathname :directory '(:relative "down") :defaults #P"/root/base.lisp"))
              (namestring (merge-pathnames "later/item.lisp"
                   (make-pathname :directory '(:absolute "base" :wild :back "temp" :back)))))))`;
  assert.deepEqual(hostValues(rt, values(rt, source)), ['ab/测.def', 9, '/work/src/foo/bar.lisp', '/work/src/app.mjs', '/work/src/obj/out.mjs', 'new', 'LISP', 'down/base.lisp', '/base/later/item.lisp']);
});

test('logical host rules are ordered, atomically replaced, and scoped to the runtime', () => {
  const rt = createRuntime(), other = createRuntime();
  values(rt, `(setf (logical-pathname-translations "WORK")
    '(("SPECIAL;*.*.*" "/priority/*.*") ("**;*.*.*" "/sandbox/**/*.*")))
    (setf (logical-pathname-translations "ALIAS") '(("**;*.*.*" "WORK:**;*.*.*")))`);
  const source = `(multiple-value-bind (path index)
      (parse-namestring ";Src;file.lisp.31 junk" "work" nil :junk-allowed t)
    (values (type-of path) (namestring path) index
            (nth-value 1 (subtypep 'logical-pathname 'pathname))
            (typep (caar (logical-pathname-translations "work")) 'logical-pathname)
            (namestring (translate-logical-pathname "ALIAS:Special;File.Lisp.3"))
            (namestring (translate-logical-pathname "ALIAS:Src;File.Lisp.3"))
            (pathname-match-p "WORK:LIB-ABC;TEST.LISP" "WORK:LIB-*;T*.LISP.*")
            (namestring (translate-pathname "WORK:LIB-A-MID-B;FILE.LISP"
                 "WORK:LIB-*-MID-*;*.LISP" "ALIAS:*--*;*.LISP"))))`;
  const [type, ...result] = values(rt, source);
  assert.equal(type.name, 'LOGICAL-PATHNAME');
  assert.deepEqual(hostValues(rt, result), ['WORK:;SRC;FILE.LISP.31', 17, true, true, '/priority/file.lisp', '/sandbox/src/file.lisp', true, 'ALIAS:A--B;FILE.LISP']);
  assert.deepEqual(hostValues(rt, values(rt, String.raw`(let ((*default-pathname-defaults* (logical-pathname "work:src;base.lisp")))
    (values (namestring (read-from-string "#P\";relative\""))
            (namestring (merge-pathnames ";unit"))
            (enough-namestring ";unit.lisp")))`)), ['WORK:;RELATIVE', 'WORK:SRC;UNIT.LISP.NEWEST', ';UNIT.LISP.NEWEST']);
  assert.throws(() => values(rt, '(setf (logical-pathname-translations "WORK") \'(("INVALID HOST:*" "file")))'), error => error.className === 'FILE-ERROR');
  assert.equal(values(rt, '(length (logical-pathname-translations "WORK"))')[0], 2);
  assert.throws(() => values(other, '(logical-pathname "WORK:FILE.LISP")'), error => error.className === 'TYPE-ERROR');
  values(rt, '(setf (logical-pathname-translations "CYCLE") \'(("**;*.*.*" "CYCLE:**;*.*.*")))');
  assert.throws(() => values(rt, '(translate-logical-pathname "CYCLE:FILE.LISP")'), error => error.className === 'FILE-ERROR' && /Circular/.test(error.message));
  values(rt, '(setf (logical-pathname-translations "GROW") \'(("**;*.*.*" "GROW:DIR;**;*.*.*")))');
  const task = rt.createTask('TRANSLATE-LOGICAL-PATHNAME', [rt.fromJS('GROW:FILE.LISP')]);
  task.run({ budget: 4 });
  assert.equal(task.done, false);
});

test('filesystem calls translate pathnames, pass raw filenames and handle rejection on the Lisp task', async () => {
  const encountered = [];
  const rt = createRuntime({ pathnameHost: {
    async probeFile(file, { pathname, physicalPath, task }) {
      encountered.push([file, pathname.logical, physicalPath.logical, Boolean(task)]);
      return file === '/files/missing.lisp' ? null : file;
    },
    async deleteFile() { throw new Error('test access denied'); },
    directory(pattern) {
      assert.equal(pattern, '/files/**/*.*');
      return ['/files/*:literal.txt', '/files/*:literal.txt'];
    },
    ensureDirectoriesExist(dir) { assert.equal(dir, '/files/new/dir/'); return true; },
    renameFile(file, context, target) { return { oldTruename: file, newTruename: target }; },
  } });
  values(rt, '(setf (logical-pathname-translations "APP") \'(("**;*.*.*" "/files/**/*.*")))');
  const result = (await rt.evalStringAsync(String.raw`(values
    (namestring (probe-file "APP:source.lisp")) (probe-file "APP:missing.lisp")
    (namestring (probe-file #P"/files/\\*.lisp"))
    (mapcar #'namestring (directory "APP:**;*.*.*"))
    (handler-case (delete-file "APP:source.lisp")
      (file-error (condition) (namestring (file-error-pathname condition))))
    (nth-value 1 (ensure-directories-exist "APP:NEW;DIR;*.TXT"))
    (nth-value 2 (rename-file "APP:source.lisp" ";renamed.lisp")))`)).values;
  assert.deepEqual(hostValues(rt, result.slice(0, 3)), ['/files/source.lisp', null, '/files/\\*.lisp']);
  assert.equal(rt.print(result[3]), '("/files/*\\\\:literal.txt")');
  assert.deepEqual(hostValues(rt, result.slice(4, 6)), ['APP:SOURCE.LISP.NEWEST', true]);
  assert.equal(rt.pathnameNamestring(result[6]), '/files/renamed.lisp');
  assert.deepEqual(encountered, [['/files/source.lisp', true, false, true], ['/files/missing.lisp', true, false, true], ['/files/*.lisp', false, false, true]]);
  assert.throws(() => values(createRuntime(), '(probe-file "file")'), error => error.className === 'FILE-ERROR' && /not configured/.test(error.message));
});
