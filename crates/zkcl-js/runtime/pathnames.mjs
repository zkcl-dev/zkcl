import { NIL, T, UNBOUND, LispString, LispArray, LispSymbol, LispPathname, fromArray, toArray, equal } from './objects.mjs';
import { FIXNUM_MAX, integer, isInteger } from './numbers.mjs';
import { LispCondition, NativeFunction } from './execution.mjs';
import { LispFileStream, resolveSynonymStream } from './file_streams.mjs';

const fields = ['HOST', 'DEVICE', 'DIRECTORY', 'NAME', 'TYPE', 'VERSION'];
const marker = value => value instanceof LispSymbol ? value.name : null;
const missing = value => value === NIL || marker(value) === 'UNSPECIFIC';
const isString = value => typeof value === 'string' || value instanceof LispString || value instanceof LispArray && value.rank === 1 && value.elementKind === 'character';
const string = value => {
  if (typeof value === 'string' || value instanceof LispString) return value.toString();
  if (isString(value)) return Array.from({ length: value.length }, (_, i) => value.get(i).toString()).join('');
  throw new LispCondition('TYPE-ERROR', 'Expected a string', { datum: value, expectedType: 'STRING' });
};
const s = text => new LispString(text);
const upper = text => text.replace(/[a-z]/g, c => c.toUpperCase());
const lower = text => text.replace(/[A-Z]/g, c => c.toLowerCase());
const flipCase = text => /[a-z]/.test(text) && !/[A-Z]/.test(text) ? upper(text) : /[A-Z]/.test(text) && !/[a-z]/.test(text) ? lower(text) : text;
const mapString = (value, fn) => isString(value) ? s(fn(string(value))) : value;
const validHost = text => /^[A-Z0-9-]+$/u.test(text);
const validLogicalWord = text => /^[A-Z0-9*-]+$/u.test(text) && !text.includes('**');
const invalidText = text => /[\0\uD800-\uDFFF]/u.test(text);
const validSegment = text => !text.includes('/') && !invalidText(text);
const directory = pathname => pathname.components[2] === NIL ? [] : toArray(pathname.components[2]);
const emptyPath = () => new LispPathname(Array(6).fill(NIL));
const primary = result => result.values[0] ?? NIL;

function pathnameKey(path) {
  const component = value => value === NIL ? null : isString(value) ? ['text', string(value)]
    : marker(value) !== null ? ['marker', marker(value)] : ['version', String(value)];
  return JSON.stringify(path.components.map((value, i) => i === 2 ? directory(path).map(component) : component(value)));
}

function typeError(value, expectedType, message = `Expected ${expectedType}`) {
  throw new LispCondition('TYPE-ERROR', message, { datum: value, expectedType });
}
function fileError(pathname, message, cause) {
  throw new LispCondition('FILE-ERROR', message, { pathname, cause });
}
class NamestringError extends Error {
  constructor(message, hostMismatch = false) { super(message); this.hostMismatch = hostMismatch; }
}
function unescape(text) {
  let result = '';
  for (let i = 0; i < text.length; i++) {
    if (text[i] === '\\' && ++i === text.length) throw new NamestringError('Dangling pathname escape');
    result += text[i];
  }
  return result;
}
function escape(text, dots = false, leadingDot = false) {
  const literalGlob = text === '*' || text === '**';
  return Array.from(text, (c, i) => c === '\\' || c === ':' || literalGlob && i === 0 || dots && c === '.' && (i > 0 || leadingDot) ? `\\${c}` : c).join('');
}
function textOfLeaf(value) {
  return missing(value) ? '' : marker(value) === 'WILD' ? '*' : string(value);
}
function renderDirectory(path, escaped = true) {
  const [head, ...parts] = directory(path);
  if (head === undefined) return '';
  const absolute = marker(head) === 'ABSOLUTE';
  const separator = path.logical ? ';' : '/';
  let result = absolute ? (path.logical ? '' : '/') : (path.logical ? ';' : parts.length ? '' : './');
  for (const part of parts) {
    const kind = marker(part);
    if (kind === 'UP' || kind === 'BACK') result += '..';
    else if (kind === 'WILD' || kind === 'WILD-INFERIORS') result += kind === 'WILD' ? '*' : '**';
    else result += !path.logical && escaped ? escape(string(part)) : string(part);
    result += separator;
  }
  return result;
}
function renderFile(path, escaped = true) {
  const [, , , name, type, version] = path.components;
  const versionName = marker(version);
  if (path.logical) {
    const leaf = textOfLeaf(name);
    if (type === NIL) return version === NIL || versionName === 'NEWEST' ? leaf : null;
    const file = `${leaf}.${textOfLeaf(type)}`;
    return version === NIL ? file : `${file}.${versionName === 'WILD' ? '*' : versionName === 'NEWEST' ? 'NEWEST' : String(version)}`;
  }
  if (isInteger(version) || versionName === 'WILD' || missing(name) && !missing(type)) return null;
  const leaf = (value, dots, leadingDot) => escaped && isString(value) ? escape(string(value), dots, leadingDot) : textOfLeaf(value);
  return leaf(name, missing(type), false) + (missing(type) ? '' : '.' + leaf(type, true, true));
}
export function pathnameNamestring(path) {
  const file = renderFile(path);
  if (file === null) return null;
  return (path.logical ? string(path.components[0]) + ':' : '') + renderDirectory(path) + file;
}

function physical(text, key, syntax = true) {
  if (invalidText(text)) throw new NamestringError('Invalid pathname character');
  if (!text) return emptyPath();
  const parts = text.split('/').filter(Boolean), absolute = text.startsWith('/');
  const decode = syntax ? unescape : value => value;
  const last = parts.at(-1);
  const directoryOnly = text.endsWith('/') || last !== undefined && ['.', '..'].includes(decode(last));
  const file = directoryOnly ? undefined : parts.pop();
  const components = [];
  for (const part of parts) {
    if (syntax && part === '*') components.push(key('WILD'));
    else if (syntax && part === '**') components.push(key('WILD-INFERIORS'));
    else {
      const raw = decode(part);
      if (raw !== '.') components.push(raw === '..' ? key('UP') : s(raw));
    }
  }
  let name = NIL, type = NIL;
  if (file !== undefined) {
    let dot = -1;
    for (let i = 0; i < file.length; i++) {
      if (syntax && file[i] === '\\') { if (++i === file.length) throw new NamestringError('Dangling pathname escape'); }
      else if (file[i] === '.' && i !== 0) dot = i;
    }
    const leaf = value => syntax && value === '*' ? key('WILD') : s(decode(value));
    name = leaf(dot < 0 ? file : file.slice(0, dot));
    if (dot >= 0) type = leaf(file.slice(dot + 1));
  }
  const dir = absolute || text.includes('/') || directoryOnly ? fromArray([key(absolute ? 'ABSOLUTE' : 'RELATIVE'), ...components]) : NIL;
  return new LispPathname([NIL, NIL, dir, name, type, file === undefined ? NIL : key('NEWEST')]);
}
function logical(text, host, key) {
  const colon = text.indexOf(':');
  if (colon >= 0) {
    if (upper(text.slice(0, colon)) !== host) throw new NamestringError('Logical pathname host does not match', true);
    text = text.slice(colon + 1);
  }
  const absolute = !text.startsWith(';');
  if (!absolute) text = text.slice(1);
  const parts = text.split(';'), file = parts.pop();
  const leaf = value => {
    if (value === '*') return key('WILD');
    const normalized = upper(value);
    if (validLogicalWord(normalized)) return s(normalized);
    throw new NamestringError('Invalid logical pathname component');
  };
  const dir = fromArray([key(absolute ? 'ABSOLUTE' : 'RELATIVE'), ...parts.map(value => value === '**' ? key('WILD-INFERIORS') : leaf(value))]);
  const [name, type, version, extra] = file.split('.');
  if (extra !== undefined) throw new NamestringError('Invalid logical pathname file');
  let number = NIL;
  if (version !== undefined) {
    if (version === '*') number = key('WILD');
    else if (upper(version) === 'NEWEST') number = key('NEWEST');
    else if (/^\d+$/u.test(version) && Number(version) > 0 && Number(version) <= FIXNUM_MAX) number = Number(version);
    else throw new NamestringError('Invalid logical pathname version');
  }
  return new LispPathname([s(host), key('UNSPECIFIC'), dir, name === '' ? NIL : leaf(name), type === undefined ? NIL : leaf(type), number]);
}

function resolveBack(parts) {
  const result = [];
  for (const part of parts) {
    const previous = result.at(-1);
    if (marker(part) === 'BACK' && (isString(previous) || marker(previous) === 'WILD')) result.pop();
    else result.push(part);
  }
  return result;
}
function mapCase(path, toLogical) {
  if (path.logical === toLogical) return path.components;
  const transform = value => mapString(value, toLogical ? upper : lower);
  const result = path.components.slice();
  if (result[2] !== NIL) result[2] = fromArray(directory(path).map(transform));
  result[3] = transform(result[3]);
  result[4] = transform(result[4]);
  return result;
}
function merge(path, base, version, key, supplied) {
  const from = path.components, defaults = mapCase(base, path.logical);
  const takeDefault = i => supplied ? !supplied.has(fields[i]) : from[i] === NIL;
  const result = from.map((value, i) => takeDefault(i) ? defaults[i] : value);
  // The POSIX parser produces physical pathnames with no named host.
  // A logical host is introduced by parsing or by explicit construction.
  if (!path.logical && isString(result[0])) result[0] = NIL;
  if (from[2] !== NIL && defaults[2] !== NIL && marker(directory(path)[0]) === 'RELATIVE' && !supplied?.has('DIRECTORY')) {
    const [head, ...tail] = toArray(defaults[2]);
    result[2] = fromArray([head, ...resolveBack(tail.concat(directory(path).slice(1)))]);
  }
  if (takeDefault(5)) result[5] = from[3] === NIL && defaults[5] !== NIL ? defaults[5] : version;
  if (path.logical) result[1] = key('UNSPECIFIC');
  return new LispPathname(result);
}

const wildcard = (value, logical) => ['WILD', 'WILD-INFERIORS'].includes(marker(value)) || logical && isString(value) && string(value).includes('*');
function captures(text, pattern) {
  const [first, ...parts] = pattern.split('*');
  if (!parts.length) return text === first ? [] : null;
  const last = parts.pop();
  if (!text.startsWith(first) || !text.endsWith(last) || text.length < first.length + last.length) return null;
  let remaining = text.slice(first.length, text.length - last.length);
  const matched = [];
  for (const part of parts) {
    const index = remaining.indexOf(part);
    if (index < 0) return null;
    matched.push(remaining.slice(0, index));
    remaining = remaining.slice(index + part.length);
  }
  return [...matched, remaining];
}
function componentEqual(value, pattern) {
  return marker(value) !== null && marker(pattern) !== null ? marker(value) === marker(pattern) : equal(value, pattern);
}
function matchLeaf(value, pattern, logical) {
  if (pattern === NIL || marker(pattern) === 'WILD') return true;
  if (logical && isString(pattern) && string(pattern).includes('*')) return isString(value) && captures(string(value), string(pattern)) !== null;
  return componentEqual(value, pattern);
}
function matchDirectory([head, ...parts], [patternHead, ...pattern], logical) {
  if (patternHead === undefined) return [{ parts }];
  if (head !== undefined && marker(head) !== marker(patternHead)) return null;
  const first = { start: 0, step: 0 }, work = [first], seen = new Set(['0:0']);
  while (work.length) {
    const position = work.pop(), { start, step } = position;
    if (step === pattern.length) {
      if (start !== parts.length) continue;
      const ends = Array(pattern.length + 1).fill(0);
      for (let next = position; next.previous; next = next.previous) if (next.step !== next.previous.step) ends[next.step] = next.start;
      return pattern.flatMap((part, i) => wildcard(part, logical) ? [{ parts: parts.slice(ends[i], ends[i + 1]), part }] : []);
    }
    const pat = pattern[step], value = parts[start];
    const advance = (nextStart, nextStep) => {
      const id = `${nextStart}:${nextStep}`;
      if (!seen.has(id)) { seen.add(id); work.push({ start: nextStart, step: nextStep, previous: position }); }
    };
    if (marker(pat) === 'WILD-INFERIORS') {
      if (start < parts.length) advance(start + 1, step);
      advance(start, step + 1);
    } else if (start < parts.length && (marker(pat) === 'WILD' ? marker(value) !== 'WILD-INFERIORS' : matchLeaf(value, pat, logical))) advance(start + 1, step + 1);
  }
  return null;
}
function matchPath(source, pattern) {
  const [host, device, , name, type, version] = source.components;
  const [patHost, patDevice, , patName, patType, patVersion] = pattern.components;
  return (patHost === NIL || componentEqual(host, patHost)) && (patDevice === NIL || componentEqual(device, patDevice)) &&
    matchLeaf(name, patName, pattern.logical) && matchLeaf(type, patType, pattern.logical) && matchLeaf(version, patVersion, false) &&
    matchDirectory(directory(source), directory(pattern), pattern.logical) !== null;
}

export function installPathnames(rt, hostOperations = null) {
  const key = name => rt.intern(name, 'KEYWORD');
  const keyword = value => value instanceof LispSymbol && value.package === rt.findPackage('KEYWORD') ? value.name : null;
  const hosts = new Map();
  const defaultsSymbol = rt.exportSymbol(rt.intern('*DEFAULT-PATHNAME-DEFAULTS*', 'COMMON-LISP'), 'COMMON-LISP');
  defaultsSymbol.special = true;
  if (defaultsSymbol.value === UNBOUND) defaultsSymbol.value = emptyPath();
  const defaults = task => {
    const value = rt.readVariable(defaultsSymbol, task ?? rt.activeTask);
    if (!(value instanceof LispPathname)) typeError(value, 'PATHNAME', '*DEFAULT-PATHNAME-DEFAULTS* requires a pathname');
    return value;
  };
  const isPathname = value => value instanceof LispPathname;
  const asPathname = (value, task) => {
    const target = resolveSynonymStream(rt, value, task, false);
    return isPathname(target) ? target : target instanceof LispFileStream ? target.pathname : null;
  };
  const define = (name, min, max, fn) => rt.defineBuiltin(name, (args, r, task) => {
    if (args.length < min || args.length > max) throw new LispCondition('PROGRAM-ERROR', `Wrong number of arguments to ${name}`);
    return fn(args, r, task);
  }, { primitive: name });
  function options(args, known, allowOther = false) {
    if (args.length % 2) throw new LispCondition('PROGRAM-ERROR', 'Odd number of pathname keyword arguments');
    const result = new Map();
    let unknown;
    for (let i = 0; i < args.length; i += 2) {
      const name = keyword(args[i]);
      if (name === 'ALLOW-OTHER-KEYS' || known.includes(name)) {
        if (!result.has(name)) result.set(name, args[i + 1]);
      } else if (unknown === undefined) unknown = args[i];
    }
    if (!allowOther && (result.get('ALLOW-OTHER-KEYS') ?? NIL) === NIL && unknown !== undefined) {
      throw new LispCondition('PROGRAM-ERROR', 'Unknown pathname keyword argument', { datum: unknown });
    }
    return result;
  }
  function useCommon(opts) {
    const mode = opts.get('CASE') ?? key('LOCAL');
    if (!['LOCAL', 'COMMON'].includes(keyword(mode))) typeError(mode, '(MEMBER :LOCAL :COMMON)');
    return keyword(mode) === 'COMMON';
  }
  function hostName(value, requireDefined) {
    const name = upper(string(value));
    if (!validHost(name) || requireDefined && !hosts.has(name)) typeError(value, 'STRING', 'Expected a defined logical pathname host');
    return name;
  }
  function hostComponent(value) {
    if (value === NIL || keyword(value) === 'UNSPECIFIC') return value;
    return s(hostName(value, true));
  }
  function explicitHost(text) {
    const colon = text.indexOf(':');
    if (colon < 0) return null;
    const host = upper(text.slice(0, colon));
    return hosts.has(host) ? host : null;
  }
  function parse(text, host, base) {
    if (isString(host)) return logical(text, string(host), key);
    const explicit = explicitHost(text);
    if (explicit !== null) {
      if (host !== NIL) throw new NamestringError('Pathname host does not match', true);
      return logical(text, explicit, key);
    }
    // POSIX namestrings and hostless logical namestrings can be ambiguous.
    // A logical default gives the logical syntax priority for hostless strings.
    if (host === NIL && !text.includes(':') && base.logical) {
      try { return logical(text, string(base.components[0]), key); }
      catch (error) { if (!(error instanceof NamestringError)) throw error; }
    }
    const path = physical(text, key);
    return host === NIL ? path : new LispPathname([host, ...path.components.slice(1)]);
  }
  function pathname(value, task, base) {
    const path = asPathname(value, task);
    if (path) return path;
    if (!isString(value)) typeError(value, '(OR PATHNAME STRING STREAM)');
    try { return parse(string(value), NIL, base ?? defaults(task)); }
    catch (error) { if (!(error instanceof NamestringError)) throw error; return fileError(value, error.message, error); }
  }
  rt.pathname = pathname;
  rt.pathnameNamestring = pathnameNamestring;
  rt.readPathname = (value, task, invalid) => {
    if (!isString(value)) return invalid('#P requires a namestring');
    try { return parse(string(value), NIL, defaults(task)); }
    catch (error) {
      if (!(error instanceof NamestringError)) throw error;
      return invalid(error.message);
    }
  };
  rt.pathnameReadably = (path, text, task) => {
    try { return equal(path, parse(text, NIL, defaults(task))); }
    catch (error) { if (!(error instanceof NamestringError)) throw error; return false; }
  };

  function normalize(value, field, common) {
    if (field === 0) return hostComponent(value);
    const k = keyword(value);
    if (value === NIL) return NIL;
    if (field === 1) {
      if (k === 'UNSPECIFIC') return value;
      return typeError(value, '(MEMBER NIL :UNSPECIFIC)');
    }
    if (field === 5) {
      if (['UNSPECIFIC', 'NEWEST', 'WILD'].includes(k) || typeof value === 'number' && isInteger(value) && value > 0) return value;
      return typeError(value, `(OR (MEMBER NIL :UNSPECIFIC :NEWEST :WILD) (INTEGER 1 ${FIXNUM_MAX}))`, 'Invalid pathname version');
    }
    const textPart = part => {
      const text = string(part);
      if (!validSegment(text)) typeError(part, 'STRING', 'Invalid pathname component');
      return s(common ? flipCase(text) : text);
    };
    if (field !== 2) return ['UNSPECIFIC', 'WILD'].includes(k) ? value : textPart(value);
    if (k === 'WILD') return fromArray([key('ABSOLUTE'), key('WILD-INFERIORS')]);
    const [head, ...parts] = isString(value) ? [key('ABSOLUTE'), value] : toArray(value);
    if (!['ABSOLUTE', 'RELATIVE'].includes(keyword(head))) typeError(value, 'LIST', 'Invalid pathname directory marker');
    return fromArray([head, ...parts.map(part => {
      if (['UP', 'BACK', 'WILD', 'WILD-INFERIORS'].includes(keyword(part))) return part;
      const converted = textPart(part);
      if (['', '.', '..'].includes(string(converted))) typeError(part, 'STRING', 'Invalid pathname directory component');
      return converted;
    })]);
  }
  function normalizeLogical(path, invalid) {
    if (!path.logical) return path;
    const components = path.components.slice();
    const normalizeWord = (value, dir) => {
      const kind = marker(value);
      if (kind === 'WILD' || dir && kind === 'WILD-INFERIORS' || !dir && value === NIL) return value;
      if (!isString(value)) return invalid(value);
      const text = upper(string(value));
      if (text === '*') return key('WILD');
      if (dir && text === '**') return key('WILD-INFERIORS');
      return validLogicalWord(text) ? s(text) : invalid(value);
    };
    components[1] = key('UNSPECIFIC');
    const [head, ...parts] = directory(path);
    if (head !== undefined) components[2] = fromArray([head, ...parts.map(value => normalizeWord(value, true))]);
    components[3] = normalizeWord(components[3], false);
    components[4] = normalizeWord(components[4], false);
    if (marker(components[5]) === 'UNSPECIFIC') return invalid(components[5]);
    return new LispPathname(components);
  }
  const logicalTypeError = value => typeError(value, 'STRING', 'Invalid logical pathname component');
  const mergePath = (path, base, version = key('NEWEST'), supplied) => normalizeLogical(merge(path, base, version, key, supplied), logicalTypeError);
  function isWild(path, selector = NIL) {
    const selected = selector === NIL ? [2, 3, 4, 5] : [fields.indexOf(keyword(selector))];
    if (selected[0] < 0) typeError(selector, '(MEMBER NIL :HOST :DEVICE :DIRECTORY :NAME :TYPE :VERSION)');
    return selected.some(i => i === 2 ? directory(path).some(x => wildcard(x, path.logical)) : i >= 3 && wildcard(path.components[i], path.logical));
  }
  define('PATHNAME', 1, 1, (a, _r, task) => pathname(a[0], task));
  define('PATHNAMEP', 1, 1, a => isPathname(a[0]) ? T : NIL);
  define('MAKE-PATHNAME', 0, Infinity, (a, _r, task) => {
    const opts = options(a, [...fields, 'DEFAULTS', 'CASE']);
    const common = useCommon(opts);
    const base = opts.has('DEFAULTS') ? pathname(opts.get('DEFAULTS'), task) : new LispPathname([defaults(task).components[0], NIL, NIL, NIL, NIL, NIL]);
    const host = opts.has('HOST') ? normalize(opts.get('HOST'), 0, false) : base.components[0];
    const logical = isString(host);
    const components = [host, ...fields.slice(1).map((field, i) => opts.has(field) ? normalize(opts.get(field), i + 1, common && !logical) : NIL)];
    return mergePath(new LispPathname(components), base, NIL, opts);
  });
  define('PARSE-NAMESTRING', 1, Infinity, (a, r, task) => {
    const opts = options(a.slice(3), ['START', 'END', 'JUNK-ALLOWED']);
    const start = opts.get('START') ?? 0, requestedEnd = opts.get('END') ?? NIL;
    const nonnegative = value => isInteger(value) && value >= 0;
    if (!nonnegative(start)) typeError(start, '(INTEGER 0)');
    if (requestedEnd !== NIL && !nonnegative(requestedEnd)) typeError(requestedEnd, '(OR NULL (INTEGER 0))');
    const host = hostComponent(a[1] ?? NIL), path = asPathname(a[0]);
    if (path) {
      if (host !== NIL && !componentEqual(host, path.components[0])) fileError(path, 'Pathname host does not match');
      return r.multipleValues(path, start);
    }
    const base = a[2] === undefined || a[2] === NIL ? defaults(task) : pathname(a[2], task);
    const chars = [...string(a[0])], end = requestedEnd === NIL ? chars.length : requestedEnd;
    if (start > end || end > chars.length) throw new LispCondition('ERROR', 'Pathname bounds are outside the string');
    const text = chars.slice(Number(start), Number(end)).join('');
    try { return r.multipleValues(parse(text, host, base), integer(end)); }
    catch (error) {
      if (!(error instanceof NamestringError)) throw error;
      if (error.hostMismatch) throw new LispCondition('ERROR', error.message, { cause: error });
      if ((opts.get('JUNK-ALLOWED') ?? NIL) === NIL) throw new LispCondition('PARSE-ERROR', error.message, { cause: error });
      const allowed = isString(host) || explicitHost(text) !== null ? /^[a-zA-Z0-9*;:.-]$/u : /^[^\s\0]$/u;
      const slice = [...text];
      let stop = slice.findIndex(c => !allowed.test(c));
      if (stop < 0) stop = slice.length;
      for (; stop > 0; stop--) {
        try { return r.multipleValues(parse(slice.slice(0, stop).join(''), host, base), integer(Number(start) + stop)); }
        catch (partialError) { if (!(partialError instanceof NamestringError) || partialError.hostMismatch) throw partialError; }
      }
      return r.multipleValues(NIL, start);
    }
  });
  define('MERGE-PATHNAMES', 1, 3, (a, _r, task) => {
    const base = a.length > 1 ? pathname(a[1], task) : defaults(task);
    return mergePath(pathname(a[0], task, base), base, a.length > 2 ? normalize(a[2], 5, false) : key('NEWEST'));
  });
  for (let i = 0; i < fields.length; i++) define(`PATHNAME-${fields[i]}`, 1, i === 5 ? 1 : Infinity, (a, _r, task) => {
    const path = pathname(a[0], task), value = path.components[i];
    if (i === 5 || !useCommon(options(a.slice(1), ['CASE'])) || path.logical) return value;
    return i === 2 && value !== NIL ? fromArray(directory(path).map(x => mapString(x, flipCase))) : mapString(value, flipCase);
  });
  for (const [name, render] of [
    ['NAMESTRING', pathnameNamestring], ['FILE-NAMESTRING', renderFile], ['DIRECTORY-NAMESTRING', renderDirectory],
    ['HOST-NAMESTRING', p => p.logical ? string(p.components[0]) : ''],
  ]) define(name, 1, 1, (a, _r, task) => {
    const result = render(pathname(a[0], task));
    return result === null ? NIL : s(result);
  });
  define('ENOUGH-NAMESTRING', 1, 2, (a, _r, task) => {
    const base = a.length > 1 ? pathname(a[1], task) : defaults(task);
    const target = mergePath(pathname(a[0], task, base), base);
    const same = [0, 1, 2].every(i => componentEqual(target.components[i], base.components[i]));
    const text = same ? renderFile(target) : pathnameNamestring(target);
    return text === null ? NIL : s(same && target.logical ? `;${text}` : text);
  });
  define('WILD-PATHNAME-P', 1, 2, (a, _r, task) => isWild(pathname(a[0], task), a[1] ?? NIL) ? T : NIL);
  define('COMPILE-FILE-PATHNAME', 1, Infinity, (a, _r, task) => {
    const output = options(a.slice(1), ['OUTPUT-FILE'], true).get('OUTPUT-FILE') ?? NIL;
    const input = mergePath(pathname(a[0], task), defaults(task));
    const components = input.components.slice();
    components[4] = s(input.logical ? 'MJS' : 'mjs');
    const base = new LispPathname(components);
    return output === NIL ? base : mergePath(pathname(output, task, base), base);
  });
  define('LOGICAL-PATHNAME', 1, 1, a => {
    const path = asPathname(a[0]);
    if (path?.logical) return path;
    if (!isString(a[0])) typeError(a[0], 'LOGICAL-PATHNAME');
    const text = string(a[0]), host = explicitHost(text);
    if (host === null) typeError(a[0], 'LOGICAL-PATHNAME', 'Expected a namestring with a defined logical pathname host');
    try { return logical(text, host, key); }
    catch (error) { if (!(error instanceof NamestringError)) throw error; return typeError(a[0], 'LOGICAL-PATHNAME', error.message); }
  });
  define('LOGICAL-PATHNAME-TRANSLATIONS', 1, 1, a => {
    const rules = hosts.get(hostName(a[0], true));
    return fromArray(rules.map(pair => fromArray(pair)));
  });
  function setTranslations(host, sourceRules, task) {
    const rules = toArray(sourceRules).map(pair => {
      const [from, to, ...rest] = toArray(pair);
      if (from === undefined || to === undefined || rest.length) typeError(pair, 'LIST', 'Expected a pair of pathname translation patterns');
      try {
        const source = isString(from) ? logical(string(from), host, key) : asPathname(from);
        if (!source?.logical || string(source.components[0]) !== host) throw new NamestringError('Logical pathname host does not match', true);
        const target = isString(to) && upper(string(to).split(':', 1)[0]) === host && string(to).includes(':') ? logical(string(to), host, key) : pathname(to, task);
        return [source, target];
      } catch (error) { if (!(error instanceof NamestringError)) throw error; return fileError(from, error.message, error); }
    });
    hosts.set(host, rules);
    return sourceRules;
  }
  define('(SETF LOGICAL-PATHNAME-TRANSLATIONS)', 2, 2, (a, _r, task) => {
    return setTranslations(hostName(a[1], false), a[0], task);
  });
  define('PATHNAME-MATCH-P', 2, 2, (a, _r, task) => matchPath(pathname(a[0], task), pathname(a[1], task)) ? T : NIL);

  function translatePath(source, from, to) {
    const invalid = () => fileError(source, 'Pathname does not satisfy the translation pattern');
    if (!matchPath(source, from)) return invalid();
    const convertedSource = mapCase(source, to.logical), convertedFrom = mapCase(from, to.logical);
    const srcDir = convertedSource[2] === NIL ? [] : toArray(convertedSource[2]);
    const fromDir = convertedFrom[2] === NIL ? [] : toArray(convertedFrom[2]);
    const [toHead, ...toParts] = directory(to);
    const capturesDir = matchDirectory(srcDir, fromDir, from.logical);
    const convertString = (text, sourcePattern, destPattern) => {
      const matches = from.logical && isString(sourcePattern) && string(sourcePattern).includes('*')
        ? captures(string(text), string(sourcePattern)) : [string(text)];
      const [prefix, ...segments] = string(destPattern).split('*');
      if (matches === null || segments.length > matches.length) return invalid();
      return s(segments.reduce((result, segment, i) => result + matches[i] + segment, prefix));
    };
    const requirePhysical = value => {
      if (source.logical && !to.logical && isString(value) && string(value).includes('*')) {
        fileError(source, 'A partial logical wildcard cannot be represented in a physical pathname');
      }
      return value;
    };
    const result = to.components.slice();
    if (toHead === undefined) {
      if (srcDir.length) result[2] = fromArray(srcDir.map(requirePhysical));
    } else {
      let at = 0;
      result[2] = fromArray([toHead, ...toParts.flatMap(part => {
        if (!wildcard(part, to.logical)) return [part];
        const capture = capturesDir[at++];
        if (!capture || marker(part) !== 'WILD-INFERIORS' && capture.parts.length !== 1) return invalid();
        if (isString(part)) {
          const [text] = capture.parts;
          return isString(text) ? [convertString(text, capture.part, part)] : invalid();
        }
        return capture.parts.map(requirePhysical);
      })]);
    }
    for (const i of [3, 4]) {
      const leaf = result[i];
      if (leaf === NIL || marker(leaf) === 'WILD') result[i] = requirePhysical(convertedSource[i]);
      else if (wildcard(leaf, to.logical)) {
        const text = convertedSource[i];
        result[i] = isString(text) ? convertString(text, convertedFrom[i], leaf) : invalid();
      }
    }
    if (result[5] === NIL || marker(result[5]) === 'WILD') result[5] = source.logical && !to.logical ? key('NEWEST') : convertedSource[5];
    return normalizeLogical(new LispPathname(result), invalid);
  }
  define('TRANSLATE-PATHNAME', 3, Infinity, (a, _r, task) => {
    options(a.slice(3), []);
    return translatePath(...a.slice(0, 3).map(value => pathname(value, task)));
  });
  // Each rewrite has a continuation boundary so a growing rule cannot run
  // unbounded inside one scheduler step, even if no pathname repeats.
  const checkpoint = new NativeFunction('PATHNAME-TRANSLATION-STEP', () => NIL);
  function* translateLogical(original) {
    const visited = new Set();
    let path = original;
    while (path.logical) {
      const signature = pathnameKey(path);
      if (visited.has(signature)) fileError(original, 'Circular logical pathname translations');
      visited.add(signature);
      const host = string(path.components[0]), rules = hosts.get(host);
      if (!rules) fileError(original, `No logical pathname host named ${host}`);
      const rule = rules.find(([pattern]) => matchPath(path, pattern));
      if (!rule) fileError(original, `No translation for logical pathname host ${host}`);
      yield rt.requestCall(checkpoint);
      path = translatePath(path, ...rule);
    }
    return path;
  }
  define('TRANSLATE-LOGICAL-PATHNAME', 1, Infinity, function* (a, _r, task) {
    options(a.slice(1), []);
    return yield* translateLogical(pathname(a[0], task));
  });

  // The deployment runtime is independent of any filesystem implementation.
  // Concrete operations receive raw local paths. DIRECTORY receives a pattern
  // and its escaped namestring. ENSURE-DIRECTORIES-EXIST passes only the directory.
  rt.pathnameHost = hostOperations;
  function localNamestring(path, original, { pattern = false, directoryOnly = false }) {
    if (pattern) return pathnameNamestring(path);
    if (isWild(path, directoryOnly ? key('DIRECTORY') : NIL)) fileError(original, 'File operation requires a non-wild pathname');
    const [head, ...parts] = directory(path), resolved = resolveBack(parts);
    if (marker(head) === 'ABSOLUTE' && ['UP', 'BACK'].includes(marker(resolved[0]))) fileError(original, 'Absolute pathname goes above its root directory');
    const components = path.components.slice();
    if (head !== undefined) components[2] = fromArray([head, ...resolved]);
    const concrete = new LispPathname(components), file = directoryOnly ? '' : renderFile(concrete, false);
    if (file === null) fileError(original, 'Pathname cannot be represented as a physical path');
    return renderDirectory(concrete, false) + file;
  }
  function hostPath(value, original) {
    if (value instanceof LispPathname && !value.logical && !isWild(value)) return value;
    if (typeof value !== 'string') fileError(original, 'Filesystem host did not return a physical path');
    try { return physical(value, key, false); }
    catch (error) { if (!(error instanceof NamestringError)) throw error; return fileError(original, error.message, error); }
  }
  const invokeHost = new NativeFunction('PATHNAME-HOST-OPERATION', ([operation, filename, context, args, receive], _, task) => {
    const host = rt.pathnameHost, method = host?.[operation];
    if (typeof method !== 'function') fileError(context.pathname, `Filesystem host operation ${operation} is not configured`);
    const frame = task.currentFrame, epoch = frame?.resumeEpoch;
    const cancelled = () => task.done || task.currentFrame !== frame || frame?.resumeEpoch !== epoch;
    const failure = error => {
      if (cancelled() || error instanceof LispCondition) throw error;
      fileError(context.pathname, error instanceof Error ? error.message : String(error), error);
    };
    let result;
    try { result = method.call(host, filename, { ...context, task, runtime: rt }, ...args); }
    catch (error) { return failure(error); }
    const received = value => receive ? receive(value, cancelled()) : value;
    return result && typeof result.then === 'function' ? Promise.resolve(result).then(received, failure) : received(result);
  });
  const mergedDesignator = (value, task) => {
    const stream = resolveSynonymStream(rt, value, task, false);
    return stream instanceof LispFileStream ? stream.pathname : mergePath(pathname(value, task), defaults(task));
  };
  function* filesystemPath(original, { pattern = false, directoryOnly = false } = {}) {
    if (!pattern && isWild(original, directoryOnly ? key('DIRECTORY') : NIL)) fileError(original, 'File operation requires a non-wild pathname');
    const physicalPath = yield* translateLogical(original);
    const filename = localNamestring(physicalPath, original, { pattern, directoryOnly });
    if (filename === null) fileError(original, 'Pathname has no physical namestring');
    return { filename, physicalPath };
  }
  function* callHost(operation, pathname, { args = [], receive, ...pathOptions } = {}) {
    const { filename, physicalPath } = yield* filesystemPath(pathname, pathOptions);
    return primary(yield rt.requestCall(invokeHost, [operation, filename, { pathname, physicalPath }, args, receive]));
  }
  rt.mergePathnames = (value, task, base) => base === undefined ? mergedDesignator(value, task) : mergePath(pathname(value, task, base), base);
  rt.callPathnameHost = (operation, value, task, options) => callHost(operation, asPathname(value, task) ?? mergedDesignator(value, task), options);
  rt.hostPathname = hostPath;
  const absent = value => value === null || value === false || value === NIL;
  define('LOAD-LOGICAL-PATHNAME-TRANSLATIONS', 1, 1, function* (a, _r, task) {
    const host = hostName(a[0], false);
    if (hosts.has(host)) return NIL;
    const result = primary(yield rt.requestCall(invokeHost, ['loadLogicalPathnameTranslations', host, { pathname: a[0] }, []]));
    if (!Array.isArray(result) || result.some(rule => !Array.isArray(rule) || rule.length !== 2)) {
      return fileError(a[0], `Filesystem host did not supply logical pathname translations for ${host}`);
    }
    setTranslations(host, fromArray(result.map(rule => fromArray(rule))), task);
    return T;
  });
  define('USER-HOMEDIR-PATHNAME', 0, 1, function* (a) {
    const host = hostComponent(a[0] ?? NIL);
    const result = primary(yield rt.requestCall(invokeHost, ['userHomedirPathname', isString(host) ? string(host) : null, { pathname: host }, []]));
    if (absent(result)) return NIL;
    const pathname = hostPath(result, host);
    if (pathname.components[3] !== NIL || pathname.components[4] !== NIL || pathname.components[5] !== NIL) {
      return fileError(host, 'Filesystem host must return a directory pathname for the home directory');
    }
    return pathname;
  });
  for (const [name, operation] of [['PROBE-FILE', 'probeFile'], ['TRUENAME', 'truename']]) define(name, 1, 1, function* (a, _r, task) {
    const original = mergedDesignator(a[0], task), value = yield* callHost(operation, original);
    if (!absent(value)) return hostPath(value, original);
    if (operation === 'probeFile') return NIL;
    return fileError(original, 'File does not exist');
  });
  define('FILE-WRITE-DATE', 1, 1, function* (a, _r, task) {
    const original = mergedDesignator(a[0], task), date = yield* callHost('fileWriteDate', original);
    if (absent(date)) return NIL;
    if (typeof date === 'bigint' && date >= 0n || Number.isSafeInteger(date) && date >= 0) return integer(date);
    return fileError(original, 'Filesystem host must return a universal time');
  });
  define('FILE-AUTHOR', 1, 1, function* (a, _r, task) {
    const original = mergedDesignator(a[0], task), author = yield* callHost('fileAuthor', original);
    if (absent(author)) return NIL;
    if (typeof author === 'string' && !invalidText(author)) return s(author);
    return fileError(original, 'Filesystem host must return a file author string');
  });
  define('DELETE-FILE', 1, 1, function* (a, _r, task) {
    const original = mergedDesignator(a[0], task);
    if ((yield* callHost('deleteFile', original)) === false) return fileError(original, 'File deletion failed');
    return T;
  });
  define('DIRECTORY', 1, Infinity, function* (a, _r, task) {
    options(a.slice(1), []);
    const original = mergedDesignator(a[0], task), results = yield* callHost('directory', original, { pattern: true });
    if (typeof results === 'string' || typeof results?.[Symbol.iterator] !== 'function') return fileError(original, 'Filesystem host must return an iterable of physical paths');
    const paths = [], seen = new Set();
    for (const result of results) {
      const path = hostPath(result, original), signature = pathnameKey(path);
      if (!seen.has(signature)) {
        seen.add(signature);
        paths.push(path);
      }
    }
    return fromArray(paths);
  });
  define('ENSURE-DIRECTORIES-EXIST', 1, Infinity, function* (a, _r, task) {
    const opts = options(a.slice(1), ['VERBOSE']);
    const original = mergedDesignator(a[0], task), created = yield* callHost('ensureDirectoriesExist', original, { directoryOnly: true });
    if (created !== true && created !== false) fileError(original, 'Filesystem host must indicate whether it created directories');
    if (created && (opts.get('VERBOSE') ?? NIL) !== NIL) rt.output.write(`Created directories for ${pathnameNamestring(original)}\n`);
    return rt.multipleValues(a[0], created ? T : NIL);
  });
  define('RENAME-FILE', 2, 2, function* (a, _r, task) {
    const original = mergedDesignator(a[0], task);
    let requested = pathname(a[1], task, original);
    if (requested.logical && isString(a[1]) && !/[:;]/u.test(string(a[1]))) {
      // A bare destination name keeps the original directory on either host.
      const components = requested.components.slice();
      components[2] = NIL;
      requested = new LispPathname(components);
    }
    const target = mergePath(requested, original);
    const { filename, physicalPath } = yield* filesystemPath(original);
    const destination = yield* filesystemPath(target);
    const result = primary(yield rt.requestCall(invokeHost, ['renameFile', filename, {
      pathname: original, physicalPath, target, physicalTarget: destination.physicalPath,
    }, [destination.filename]]));
    if (!result || typeof result !== 'object') return fileError(original, 'Filesystem host must return the old and new truenames');
    return rt.multipleValues(target, hostPath(result.oldTruename, original), hostPath(result.newTruename, target));
  });
  return rt;
}
