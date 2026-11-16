import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, realpath, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createRuntime, LispPathname, LispString, NIL, T } from '../runtime/runtime.mjs';

test('Lisp LOAD reads source in one task and restores its pathname and package bindings', async () => {
  const reads = [], observed = [];
  const runtime = createRuntime({ pathnameHost: {
    async readFile(file, _context, format) {
      reads.push([file, format.name]);
      if (file !== '/virtual/load-phase.lisp') return null;
      return {
        truename: '/canonical/load-phase.lisp',
        contents: `
          (defpackage #:loaded-source (:use #:cl))
          (in-package #:loaded-source)
          (record-context (namestring *load-pathname*)
                          (namestring *load-truename*)
                          (package-name *package*))
          (defun load-marker () 42)
        `,
      };
    },
  } });
  const original = runtime.currentPackage;
  let resolve, start;
  const wait = new Promise(yes => { resolve = yes; });
  const started = new Promise(yes => { start = yes; });
  runtime.registerFunction('RECORD-CONTEXT', args => { observed.push(args.map(value => value.toString())); start(); return wait; });
  const load = runtime.callAsync('LOAD', [new LispString('/virtual/load-phase')]);
  await started;
  assert.deepEqual(reads, [['/virtual/load-phase', 'DEFAULT'], ['/virtual/load-phase.lisp', 'DEFAULT']]);
  assert.deepEqual(observed, [['/virtual/load-phase', '/canonical/load-phase.lisp', 'LOADED-SOURCE']]);
  assert.equal(runtime.currentPackage, original);
  assert.deepEqual(runtime.evalString('(values *load-pathname* *load-truename*)').values, [NIL, NIL]);
  resolve(NIL);
  assert.deepEqual((await load).values, [T]);
  assert.deepEqual(runtime.call('LOADED-SOURCE::LOAD-MARKER').values, [42]);
  assert.equal(runtime.currentPackage, original);
  assert.deepEqual(await runtime.evalStringAsync('(load "/virtual/no-file.lisp" :if-does-not-exist nil)').then(r => r.values), [NIL]);
  assert.deepEqual(runtime.evalString('(load (make-string-input-stream "(defparameter *stream-loaded* 17)")) *stream-loaded*').values, [17]);
});

test('Lisp COMPILE-FILE writes a portable generated module and loads fresh literal graphs', async () => {
  const directory = await mkdtemp(join(process.env.ZKCL_JS_TEST_TMP ?? tmpdir(), 'zkcl-module-'));
  const pathnameHost = {
    async readFile(file) { return { contents: await readFile(file, 'utf8'), truename: await realpath(file) }; },
    async writeFile(file, _context, source) { await writeFile(file, source, 'utf8'); return realpath(file); },
    async loadModule(file, _context, source) {
      const url = source === undefined ? pathToFileURL(file).href : `data:text/javascript,${encodeURIComponent(source)}`;
      return { ...await import(url), truename: await realpath(file) };
    },
  };
  try {
    const input = join(directory, 'input.lisp'), output = join(directory, 'input.mjs');
    const state = '#Y"E220A8397B1DCDAF6E789E6AA1B965F406C45D188009454FF88BB8A8724C81EC"';
    await writeFile(input, `
      (defpackage #:compiled-source (:use #:cl) (:export #:read-values))
      (in-package #:compiled-source)
      (eval-when (:compile-toplevel)
        (record-source (namestring *compile-file-pathname*) (namestring *compile-file-truename*)))
      (defun read-values ()
        (values (load-time-value (initialize-cell)) '#1=(a . #1#)
                #P"data.bin" 9007199254740993 -0.0d0 #2A((1 2)(3 4)) ${state}
                #.(find-package :compiled-source)))
    `, 'utf8');
    const compiler = createRuntime({ pathnameHost }), inputs = [];
    compiler.registerFunction('RECORD-SOURCE', args => { inputs.push(args.map(value => value.toString())); return Promise.resolve(NIL); });
    compiler.registerFunction('INITIALIZE-CELL', () => assert.fail('LOAD-TIME-VALUE must wait for loading'));
    const [artifact, warnings, failure] = (await compiler.callAsync('COMPILE-FILE', [new LispString(input)])).values;
    assert.ok(artifact instanceof LispPathname);
    assert.deepEqual([warnings, failure], [NIL, NIL]);
    assert.deepEqual(inputs, [[input, await realpath(input)]]);
    assert.equal(compiler.pathnameNamestring(artifact), await realpath(output));
    assert.deepEqual(compiler.evalString('(values *compile-file-pathname* *compile-file-truename*)').values, [NIL, NIL]);
    const { program } = await import(pathToFileURL(output).href);
    assert.equal(program.target, 'js');
    assert.ok(program.entries.every(entry => typeof entry.step === 'function' && !Object.hasOwn(entry, 'instructions')));

    const receiver = createRuntime({ pathnameHost });
    const initialized = [];
    receiver.registerFunction('INITIALIZE-CELL', (_args, rt) => {
      const value = rt.fromJS(`value-${initialized.length + 1}`);
      initialized.push(value);
      return Promise.resolve(rt.multipleValues(value, NIL));
    });
    const load = () => receiver.callAsync('LOAD', [new LispString(output)]);
    assert.deepEqual((await load()).values, [T]);
    const previous = receiver.readFunction('COMPILED-SOURCE:READ-VALUES');
    const [first, firstCycle, path, integer, negativeZero, array, randomState, packageObject] = receiver.call(previous).values;
    assert.equal(first, initialized[0]);
    assert.equal(firstCycle.cdr, firstCycle);
    assert.equal(receiver.pathnameNamestring(path), 'data.bin');
    assert.equal(integer, 9007199254740993n);
    assert.ok(Object.is(negativeZero.value, -0));
    assert.equal(array.aref(1, 0), 3);
    assert.equal(receiver.print(randomState), state);
    assert.equal(packageObject, receiver.findPackage('COMPILED-SOURCE'));
    assert.notEqual(packageObject, compiler.findPackage('COMPILED-SOURCE'));
    assert.equal(receiver.call(previous).values[6], randomState);
    assert.equal(receiver.call('RANDOM', [37, randomState]).values[0], receiver.call('RANDOM', [37]).values[0]);
    assert.deepEqual((await load()).values, [T]);
    const [second, secondCycle, , , , , secondRandomState] = receiver.call('COMPILED-SOURCE:READ-VALUES').values;
    assert.equal(second, initialized[1]);
    assert.notEqual(secondCycle, firstCycle);
    assert.equal(secondCycle.cdr, secondCycle);
    assert.notEqual(secondRandomState, randomState);
    assert.equal(receiver.print(secondRandomState), state);
    assert.equal(receiver.call(previous).values[0], first);

    const explicitOutput = join(directory, 'named.fasl');
    const [named] = (await compiler.callAsync('COMPILE-FILE', [new LispString(input), compiler.intern('OUTPUT-FILE', 'KEYWORD'), new LispString(explicitOutput)])).values;
    assert.equal(compiler.pathnameNamestring(named), await realpath(explicitOutput));
    assert.deepEqual((await receiver.callAsync('LOAD', [new LispString(explicitOutput)])).values, [T]);
    assert.equal(receiver.call('COMPILED-SOURCE:READ-VALUES').values[0], initialized[2]);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test('cancelled COMPILE-FILE runs Lisp cleanup with file bindings before abandoning output', async () => {
  const events = [];
  const pathnameHost = {
    readFile() { return {
      truename: '/canonical/cancelled.lisp',
      contents: `
        (eval-when (:compile-toplevel)
          (unwind-protect (compile-gate)
            (compile-note (namestring *compile-file-truename*))))
        (eval-when (:compile-toplevel) (compile-note "later form"))
      `,
    }; },
    writeFile() { events.push('wrote file'); return '/canonical/cancelled.mjs'; },
  };
  const runtime = createRuntime({ pathnameHost }), controller = new AbortController();
  let unblock, begin;
  const wait = new Promise(yes => { unblock = yes; });
  const started = new Promise(yes => { begin = yes; });
  runtime.registerFunction('COMPILE-GATE', () => { begin(); return wait; });
  runtime.registerFunction('COMPILE-NOTE', ([value]) => { events.push(value.toString()); return NIL; });
  const compiling = runtime.callAsync('COMPILE-FILE', [new LispString('/virtual/cancelled.lisp')], { signal: controller.signal });
  await started;
  const reason = new Error('cancel compilation');
  controller.abort(reason);
  await assert.rejects(compiling, error => error === reason);
  unblock(NIL);
  await Promise.resolve();
  assert.deepEqual(events, ['/canonical/cancelled.lisp']);
  assert.deepEqual(runtime.evalString('(values *compile-file-pathname* *compile-file-truename*)').values, [NIL, NIL]);
});
