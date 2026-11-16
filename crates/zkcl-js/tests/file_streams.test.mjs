import test from 'node:test';
import assert from 'node:assert/strict';
import { createRuntime, NIL, T } from '../runtime/runtime.mjs';
import { LispFileStream } from '../runtime/file_streams.mjs';

function fixtures(contents) {
  const files = new Map(Object.entries(contents)), events = [];
  const host = {
    truename(filename) { return filename; },
    openFile(filename, _context, options) {
      events.push(['open', filename, options.direction, options.elementType]);
      const binary = options.elementType === 'octet';
      const original = files.get(filename);
      if (original === undefined && options.ifDoesNotExist === null) return null;
      if (original === undefined && options.ifDoesNotExist === 'error') throw new Error('File does not exist');
      const writing = ['output', 'io'].includes(options.direction);
      let data = [...(!writing || ['overwrite', 'append'].includes(options.ifExists) ? original ?? [] : [])];
      const box = value => binary ? Uint8Array.from(value) : value.join('');
      const read = (at, count) => box(data.slice(at, count === null ? undefined : at + count));
      return {
        length: () => data.length,
        readAt: async (at, count) => read(at, count),
        readNoHangAt: read,
        async writeAt(at, value) { data.splice(at, [...value].length, ...value); },
        stringLength: value => new TextEncoder().encode(value).length,
        async close({ abort }) {
          events.push(['close', filename, abort]);
          if (writing && !abort) files.set(filename, box(data));
        },
      };
    },
  };
  return { host, files, events };
}
const evalAsync = (rt, source) => rt.evalStringAsync(source).then(result => result.values);

test('file stream reads and reader macros share one Unicode cursor and pathname identity', async () => {
  const { host, events } = fixtures({ '/src/test.lisp': '!35 😀 foo\nline two\n' });
  const rt = createRuntime({ pathnameHost: host });
  rt.evalString('(defparameter *saved-file* nil)');
  const [first, char, position, line, missingNewline, location, asClass] = await evalAsync(rt, String.raw`
    (with-open-file (s "/src/test.lisp")
      (setq *saved-file* s)
      (let ((*readtable* (copy-readtable nil)))
        (set-macro-character #\! (lambda (input char)
          (declare (ignore char))
          (unless (and (eq input s) (eq (pathname input) (pathname s))) (error "wrong reader stream"))
          (read input t nil t)))
        (let* ((first (read s)) (char (read-char-no-hang s))
               (unread (unread-char char s)) (peeked (peek-char nil s))
               (position (file-position s)))
          (declare (ignore unread))
          (multiple-value-bind (line missing) (read-line s)
            (values first peeked position line missing (namestring (pathname s)) (typep s 'file-stream))))))`);
  assert.deepEqual([first, char.toString(), position, line.toString(), missingNewline, location.toString(), asClass], [35, '😀', 4, '😀 foo', NIL, '/src/test.lisp', T]);
  assert.deepEqual(events, [['open', '/src/test.lisp', 'input', 'character'], ['close', '/src/test.lisp', false]]);
  const [stream, closed, pathname, closedAgain] = rt.evalString('(values *saved-file* (open-stream-p *saved-file*) (pathname *saved-file*) (close *saved-file*))').values;
  assert.ok(stream instanceof LispFileStream);
  assert.equal(closed, NIL);
  assert.equal(pathname, stream.pathname);
  assert.equal(closedAgain, NIL);
  assert.deepEqual((await evalAsync(rt, '(handler-case (read-char *saved-file*) (stream-error (c) (eq *saved-file* (stream-error-stream c))))')), [T]);
  assert.deepEqual(await evalAsync(rt, `(with-open-file (s "/src/test.lisp")
    (file-position s :end)
    (handler-case (read s)
      (end-of-file (c) (eq s (stream-error-stream c)))))`), [T]);
});

test('OPEN and CLOSE pass file policy and WITH-OPEN-FILE aborts on nonlocal exit', async () => {
  const { host, files, events } = fixtures({ '/out': 'original' });
  const rt = createRuntime({ pathnameHost: host });
  const result = await evalAsync(rt, `(values
      (catch 'done
        (with-open-file (s "/out" :direction :output :if-exists :supersede)
          (write-line "abandoned" s)
          (throw 'done :aborted)))
      (open "/missing" :if-does-not-exist nil))`);
  assert.equal(result[0].name, 'ABORTED');
  assert.equal(result[1], NIL);
  assert.equal(files.get('/out'), 'original');
  assert.deepEqual(await evalAsync(rt, String.raw`(with-open-file (s "/out" :direction :io :if-exists :append)
    (write-char #\! s)
    (write-string "append" s)
    (values (file-position s) (file-length s) (file-string-length s "😀")))`), [15, 15, 4]);
  assert.equal(files.get('/out'), 'original!append');
  assert.deepEqual(events.filter(event => event[0] === 'close'), [['close', '/out', true], ['close', '/out', false]]);
  const probe = (await evalAsync(rt, '(open "/out" :direction :probe)'))[0];
  assert.ok(probe instanceof LispFileStream);
  assert.equal(probe.open, false);
  const [badPath] = await evalAsync(rt, '(handler-case (open "/does-not-exist") (file-error (c) (namestring (file-error-pathname c))))');
  assert.equal(badPath.toString(), '/does-not-exist');
});

test('octet file streams share one position across READ-BYTE, sequence I/O, and seeks', async () => {
  const { host, files } = fixtures({});
  const rt = createRuntime({ pathnameHost: host });
  const result = await evalAsync(rt, `(with-open-file (s "/data" :direction :io :if-exists :supersede
      :element-type '(unsigned-byte 8))
    (write-byte 255 s)
    (write-sequence #(3 4 5 6) s :start 1 :end 3)
    (file-position s :start)
    (let ((target (make-array 3 :element-type '(unsigned-byte 8) :initial-element 0)))
      (values (equal (stream-element-type s) '(unsigned-byte 8)) (read-byte s)
              (read-sequence target s :start 1) target
              (file-position s) (read-byte s nil :end))))`);
  assert.deepEqual(result.slice(0, 3), [T, 255, 3]);
  assert.equal(rt.print(result[3]), '#(0 4 5)');
  assert.deepEqual([result[4], result[5].name], [3, 'END']);
  assert.deepEqual(files.get('/data'), Uint8Array.of(255, 4, 5));
  await assert.rejects(evalAsync(rt, '(with-open-file (s "/data" :element-type \'(unsigned-byte 8)) (read-char s))'), error => error.className === 'STREAM-ERROR');
});

test('LOAD uses the open file stream cursor and leaves it positioned after a nonlocal transfer', async () => {
  const file = '/src/custom.lisp';
  const { host, events } = fixtures({ [file]: '(error "Already read") (throw :names (list *load-pathname* *load-truename*)) 73' });
  const rt = createRuntime({ pathnameHost: host });
  const result = await evalAsync(rt, `(with-open-file (s "${file}")
    (read s)
    (let ((names (catch :names (load s))))
      (values (namestring (first names)) (namestring (second names)) (read s) (open-stream-p s))))`);
  assert.deepEqual(result.map(value => rt.toJS(value)), [file, file, 73, true]);
  assert.deepEqual(events, [['open', file, 'input', 'character'], ['close', file, false]]);
});

test('cancellation aborts a file open that completes after its Lisp task has unwound', async () => {
  let open, release;
  const acquired = new Promise(resolve => { open = resolve; });
  const wasClosed = new Promise(resolve => { release = resolve; });
  const rt = createRuntime({ pathnameHost: { openFile() { return acquired; } } });
  const task = rt.createTask('OPEN', [rt.fromJS('/pending')]);
  const abort = new AbortController();
  const running = task.runAsync({ signal: abort.signal });
  assert.ok(task.suspended);
  abort.abort(new Error('cancelled open'));
  await assert.rejects(running, /cancelled open/);
  open({ readAt: () => '', close: ({ abort }) => release(abort) });
  assert.equal(await wasClosed, true);
});

test('cancellation abandons an in-flight write and awaits asynchronous abort close', async () => {
  let start, finish, close;
  const writing = new Promise(resolve => { start = resolve; });
  const lateWrite = new Promise(resolve => { finish = resolve; });
  const closing = new Promise(resolve => { close = resolve; });
  const aborts = [];
  const rt = createRuntime({ pathnameHost: { openFile() { return {
    writeAt() { start(); return lateWrite; },
    async close({ abort }) { aborts.push(abort); await closing; },
  }; } } });
  const controller = new AbortController();
  const running = rt.evalStringAsync('(with-open-file (s "/out" :direction :output) (write-string "data" s))', { signal: controller.signal });
  await writing;
  controller.abort(new Error('cancelled write'));
  assert.deepEqual(aborts, [true]);
  finish();
  close();
  await assert.rejects(running, /cancelled write/);
  assert.deepEqual(aborts, [true]);
});

test('cancellation during an explicit close records closure before Lisp cleanup', { timeout: 5000 }, async () => {
  let startClose, finishClose;
  const closing = new Promise(resolve => { startClose = resolve; });
  const committed = new Promise(resolve => { finishClose = resolve; });
  const aborts = [];
  const rt = createRuntime({ pathnameHost: { openFile() { return {
    writeAt() {},
    close({ abort }) { aborts.push(abort); startClose(); return committed; },
  }; } } });
  const [stream] = rt.evalString('(open "/out" :direction :output)').values;
  rt.registerFunction('OPENED-STREAM', () => stream);
  const controller = new AbortController(), reason = new Error('cancelled close');
  const running = assert.rejects(rt.evalStringAsync(`
    (let ((s (opened-stream)))
      (unwind-protect (close s)
        (close s :abort t)))`, { signal: controller.signal }), error => error === reason);
  await closing;
  controller.abort(reason);
  await assert.rejects(rt.callAsync('CLOSE', [stream]), { className: 'STREAM-ERROR' });
  finishClose();
  await running;
  assert.deepEqual(aborts, [false]);
  assert.deepEqual(rt.call('OPEN-STREAM-P', [stream]).values, [NIL]);
});
