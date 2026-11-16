import { NIL, T, LispPathname, LispString } from './objects.mjs';
import { LispCondition } from './execution.mjs';
import { keys } from './builtins_helpers.mjs';
import { EOF, Reader } from './reader.mjs';
import { LispFileStream, resolveSynonymStream, streamDirection, withStreamReader } from './file_streams.mjs';
import { compiledModuleHeader, createResidentEmitter } from './compiled_file_emitter.mjs';
import { runCompiledProgram } from './loader.mjs';

export function readerBindings(runtime, task) {
  return ['*PACKAGE*', '*READTABLE*'].map(name => {
    const symbol = runtime.intern(name, 'COMMON-LISP');
    return [symbol, runtime.variableValue(symbol, task)];
  });
}

export function* evalReader(runtime, reader, afterForm) {
  let result = NIL;
  for (;;) {
    const form = yield* reader.readSteps(false, EOF);
    if (form === EOF) return result;
    result = yield* runtime.evalSteps(form);
    if (afterForm) yield* afterForm(result);
  }
}

function fileError(pathname, message) {
  throw new LispCondition('FILE-ERROR', message, { pathname });
}

const missing = file => file == null || file === NIL || file === false;

function* openPathForLoad(runtime, pathname, task, externalFormat) {
  const host = runtime.pathnameHost;
  const readsSource = typeof host?.readFile === 'function';
  const loadsModules = typeof host?.loadModule === 'function';
  const candidates = [pathname];
  if (pathname.components[4] === NIL) {
    if (!readsSource && !loadsModules) fileError(pathname, 'Filesystem host cannot load source or compiled files');
    const suffixes = readsSource ? ['lisp', 'lsp'] : [];
    if (loadsModules) suffixes.push('mjs');
    for (const suffix of suffixes) {
      const components = pathname.components.slice();
      components[4] = new LispString(pathname.logical ? suffix.toUpperCase() : suffix);
      candidates.push(new LispPathname(components, { logical: pathname.logical }));
    }
  }
  for (const candidate of candidates) {
    const type = candidate.components[4];
    const compiled = type instanceof LispString && type.toString().toLowerCase() === 'mjs';
    if (type === NIL && !readsSource) continue;
    const operation = compiled ? 'loadModule' : 'readFile';
    const file = yield* runtime.callPathnameHost(operation, candidate, task, { args: compiled ? [] : [externalFormat] });
    if (missing(file)) continue;
    if (!compiled && typeof file.contents === 'string' && file.contents.startsWith(compiledModuleHeader)) {
      const module = yield* runtime.callPathnameHost('loadModule', candidate, task, { args: [file.contents] });
      if (missing(module)) continue;
      return { compiled: true, file: { ...module, truename: module.truename ?? file.truename } };
    }
    return { file, compiled };
  }
  return null;
}

export function installLispFiles(runtime, compileSource, compileForm) {
  const cl = name => runtime.intern(name, 'COMMON-LISP');
  const keyword = name => runtime.intern(name, 'KEYWORD');
  const special = name => {
    const symbol = cl(`*${name}*`);
    symbol.special = true;
    symbol.value = NIL;
    return runtime.exportSymbol(symbol, 'COMMON-LISP');
  };
  const load = {
    pathname: special('LOAD-PATHNAME'), truename: special('LOAD-TRUENAME'),
    print: special('LOAD-PRINT'), verbose: special('LOAD-VERBOSE'),
  };
  const compile = {
    pathname: special('COMPILE-FILE-PATHNAME'), truename: special('COMPILE-FILE-TRUENAME'),
    print: special('COMPILE-PRINT'), verbose: special('COMPILE-VERBOSE'),
  };
  const enabled = (options, name, symbol, task) => (options.get(name) ?? runtime.readVariable(symbol, task)) !== NIL;
  const print = value => runtime.requestCall(cl('PRINT'), [value]);
  const writeLine = text => runtime.requestCall(cl('WRITE-LINE'), [new LispString(text)]);
  const printValues = function* (result) {
    for (const value of result.values) yield print(value);
  };

  runtime.defineBuiltin('LOAD', function* ([input, ...args], _runtime, task) {
    if (input === undefined) throw new LispCondition('PROGRAM-ERROR', 'LOAD requires a file or input stream');
    const options = keys(args, ['VERBOSE', 'PRINT', 'IF-DOES-NOT-EXIST', 'EXTERNAL-FORMAT']);
    const stream = runtime.streamClassName(input) !== undefined;
    const verbose = enabled(options, 'VERBOSE', load.verbose, task);
    const report = enabled(options, 'PRINT', load.print, task) ? printValues : null;
    task.bind(task.currentFrame, readerBindings(runtime, task));
    let pathname = NIL, truename = NIL, opened;
    if (stream) {
      const source = resolveSynonymStream(runtime, input, task);
      if (!streamDirection(runtime, source, 'input', task)) throw new LispCondition('STREAM-ERROR', 'LOAD requires an open input stream', { stream: input });
      if (source instanceof LispFileStream) {
        source.checkInput(true);
        pathname = source.pathname;
        truename = (yield runtime.requestCall(cl('TRUENAME'), [input])).values[0];
      }
    } else {
      pathname = runtime.mergePathnames(input, task);
      const format = options.get('EXTERNAL-FORMAT') ?? keyword('DEFAULT');
      opened = yield* openPathForLoad(runtime, pathname, task, format);
      if (!opened) {
        if (options.get('IF-DOES-NOT-EXIST') === NIL) return NIL;
        fileError(pathname, 'File does not exist');
      }
      truename = runtime.hostPathname(opened.file.truename, pathname);
    }
    task.bind(task.currentFrame, [[load.pathname, pathname], [load.truename, truename]]);
    if (verbose) yield writeLine(`; Loading ${stream ? 'input stream' : runtime.pathnameNamestring(truename)}`);
    if (opened?.compiled) {
      const { file } = opened;
      const programs = file.programs ?? (file.program ? [file.program] : null);
      if (!Array.isArray(programs)) fileError(pathname, 'Expected a JavaScript compiled module');
      for (const program of programs) yield* runCompiledProgram(runtime, program, report);
    } else if (stream) {
      yield* withStreamReader(runtime, input, task, reader => evalReader(runtime, reader, report));
    } else {
      const source = opened.file.contents;
      if (typeof source !== 'string') fileError(pathname, 'Filesystem host must return decoded Lisp source');
      yield* evalReader(runtime, new Reader(runtime, source), report);
    }
    return T;
  });

  runtime.defineBuiltin('COMPILE-FILE', function* ([input, ...args], _runtime, task) {
    if (input === undefined) throw new LispCondition('PROGRAM-ERROR', 'COMPILE-FILE requires an input pathname');
    const options = keys(args, ['OUTPUT-FILE', 'VERBOSE', 'PRINT', 'EXTERNAL-FORMAT']);
    const pathname = runtime.mergePathnames(input, task);
    const format = options.get('EXTERNAL-FORMAT') ?? keyword('DEFAULT');
    const opened = yield* runtime.callPathnameHost('readFile', pathname, task, { args: [format] });
    if (missing(opened)) fileError(pathname, 'File does not exist');
    if (typeof opened.contents !== 'string') fileError(pathname, 'Filesystem host must return decoded Lisp source');
    const truename = runtime.hostPathname(opened.truename, pathname);
    const output = (yield runtime.requestCall(cl('COMPILE-FILE-PATHNAME'), [pathname, keyword('OUTPUT-FILE'), options.get('OUTPUT-FILE') ?? NIL])).values[0];
    const verbose = enabled(options, 'VERBOSE', compile.verbose, task);
    const onForm = enabled(options, 'PRINT', compile.print, task) ? function* (form) { yield print(form); } : null;
    task.bind(task.currentFrame, [[compile.pathname, pathname], [compile.truename, truename]]);
    const sourcePath = runtime.pathnameNamestring(truename);
    if (verbose) yield writeLine(`; Compiling ${sourcePath}`);
    const emitter = createResidentEmitter(runtime, sourcePath, compileForm);
    const diagnostics = task.pushScope(task.currentFrame, 'compile-file-diagnostics', { warnings: NIL, failure: NIL });
    yield* compileSource(opened.contents, { onEntry: emitter.addEntry, onForm });
    task.popScope('compile-file-diagnostics');
    const written = yield* runtime.callPathnameHost('writeFile', output, task, { args: [emitter.finish()] });
    return runtime.multipleValues(runtime.hostPathname(written, output), diagnostics.warnings, diagnostics.failure);
  });
}
