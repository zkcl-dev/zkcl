import { NIL, T, Cons, LispArray, fromArray, equal, toArray } from './objects.mjs';
import { integer, isInteger } from './numbers.mjs';
import { LispCondition, NativeFunction } from './execution.mjs';
import { Reader } from './reader.mjs';

const primary = result => result.values[0] ?? NIL;
const streamError = (stream, message, cause) => { throw new LispCondition('STREAM-ERROR', message, { stream, pathname: stream.pathname, cause }); };
const typeError = (datum, expectedType) => { throw new LispCondition('TYPE-ERROR', `Expected ${expectedType}`, { datum, expectedType }); };
const validPosition = value => Number.isSafeInteger(value) && value >= 0;
const scalarText = value => typeof value === 'string' && !/[\uD800-\uDFFF]/u.test(value);
const deviceCall = new NativeFunction('FILE-STREAM-DEVICE-OPERATION', ([stream, method, args], _, task) => {
  const operation = stream.handle[method];
  if (typeof operation !== 'function') streamError(stream, `File handle does not support ${method}`);
  const frame = task.currentFrame, epoch = frame?.resumeEpoch;
  const fail = error => {
    if (task.done || task.currentFrame !== frame || frame?.resumeEpoch !== epoch || error instanceof LispCondition) throw error;
    return streamError(stream, error instanceof Error ? error.message : String(error), error);
  };
  let result;
  try { result = operation.apply(stream.handle, args); }
  catch (error) { return fail(error); }
  if (result && typeof result.then === 'function') {
    if (method === 'readNoHangAt') {
      Promise.resolve(result).catch(() => {});
      streamError(stream, 'A nonblocking file read must return synchronously');
    }
    return Promise.resolve(result).catch(fail);
  }
  return result;
});

export class LispFileStream {
  #closing = false;

  constructor(runtime, pathname, handle, { direction, elementType, ifExists, externalFormat }) {
    this.runtime = runtime;
    this.pathname = pathname;
    this.handle = handle;
    this.input = direction === 'input' || direction === 'io';
    this.output = direction === 'output' || direction === 'io';
    this.character = elementType === 'character';
    this.externalFormat = externalFormat;
    this.open = true;
    this.position = 0;
    this.column = 0;
    this.append = this.output && ifExists === 'append';
    this.busy = false;
    this.buffer = null;
    this.readerTask = null;
    this.lastRead = null;
  }
  checkOpen() {
    if (!this.open) streamError(this, 'File stream is closed');
    if (this.#closing) streamError(this, 'File stream is already closing');
  }
  checkInput(character = this.character) {
    this.checkOpen();
    if (!this.input || this.character !== character) streamError(this, `Expected an open ${character ? 'character' : 'binary'} input stream`);
  }
  checkOutput(character = this.character) {
    this.checkOpen();
    if (!this.output || this.character !== character) streamError(this, `Expected an open ${character ? 'character' : 'binary'} output stream`);
  }
  *call(method, ...args) { return primary(yield this.runtime.requestCall(deviceCall, [this, method, args])); }
  *exclusive(action) {
    if (this.busy) streamError(this, 'File stream already has an active operation');
    this.busy = true;
    try { return yield* action(); }
    finally { this.busy = false; }
  }
  *size() {
    const size = yield* this.call('length');
    if (size === NIL) return null;
    if (!validPosition(size)) streamError(this, 'File handle length must be a nonnegative safe integer');
    return size;
  }
  *initialize() {
    const initialPosition = this.handle.position;
    const position = this.append && initialPosition === undefined ? yield* this.size() : initialPosition ?? 0;
    if (position === null) streamError(this, 'File length is required for append');
    if (!validPosition(position)) streamError(this, 'File handle position must be a nonnegative safe integer');
    this.position = position;
    this.column = position === 0 ? 0 : null;
  }
  bufferedText(amount, at = this.position) {
    if (!this.buffer) return undefined;
    const { start, reader } = this.buffer;
    const position = at - start;
    if (position < 0 || position >= reader.characterOffsets.length) return undefined;
    return reader.source.slice(reader.characterOffsets[position], amount === null ? undefined : reader.characterOffsets[position + amount]);
  }
  *readAt(amount, noHang = false, at = this.position) {
    const buffered = this.character ? this.bufferedText(amount, at) : undefined;
    if (buffered !== undefined) return buffered;
    if (noHang && typeof this.handle.readNoHangAt !== 'function') return NIL;
    const data = yield* this.call(noHang ? 'readNoHangAt' : 'readAt', at, amount);
    if (noHang && data === NIL) return NIL;
    if (this.character ? !scalarText(data) : !(data instanceof Uint8Array)) streamError(this, 'File handle returned invalid input data');
    const length = this.character ? [...data].length : data.length;
    if (amount !== null && length > amount) streamError(this, 'File handle returned too many elements');
    if (!validPosition(at + length)) streamError(this, 'File input position is outside the supported range');
    return data;
  }
  *readData(amount, { character = this.character, noHang = false, unread = false } = {}) {
    this.checkInput(character);
    const stream = this;
    return yield* this.exclusive(function* () {
      const data = yield* stream.readAt(amount, noHang);
      if (data === NIL) return NIL;
      const length = character ? [...data].length : data.length;
      stream.position += length;
      stream.lastRead = unread && length === 1 ? data : null;
      return data;
    });
  }
  unreadCharacter(value) {
    this.checkInput(true);
    if (this.busy || this.lastRead !== value) streamError(this, 'Character was not the last character read');
    this.position--;
    this.lastRead = null;
    return NIL;
  }
  activeReader(task) { return this.readerTask === task && task != null ? this.buffer.reader : null; }
  *withReader(task, action) {
    this.checkInput(true);
    const active = this.activeReader(task);
    if (active) return yield* action(active);
    const stream = this;
    return yield* this.exclusive(function* () {
      if (!stream.buffer || stream.position < stream.buffer.start || stream.position - stream.buffer.start >= stream.buffer.reader.characterOffsets.length) {
        const text = yield* stream.readAt(null);
        stream.buffer = { start: stream.position, reader: new Reader(stream.runtime, text, { stream }) };
      }
      const { start, reader } = stream.buffer;
      reader.index = reader.characterOffsets[stream.position - start];
      stream.readerTask = task;
      stream.lastRead = null;
      try { return yield* action(reader); }
      finally {
        stream.position = start + reader.characterOffsets.indexOf(reader.index);
        stream.readerTask = null;
        if (!stream.open) stream.buffer = null;
      }
    });
  }
  *writeSteps(data) {
    const character = typeof data === 'string';
    this.checkOutput(character);
    const stream = this;
    return yield* this.exclusive(function* () {
      const length = character ? [...data].length : data.length;
      if (!validPosition(stream.position + length)) streamError(stream, 'File output position is outside the supported range');
      yield* stream.call('writeAt', stream.position, data);
      stream.position += length;
      stream.buffer = null;
      stream.lastRead = null;
      if (character) {
        const last = data.lastIndexOf('\n');
        if (last >= 0) stream.column = [...data.slice(last + 1)].length;
        else if (stream.column !== null) stream.column += length;
      }
      return NIL;
    });
  }
  *filePosition(task, target) {
    this.checkOpen();
    const reader = this.activeReader(task);
    if (target === undefined) return integer(reader ? this.buffer.start + reader.characterOffsets.indexOf(reader.index) : this.position);
    if (reader) {
      const offset = target === 'end' ? reader.end : reader.characterOffsets[target - this.buffer.start];
      if (offset === undefined) return NIL;
      reader.index = offset;
      return T;
    }
    const stream = this;
    return yield* this.exclusive(function* () {
      const position = target === 'end' ? yield* stream.size() : target;
      if (position === null) return NIL;
      stream.position = position;
      stream.column = position === 0 ? 0 : null;
      stream.lastRead = null;
      stream.buffer = null;
      return T;
    });
  }
  *fileLength() {
    this.checkOpen();
    const stream = this;
    return yield* this.exclusive(function* () { const size = yield* stream.size(); return size === null ? NIL : integer(size); });
  }
  *flush(method) {
    this.checkOutput();
    const stream = this;
    return yield* this.exclusive(function* () {
      if (typeof stream.handle[method] === 'function') yield* stream.call(method);
      return NIL;
    });
  }
  *close(abort, task = this.runtime.activeTask) {
    if (!this.open) return NIL;
    if (this.#closing) streamError(this, 'File stream is already closing');
    const stream = this;
    function* close() {
      // A close can commit output in the host. Finish recording its outcome
      // before Lisp cancellation can attempt to close the same handle again.
      const shield = task.pushScope(task.currentFrame, 'cancel-shield');
      stream.#closing = true;
      try {
        yield* stream.call('close', { abort });
        stream.open = false;
        if (!stream.readerTask) stream.buffer = null;
        return T;
      } finally {
        stream.#closing = false;
        if (task.scopes.at(-1) === shield) task.popScope('cancel-shield');
      }
    }
    if (this.activeReader(task)) return yield* close();
    return yield* this.exclusive(close);
  }
}

export const isWritableStream = value => value instanceof LispFileStream ? value.output : value != null && typeof value.write === 'function';
const writeOutput = new NativeFunction('WRITE-STREAM-TEXT', ([stream, text]) => stream.write(text));
export function* writeStreamSteps(runtime, stream, text, task = runtime.activeTask, stack = new Set()) {
  stream = resolveSynonymStream(runtime, stream, task, true, stack);
  checkStream(stream);
  if (stream instanceof CompositeStream) {
    stack = enter(stream, stack);
    if (stream.kind === 'broadcast') {
      for (const output of stream.streams) yield* writeStreamSteps(runtime, output, text, task, stack);
    } else if (stream.outputStream) yield* writeStreamSteps(runtime, stream.outputStream, text, task, stack);
    else streamError(stream, 'Stream cannot accept output');
    return NIL;
  }
  if (stream instanceof LispFileStream) return yield* stream.writeSteps(text);
  if (typeof text !== 'string' || !isWritableStream(stream)) return typeError(stream, 'OUTPUT-STREAM');
  yield runtime.requestCall(writeOutput, [stream, text]);
  return NIL;
}

export function* openFile(runtime, task, path, options) {
  const keyword = value => value?.package === runtime.packages.get('KEYWORD') ? value.name : null;
  const choice = (value, allowed, fallback, nilAllowed = false) => {
    if (value === undefined) return fallback;
    if (nilAllowed && value === NIL) return null;
    const name = keyword(value);
    if (!allowed.includes(name)) typeError(value, `(MEMBER ${allowed.map(name => `:${name}`).join(' ')}${nilAllowed ? ' NIL' : ''})`);
    return name.toLowerCase();
  };
  const direction = choice(options.get('DIRECTION'), ['INPUT', 'OUTPUT', 'IO', 'PROBE'], 'input');
  const element = options.get('ELEMENT-TYPE');
  let elementType = 'character';
  if (element !== undefined && keyword(element) !== 'DEFAULT' && ![T, ...['CHARACTER', 'BASE-CHAR', 'STANDARD-CHAR'].map(name => runtime.intern(name, 'COMMON-LISP'))].includes(element)) {
    const [type, width, ...rest] = element instanceof Cons ? toArray(element) : [];
    if (type !== runtime.intern('UNSIGNED-BYTE', 'COMMON-LISP') || width !== 8 || rest.length) {
      typeError(element, '(OR (MEMBER :DEFAULT CHARACTER BASE-CHAR STANDARD-CHAR T) (EQL (UNSIGNED-BYTE 8)))');
    }
    elementType = 'octet';
  }
  const pathname = runtime.mergePathnames(path, task);
  const writing = direction === 'output' || direction === 'io';
  const ifExists = choice(options.get('IF-EXISTS'), ['ERROR', 'NEW-VERSION', 'RENAME', 'RENAME-AND-DELETE', 'OVERWRITE', 'APPEND', 'SUPERSEDE'],
    writing && pathname.components[5]?.name === 'NEWEST' ? 'new-version' : 'error', true);
  const ifDoesNotExist = choice(options.get('IF-DOES-NOT-EXIST'), ['ERROR', 'CREATE'],
    direction === 'probe' ? null : writing && ifExists !== 'overwrite' && ifExists !== 'append' ? 'create' : 'error', true);
  const externalFormat = options.get('EXTERNAL-FORMAT') ?? runtime.intern('DEFAULT', 'KEYWORD');
  const descriptor = { direction, elementType, ifExists, ifDoesNotExist, externalFormat };
  const fileFault = message => { throw new LispCondition('FILE-ERROR', message, { pathname }); };
  let stream, acquired, abandoned = false, initialized = false;
  const receive = (handle, cancelled) => {
    if (typeof handle?.close === 'function') {
      if (abandoned || cancelled) return handle.close({ abort: true });
      acquired = handle;
    }
    return handle;
  };
  try {
    const handle = yield* runtime.callPathnameHost('openFile', pathname, task, { args: [descriptor], receive });
    if (handle === NIL) {
      if (ifDoesNotExist === null || writing && ifExists === null) return NIL;
      return fileFault('Filesystem host did not open the file');
    }
    if (!acquired) return fileFault('Filesystem host did not return a file handle with CLOSE');
    stream = new LispFileStream(runtime, pathname, handle, descriptor);
    if (stream.input && typeof handle.readAt !== 'function' || stream.output && typeof handle.writeAt !== 'function') fileFault('Filesystem host file handle does not support the requested direction');
    yield* stream.initialize();
    if (direction === 'probe') yield* stream.close(false, task);
    initialized = true;
    return stream;
  } finally {
    abandoned = true;
    if (!initialized && acquired) {
      stream ??= new LispFileStream(runtime, pathname, acquired, descriptor);
      if (stream.open) yield* stream.close(true, task);
    }
  }
}

export function* readFileOctets([pathname, start, end], runtime, task) {
  const offset = value => {
    if (!isInteger(value) || value < 0 || value > Number.MAX_SAFE_INTEGER) {
      typeError(value, '(INTEGER 0 9007199254740991)');
    }
    return Number(value);
  };
  const first = offset(start), length = end === NIL ? null : offset(end) - first;
  if (length !== null && length < 0) throw new LispCondition('PROGRAM-ERROR', 'File :END precedes :START');
  const octet = fromArray([runtime.intern('UNSIGNED-BYTE', 'COMMON-LISP'), 8]);
  // The task owns the open/read/close sequence. Let the pending device call
  // finish and close its handle before delivering an enclosing cancellation.
  task.pushScope(task.currentFrame, 'cancel-shield');
  const stream = yield* openFile(runtime, task, pathname, new Map([['ELEMENT-TYPE', octet]]));
  let complete = false;
  try {
    const bytes = yield* stream.readAt(length, false, first);
    const result = new LispArray([bytes.length], { elementType: 'Octet', initialContents: Array.from(bytes) });
    complete = true;
    return result;
  } finally { yield* stream.close(!complete, task); }
}

export function filePositionDesignator(value) {
  if (value?.package?.name === 'KEYWORD') {
    if (value.name === 'START') return 0;
    if (value.name === 'END') return 'end';
  }
  if (!isInteger(value) || value < 0 || value > Number.MAX_SAFE_INTEGER) typeError(value, '(OR (MEMBER :START :END) (INTEGER 0))');
  return Number(value);
}

export class StringStream {
  constructor(
    text = "",
    { input = false, start = 0, end = [...text].length, column = 0 } = {},
  ) {
    this.text = text;
    this.input = input;
    this.characters = input ? [...text] : null;
    this.position = start;
    this.end = end;
    this.open = true;
    const newline = text.lastIndexOf("\n");
    this._column = [...text.slice(newline + 1)].length + (newline < 0 ? column : 0);
  }
  get column() { return this.layout?.column ?? this._column; }
  write(text, literal = false) {
    if (!this.open || this.input)
      streamError(this, "Stream is not open for output");
    if (this.layout) this.layout.text(text, literal);
    else {
      this.text += text;
      const newline = text.lastIndexOf("\n");
      this._column = [...text.slice(newline + 1)].length + (newline < 0 ? this._column : 0);
    }
  }
}

const streamClasses = {
  twoWay: 'TWO-WAY-STREAM', echo: 'ECHO-STREAM', synonym: 'SYNONYM-STREAM',
  broadcast: 'BROADCAST-STREAM', concatenated: 'CONCATENATED-STREAM',
};
export class CompositeStream {
  constructor(kind, fields) {
    this.kind = kind;
    this.open = true;
    Object.assign(this, fields);
  }
}
export function streamClassName(runtime, stream) {
  if (stream instanceof LispFileStream) return 'FILE-STREAM';
  if (stream instanceof StringStream) return 'STRING-STREAM';
  if (stream instanceof CompositeStream) return streamClasses[stream.kind];
  if (stream instanceof Reader || stream === runtime.output || stream === runtime.errorOutput && stream !== undefined) return 'STREAM';
}
function checkStream(stream) {
  if (stream?.open === false) streamError(stream, 'Stream is closed');
}
function enter(stream, stack) {
  if (stack.has(stream)) streamError(stream, 'Circular stream composition');
  return new Set(stack).add(stream);
}
export function resolveSynonymStream(runtime, stream, task = runtime.activeTask, check = true, stack = new Set()) {
  while (stream instanceof CompositeStream && stream.kind === 'synonym') {
    if (check) checkStream(stream);
    stack = enter(stream, stack);
    stream = runtime.readVariable(stream.symbol, task);
    if (streamClassName(runtime, stream) === undefined) typeError(stream, 'STREAM');
  }
  return stream;
}
export function streamOutputEndpoint(runtime, stream, task = runtime.activeTask, stack = new Set()) {
  stream = resolveSynonymStream(runtime, stream, task, true, stack);
  if (!(stream instanceof CompositeStream)) return stream;
  checkStream(stream);
  const endpoint = stream.outputStream;
  return endpoint === undefined ? stream : streamOutputEndpoint(runtime, endpoint, task, enter(stream, stack));
}
export function streamFileEndpoint(runtime, stream, task = runtime.activeTask, stack = new Set()) {
  stream = resolveSynonymStream(runtime, stream, task, true, stack);
  if (!(stream instanceof CompositeStream)) return stream;
  checkStream(stream);
  const target = stream.kind === 'broadcast' ? stream.streams.at(-1) : stream.outputStream;
  return target === undefined ? stream : streamFileEndpoint(runtime, target, task, enter(stream, stack));
}
export function streamDirection(runtime, stream, direction, task = runtime.activeTask) {
  stream = resolveSynonymStream(runtime, stream, task, false);
  if (stream instanceof CompositeStream) {
    if (stream.kind === 'twoWay' || stream.kind === 'echo') return true;
    return direction === 'input' ? stream.kind === 'concatenated' : stream.kind === 'broadcast';
  }
  if (stream instanceof Reader) return direction === 'input';
  if (stream instanceof StringStream) return direction === 'input' ? stream.input : !stream.input;
  if (stream instanceof LispFileStream) return stream[direction];
  return direction === 'output' && isWritableStream(stream);
}
export function streamOpen(runtime, stream, task = runtime.activeTask) {
  if (stream?.open === false) return false;
  const target = resolveSynonymStream(runtime, stream, task, false);
  return streamClassName(runtime, target) !== undefined && target.open !== false;
}
export function streamCharacterMode(runtime, stream, direction, task = runtime.activeTask, stack = new Set()) {
  stream = resolveSynonymStream(runtime, stream, task, true, stack);
  if (!streamDirection(runtime, stream, direction, task)) typeError(stream, direction === 'input' ? 'INPUT-STREAM' : 'OUTPUT-STREAM');
  checkStream(stream);
  if (stream instanceof LispFileStream) return stream.character;
  if (!(stream instanceof CompositeStream)) return true;
  stack = enter(stream, stack);
  const target = stream.kind === 'broadcast' ? stream.streams.at(-1)
    : stream.kind === 'concatenated' ? stream.streams[0] : stream[`${direction}Stream`];
  return target === undefined || streamCharacterMode(runtime, target, direction, task, stack);
}
export function streamElementType(runtime, stream, task = runtime.activeTask, stack = new Set()) {
  stream = resolveSynonymStream(runtime, stream, task, false, stack);
  const cl = name => runtime.intern(name, 'COMMON-LISP');
  if (stream instanceof LispFileStream && !stream.character) return fromArray([cl('UNSIGNED-BYTE'), 8]);
  if (stream instanceof CompositeStream) {
    stack = enter(stream, stack);
    if (stream.kind === 'broadcast') return stream.streams.length ? streamElementType(runtime, stream.streams.at(-1), task, stack) : T;
    if (stream.kind === 'concatenated') return stream.streams.length ? streamElementType(runtime, stream.streams[0], task, stack) : T;
    const input = streamElementType(runtime, stream.inputStream, task, stack), output = streamElementType(runtime, stream.outputStream, task, stack);
    return equal(input, output) ? input : fromArray([cl('OR'), input, output]);
  }
  if (stream instanceof Reader || stream instanceof StringStream || stream instanceof LispFileStream || isWritableStream(stream)) return cl('CHARACTER');
  return typeError(stream, 'STREAM');
}
export function* flushStream(runtime, stream, method, stack = new Set()) {
  stream = resolveSynonymStream(runtime, stream, runtime.activeTask, true, stack);
  checkStream(stream);
  if (stream instanceof CompositeStream) {
    stack = enter(stream, stack);
    if (stream.kind === 'broadcast') {
      for (const output of stream.streams) yield* flushStream(runtime, output, method, stack);
    } else if (stream.outputStream) yield* flushStream(runtime, stream.outputStream, method, stack);
    else streamError(stream, 'Stream cannot accept output');
    return NIL;
  }
  if (stream instanceof LispFileStream) return yield* stream.flush(method);
  if (!streamDirection(runtime, stream, 'output')) typeError(stream, 'STREAM');
  if (typeof stream[method] === 'function') yield runtime.requestCall(new NativeFunction(method, () => stream[method]()));
  return NIL;
}


// Reading a composition pulls from its components on demand. The path to the
// leaf carries the echo destinations and the concatenations it passed through.
const activeReaders = new WeakMap(), lastReads = new WeakMap();
export function activeStreamReader(stream, task) {
  return stream instanceof Reader ? stream : stream instanceof LispFileStream ? stream.activeReader(task) : activeReaders.get(task)?.get(stream);
}
function remember(stream, position) { lastReads.set(stream, position); }
function lookupRead(stream) { return lastReads.get(stream); }
function checkCharacterMode(stream, character) {
  if (stream instanceof LispFileStream) stream.checkInput(character);
  else if (!character || !(stream instanceof StringStream && stream.input || stream instanceof Reader)) streamError(stream, `Expected a ${character ? 'character' : 'binary'} input stream`);
}
function* locateInput(runtime, original, task, offset, options, route = [], stack = new Set()) {
  const stream = resolveSynonymStream(runtime, original, task, true, stack);
  checkStream(stream);
  if (stream instanceof CompositeStream) {
    stack = enter(stream, stack);
    if (stream.kind === 'broadcast') streamError(stream, 'Stream cannot accept input');
    if (stream.kind === 'concatenated') {
      let at = 0;
      const commit = offset === 0;
      while (at < stream.streams.length) {
        const leaf = yield* locateInput(runtime, stream.streams[at], task, offset, options, route, stack);
        if (leaf.value !== undefined) return leaf;
        if (commit && !leaf.notReady) stream.streams.shift();
        else at++;
        offset = leaf.offset;
      }
      return { offset };
    }
    const next = stream.kind === 'echo' ? [...route, stream] : route;
    return yield* locateInput(runtime, stream.inputStream, task, offset, options, next, stack);
  }
  checkCharacterMode(stream, options.character);
  const reader = activeStreamReader(stream, task);
  if (reader) {
    let at = 0, value;
    for (; at <= offset; at++) {
      value = yield* reader.peekSteps(at);
      if (value === undefined) break;
    }
    return { stream, reader, position: reader.index, value, offset: offset - at, route };
  }
  const position = stream.position;
  if (stream instanceof LispFileStream) {
    const data = yield* stream.exclusive(() => stream.readAt(offset + 1, options.noHang, position));
    if (data === NIL) return { value: NIL, notReady: true, stream, route, position };
    const items = options.character ? [...data] : data;
    return { stream, position, value: items[offset], offset: offset - items.length, route };
  }
  const value = position + offset >= stream.end ? undefined : stream.characters[position + offset];
  return { stream, position, value, offset: offset - (stream.end - position), route };
}
function sameLocation(a, b) { return a && b && a.stream === b.stream && a.position === b.position && a.value === b.value; }
export function* peekStreamInput(runtime, stream, task, offset = 0, { character = true, noHang = false } = {}) {
  const result = yield* locateInput(runtime, stream, task, offset, { character, noHang });
  return result.value;
}
export function* takeStreamInput(runtime, original, task, { character = true, noHang = false } = {}) {
  const root = resolveSynonymStream(runtime, original, task);
  const location = yield* locateInput(runtime, root, task, 0, { character, noHang });
  const { stream, reader, position, value, route } = location;
  if (value === undefined || value === NIL) {
    if (value !== NIL && root instanceof CompositeStream) remember(root, null);
    return value;
  }
  if (reader) {
    if (reader.index !== position) streamError(stream, 'Input cursor changed during read');
    yield* reader.takeSteps();
  } else {
    checkStream(stream);
    if (stream.position !== position) streamError(stream, 'Input cursor changed during read');
    stream.position++;
    stream.lastRead = character ? value : null;
  }
  if (root instanceof CompositeStream) {
    const previous = lookupRead(root);
    remember(root, { ...location, unread: false });
    for (const echo of route.slice().reverse()) {
      const pending = lookupRead(echo);
      const echoed = (previous?.unread && previous.route.includes(echo) && sameLocation(previous, location)) || (pending?.unread && sameLocation(pending, location));
      remember(echo, { ...location, unread: false });
      if (!echoed) yield* writeStreamSteps(runtime, echo.outputStream, character ? value : Uint8Array.of(value), task);
    }
  }
  return value;
}
export function* readStreamElement(runtime, original, task, { character = true, noHang = false } = {}) {
  const stream = resolveSynonymStream(runtime, original, task);
  const reader = activeStreamReader(stream, task);
  if (reader) {
    if (!character) streamError(stream, 'Expected a binary input stream');
    return yield* reader.takeSteps();
  }
  return yield* takeStreamInput(runtime, stream, task, { character, noHang });
}
export function* unreadStreamCharacter(runtime, original, task, character) {
  const stream = resolveSynonymStream(runtime, original, task);
  const reader = activeStreamReader(stream, task);
  return reader ? yield* reader.unreadSteps(character) : yield* unreadStreamInput(runtime, stream, task, character);
}
export function* unreadStreamInput(runtime, original, task, character) {
  const root = resolveSynonymStream(runtime, original, task);
  checkStream(root);
  if (root instanceof CompositeStream) {
    const location = lookupRead(root);
    if (!location || location.unread || location.value !== character) streamError(root, 'Character was not the last character read');
    const { stream, reader, position } = location;
    if (reader) {
      if (reader.index !== position + character.length) streamError(root, 'Input cursor advanced since its last read');
      yield* reader.unreadSteps(character);
    } else {
      if (stream.position !== position + 1) streamError(root, 'Input cursor advanced since its last read');
      stream.position = position;
      stream.lastRead = null;
    }
    remember(root, { ...location, unread: true });
    for (const echo of location.route) remember(echo, { ...location, unread: true });
    return NIL;
  }
  if (root instanceof Reader) return yield* root.unreadSteps(character);
  if (root instanceof LispFileStream) return root.unreadCharacter(character);
  checkCharacterMode(root, true);
  if (root.lastRead !== character || root.position === 0) streamError(root, 'Character was not the last character read');
  root.position--;
  root.lastRead = null;
  return NIL;
}
export function* withStreamReader(runtime, original, task, action) {
  const stream = resolveSynonymStream(runtime, original, task);
  checkStream(stream);
  if (stream instanceof Reader) return yield* action(stream);
  if (stream instanceof LispFileStream) return yield* stream.withReader(task, action);
  const active = activeStreamReader(stream, task);
  if (active) return yield* action(active);
  let reader;
  if (stream instanceof CompositeStream) {
    const cursor = {
      peek: offset => peekStreamInput(runtime, stream, task, offset),
      take: () => takeStreamInput(runtime, stream, task),
      unread: character => unreadStreamInput(runtime, stream, task, character),
      checkOpen() { checkStream(stream); },
    };
    reader = new Reader(runtime, '', { stream, cursor });
  } else {
    checkCharacterMode(stream, true);
    reader = new Reader(runtime, stream.text, { start: stream.position, end: stream.end, stream });
  }
  let readers = activeReaders.get(task);
  if (!readers) { readers = new Map(); activeReaders.set(task, readers); }
  readers.set(stream, reader);
  try { return yield* action(reader); }
  finally {
    if (stream instanceof StringStream) {
      stream.position = reader.characterOffsets.indexOf(reader.index);
      stream.lastRead = stream.position > 0 ? stream.characters[stream.position - 1] : null;
    }
    readers.delete(stream);
  }
}
