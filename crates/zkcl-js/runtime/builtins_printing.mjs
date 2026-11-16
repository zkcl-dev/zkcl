import * as O from "./objects.mjs";
import {
  NIL, T, Cons, LispCharacter, LispArray, list, fromArray, toArray, equal,
} from "./objects.mjs";
import * as N from "./numbers.mjs";
import { NativeFunction, LispCondition, LispRestart } from "./execution.mjs";
import {
  executeFormat, FormatLabels, FormatLayout, parseFormat, pprintList,
} from "./format.mjs";
import {
  CompositeStream, LispFileStream, StringStream, filePositionDesignator, flushStream,
  openFile, peekStreamInput, readStreamElement, resolveSynonymStream, streamClassName,
  streamDirection, streamCharacterMode, streamElementType, streamFileEndpoint, streamOpen,
  streamOutputEndpoint, unreadStreamCharacter, withStreamReader, writeStreamSteps,
} from "./file_streams.mjs";
import {
  truth, fail, typeError, isString, str, string, name, sym, index, call, requestedValue,
  keys, car, sequence, bounds, array,
  FillPointerStream, dynamicValue, readDefaultDouble, outputStream, output, PprintDispatchTable,
} from "./builtins_helpers.mjs";
import { requestedTypep, atomicTypep, nameSafe } from "./types.mjs";
import { writeToString, renderValueSteps } from "./printer.mjs";

export function installPrinting(rt, def, unary, pred) {
  for (const name of ["BASE-CHAR", "STANDARD-CHAR", "UNSIGNED-BYTE"])
    rt.exportSymbol(rt.intern(name, "COMMON-LISP"), "COMMON-LISP");
  const standardDispatch = new PprintDispatchTable();
  const variables = {
    ARRAY: T,
    BASE: 10,
    CASE: rt.intern("UPCASE", "KEYWORD"),
    CIRCLE: NIL,
    ESCAPE: T,
    GENSYM: T,
    LENGTH: NIL,
    LEVEL: NIL,
    LINES: NIL,
    "MISER-WIDTH": NIL,
    "PPRINT-DISPATCH": new PprintDispatchTable(),
    PRETTY: NIL,
    RADIX: NIL,
    READABLY: NIL,
    "RIGHT-MARGIN": NIL,
  };
  for (const [key, value] of Object.entries(variables)) {
    const symbol = rt.intern(`*PRINT-${key}*`, "COMMON-LISP");
    symbol.value = value;
    symbol.special = true;
    rt.specials?.add(symbol);
    rt.findPackage("COMMON-LISP").exports.add(symbol.name);
  }
  rt.input ??= new StringStream("", { input: true });
  const terminal = new CompositeStream("twoWay", { inputStream: rt.input, outputStream: rt.output });
  rt.streamClassName = stream => streamClassName(rt, stream);
  for (const [key, value] of [
    ["*STANDARD-INPUT*", rt.input],
    ["*STANDARD-OUTPUT*", rt.output],
    ["*ERROR-OUTPUT*", rt.errorOutput ?? rt.output],
    ["*TRACE-OUTPUT*", rt.output],
    ["*TERMINAL-IO*", terminal],
    ["*QUERY-IO*", terminal],
    ["*DEBUG-IO*", terminal],
  ]) {
    const symbol = rt.intern(key, "COMMON-LISP");
    symbol.value = value;
    symbol.special = true;
    rt.specials?.add(symbol);
    rt.findPackage("COMMON-LISP").exports.add(key);
  }
  const printOptions = (task, k = new Map()) => {
    const get = (key) =>
      k.has(key)
        ? k.get(key)
        : dynamicValue(rt, task, `*PRINT-${key}*`, variables[key]);
    const margin = get("RIGHT-MARGIN"), miser = get("MISER-WIDTH"), lines = get("LINES");
    return {
      runtime: rt,
      task,
      escape: get("ESCAPE") !== NIL,
      radix: index(get("BASE")),
      printRadix: get("RADIX") !== NIL,
      readably: get("READABLY") !== NIL,
      circle: get("CIRCLE") !== NIL,
      gensym: get("GENSYM") !== NIL,
      array: get("ARRAY") !== NIL,
      pretty: get("PRETTY") !== NIL,
      margin: margin === NIL ? 72 : index(margin),
      miser: miser === NIL ? undefined : index(miser),
      lines: lines === NIL ? undefined : index(lines),
      pprintDispatch: k.get("PPRINT-DISPATCH"),
      printCase: name(get("CASE")),
      level: get("LEVEL") === NIL ? null : index(get("LEVEL")),
      length: get("LENGTH") === NIL ? null : index(get("LENGTH")),
    };
  };
  const dispatchTable = (value, task) => {
    const table =
      value === undefined
        ? dynamicValue(
            rt,
            task,
            "*PRINT-PPRINT-DISPATCH*",
            variables["PPRINT-DISPATCH"],
          )
        : value === NIL
          ? standardDispatch
          : value;
    return table instanceof PprintDispatchTable
      ? table
      : typeError(table, "pprint dispatch table");
  };
  const findPrintDispatch = function* (value, table) {
    for (const entry of table.entries)
      if (yield* requestedTypep(value, entry.type, rt)) return entry.function;
    return null;
  };
  const printObject = rt.intern("PRINT-OBJECT", "COMMON-LISP");
  const format = rt.intern("FORMAT", "COMMON-LISP");
  rt.printObjectFallbackSteps = function* (value, stream, task) {
    const options = streamPrintOptions(printOptions(task), stream);
    if (value instanceof LispRestart && !options.escape && !options.readably) {
      if (value.report === NIL) yield* output(rt, stream, writeToString(value.name, options), task);
      else if (isString(value.report)) yield* output(rt, stream, str(value.report), task);
      else yield rt.requestCall(value.report, [stream]);
    } else if (value instanceof LispCondition && !options.escape && !options.readably && value.formatControl !== undefined && value.formatControl !== NIL) {
      yield rt.requestCall(format, [stream, value.formatControl, ...toArray(value.formatArguments ?? NIL)]);
    } else {
      const printed = yield* renderValueSteps(value, options);
      yield* writePrinted(stream, printed);
    }
    return value;
  };
  rt.printValueSteps = function* (value, options, task) {
    if (!task) return writeToString(value, options);
    const invokePrinter = function* (value, depth, labels) {
      const printer = options.pretty ? yield* findPrintDispatch(value, dispatchTable(options.pprintDispatch, task)) : null;
      if (!printer && !rt.usesPrintObject(value)) return undefined;
      const stream = new StringStream("", { column: depth === 0 ? options.column ?? 0 : 0 }), bindings = [];
      if (options.pretty && (depth > 0 || options.layout)) stream.layout = new FormatLayout(stream.column);
      stream.formatLabels = labels;
      stream.printingRoot = value;
      const bind = (key, value) => bindings.push([rt.intern(`*PRINT-${key}*`, "COMMON-LISP"), value]);
      for (const [key, field] of [["ESCAPE", "escape"], ["READABLY", "readably"], ["RADIX", "printRadix"],
        ["CIRCLE", "circle"], ["GENSYM", "gensym"], ["ARRAY", "array"], ["PRETTY", "pretty"]])
        if (options[field] !== undefined) bind(key, truth(options[field]));
      if (options.radix !== undefined) bind("BASE", options.radix);
      if (options.printCase !== undefined) bind("CASE", rt.intern(options.printCase, "KEYWORD"));
      if (options.length !== undefined) bind("LENGTH", options.length ?? NIL);
      if (options.level !== undefined) bind("LEVEL", options.level == null ? NIL : Math.max(0, options.level - depth));
      for (const [key, field] of [["RIGHT-MARGIN", "margin"], ["MISER-WIDTH", "miser"], ["LINES", "lines"]])
        if (options[field] !== undefined) bind(key, options[field]);
      if (options.pprintDispatch !== undefined) bind("PPRINT-DISPATCH", dispatchTable(options.pprintDispatch, task));
      yield rt.requestCall(new NativeFunction("PRINT-WITH-BINDINGS", function* (_args, runtime, activeTask) {
        activeTask.bind(activeTask.currentFrame, bindings);
        return yield runtime.requestCall(printer ?? printObject, printer ? [stream, value] : [value, stream]);
      }));
      return stream.layout ?? stream.text;
    };
    return yield* renderValueSteps(value, options, invokePrinter);
  };
  def(
    "COPY-PPRINT-DISPATCH",
    0,
    1,
    (a, r, t) => new PprintDispatchTable(dispatchTable(a[0], t).entries),
  );
  def("SET-PPRINT-DISPATCH", 2, 4, (a, r, t) => {
    const table = dispatchTable(a[3], t),
      priority = a[2] ?? 0;
    if (!N.isReal(priority)) typeError(priority, "real");
    table.entries = table.entries.filter((entry) => !equal(entry.type, a[0]));
    if (a[1] !== NIL) {
      if (!atomicTypep(a[1], r.intern("FUNCTION", "COMMON-LISP"), r))
        typeError(a[1], "function");
      table.entries.push({ type: a[0], function: a[1], priority });
      table.entries.sort((x, y) => -N.compare(x.priority, y.priority));
    }
    return NIL;
  });
  def("PPRINT-DISPATCH", 1, 2, function* (a, r, t) {
    const printer = yield* findPrintDispatch(a[0], dispatchTable(a[1], t));
    const fallback = new NativeFunction(
      "DEFAULT-PRINT-OBJECT",
      function* (args, _r, task) {
        yield* output(r, args[0], writeToString(args[1], printOptions(task)), task);
        return args[1];
      },
    );
    return r.multipleValues(printer ?? fallback, truth(!!printer));
  });
  for (const n of ["WRITE-TO-STRING", "PRIN1-TO-STRING", "PRINC-TO-STRING"])
    def(n, 1, n === "WRITE-TO-STRING" ? Infinity : 1, function* (a, r, t) {
      const k = keys(a.slice(1), [
        "ESCAPE",
        "BASE",
        "RADIX",
        "READABLY",
        "CIRCLE",
        "PRETTY",
        "LEVEL",
        "LENGTH",
        "CASE",
        "GENSYM",
        "ARRAY",
        "RIGHT-MARGIN",
        "MISER-WIDTH",
        "LINES",
        "PPRINT-DISPATCH",
      ]);
      const opts = printOptions(t, k);
      if (n !== "WRITE-TO-STRING") {
        opts.escape = n !== "PRINC-TO-STRING";
        if (n === "PRINC-TO-STRING") opts.readably = false;
      }
      return string(yield* r.printValueSteps(a[0], opts, t));
    });
  const formatStream = (destination, task, allowString = false) => {
    if (destination === NIL && allowString) return new StringStream();
    if (destination === T) destination = dynamicValue(rt, task, "*STANDARD-OUTPUT*", rt.output);
    if (allowString && destination instanceof LispArray && isString(destination) && destination.fillPointer !== null)
      return new FillPointerStream(destination);
    const target = streamOutputEndpoint(rt, destination, task);
    if (!streamDirection(rt, destination, "output", task)) typeError(destination, "output stream");
    if (target instanceof StringStream && (!target.open || target.input))
      fail("STREAM-ERROR", "Stream is not open for output", { stream: destination });
    return destination;
  };
  const formatNamedFunction = (spelling) => {
    const match = /^(?:([^:]+)(::?))?([^:]+)$/.exec(spelling);
    if (!match) fail("FORMAT-ERROR", "Invalid name in ~/ directive");
    const [, packageName = "COMMON-LISP-USER", separator, symbolName] = match;
    if (!rt.findPackage(packageName)) fail("PACKAGE-ERROR", `No package ${packageName}`);
    const symbol = separator === ":" ? rt.findExternalSymbol(symbolName, packageName) : rt.findSymbol(symbolName, packageName);
    if (!symbol) fail("UNDEFINED-FUNCTION", `Undefined function ${spelling}`);
    return rt.readFunction(symbol);
  };
  const formatColumn = (stream) => {
    stream = streamOutputEndpoint(rt, stream);
    const column = stream.column;
    if (Number.isInteger(column)) return column;
    if (typeof stream.text === "string")
      return [...stream.text.slice(stream.text.lastIndexOf("\n") + 1)].length;
    return undefined;
  };
  const streamPrintOptions = (options, stream) => {
    stream = streamOutputEndpoint(rt, stream, options.task);
    options.formatLabels = stream.formatLabels;
    options.printingRoot = stream.printingRoot;
    options.column = formatColumn(stream);
    options.layout = stream.layout;
    if (stream.layout && options.level !== null)
      options.level = Math.max(0, options.level - stream.layout.depth);
    return options;
  };
  const formatWrite = function* (stream, text, literal = false) {
    if (text === "") return;
    const target = streamOutputEndpoint(rt, stream);
    if (target instanceof StringStream) target.write(text, literal);
    else yield* writeStreamSteps(rt, stream, text);
  };
  const writePrinted = function* (stream, printed) {
    const target = streamOutputEndpoint(rt, stream);
    if (printed instanceof FormatLayout) target.layout.append(printed);
    else yield* formatWrite(stream, printed);
  };
  const formatEnvironment = (task) => ({
    isString,
    string: str,
    isFunction: (value) => value instanceof NativeFunction || value instanceof O.Closure ||
      value instanceof O.LispCode || typeof value?.invoke === "function",
    column: formatColumn,
    write: formatWrite,
    capture: (column) => new StringStream("", { column }),
    list: toArray,
    typeError,
    characterName: (value) => O.characterName(value.codePoint),
    namedFunction: formatNamedFunction,
    call: (fn, args) => requestedValue(rt, fn, args),
    options: () => {
      const get = (key) => dynamicValue(rt, task, `*PRINT-${key}*`, variables[key]);
      const margin = get("RIGHT-MARGIN"), miser = get("MISER-WIDTH");
      const readably = get("READABLY") !== NIL;
      const level = get("LEVEL"), length = get("LENGTH"), lines = get("LINES");
      return {
        pretty: get("PRETTY") !== NIL,
        margin: margin === NIL ? 72 : index(margin),
        miser: miser === NIL ? undefined : index(miser),
        circle: get("CIRCLE") !== NIL,
        level: readably || level === NIL ? undefined : index(level),
        length: readably || length === NIL ? undefined : index(length),
        lines: readably || lines === NIL ? undefined : index(lines),
        defaultFloat: readDefaultDouble(rt, task) ? "double" : "single",
      };
    },
    print: function* (stream, value, directive, { unlimited, pretty, ...overrides } = {}) {
      const options = streamPrintOptions(printOptions(task), stream);
      if (directive !== "W") options.escape = directive === "S";
      if (directive === "A") options.readably = false;
      if (unlimited) options.level = options.length = null;
      if (pretty) options.pretty = true;
      Object.assign(options, overrides);
      const text = yield* rt.printValueSteps(value, options, task);
      yield* writePrinted(stream, text);
    },
  });
  def("FORMAT", 2, Infinity, function* ([destination, control, ...args], r, t) {
    const stream = formatStream(destination, t, true);
    try {
      yield* executeFormat(control, args, stream, formatEnvironment(t));
      return destination === NIL ? string(stream.text) : NIL;
    } finally {
      if (destination === NIL || stream instanceof FillPointerStream) stream.open = false;
    }
  });
  const compileFormatter = def("%COMPILE-FORMATTER", 1, 1, ([control]) => {
    const syntax = parseFormat(str(control));
    return new NativeFunction("FORMATTER", function* ([destination, ...args], r, t) {
      const stream = formatStream(destination, t);
      return fromArray(yield* executeFormat(syntax, args, stream, formatEnvironment(t)));
    });
  });
  rt.primitives?.set("FORMATTER", compileFormatter);
  const outputToString = (destination, elementType) => {
    if (!(destination instanceof LispArray && isString(destination) && destination.fillPointer !== null))
      typeError(destination, "string with fill pointer");
    if (!["CHARACTER", "BASE-CHAR", "STANDARD-CHAR", "T"].includes(nameSafe(elementType)))
      typeError(elementType, "character type specifier");
    return new FillPointerStream(destination);
  };
  def("%OUTPUT-TO-STRING", 1, 2, ([destination, elementType = rt.intern("CHARACTER", "COMMON-LISP")]) =>
    outputToString(destination, elementType));
  def("%CALL-WITH-OUTPUT-TO-STRING", 3, 3, function* ([destination, elementType, callback], r, t) {
    const stream = destination === NIL
      ? yield* requestedValue(r, "MAKE-STRING-OUTPUT-STREAM", [r.intern("ELEMENT-TYPE", "KEYWORD"), elementType])
      : outputToString(destination, elementType);
    try {
      const values = yield r.requestCall(callback, [stream]);
      return destination === NIL ? string(stream.text) : values;
    } finally {
      stream.open = false;
    }
  });
  def("PPRINT-NEWLINE", 1, 2, ([kind, destination], r, t) => {
    const choice = nameSafe(kind)?.toLowerCase();
    if (!["linear", "miser", "fill", "mandatory"].includes(choice))
      typeError(kind, "pprint newline kind");
    if (formatEnvironment(t).options().pretty) outputStream(r, destination, t).layout?.newline(choice);
    return NIL;
  });
  def("PPRINT-INDENT", 2, 3, ([kind, amount, destination], r, t) => {
    const choice = nameSafe(kind);
    if (choice !== "BLOCK" && choice !== "CURRENT") typeError(kind, "pprint indent kind");
    if (!N.isReal(amount)) typeError(amount, "real");
    if (formatEnvironment(t).options().pretty)
      outputStream(r, destination, t).layout?.indent(Math.round(N.toNumber(amount)), choice === "CURRENT");
    return NIL;
  });
  def("PPRINT-TAB", 3, 4, ([kind, column, increment, destination], r, t) => {
    const choice = nameSafe(kind);
    if (!["LINE", "SECTION", "LINE-RELATIVE", "SECTION-RELATIVE"].includes(choice))
      typeError(kind, "pprint tab kind");
    const colnum = index(column), colinc = index(increment);
    if (formatEnvironment(t).options().pretty)
      outputStream(r, destination, t).layout?.tab(colnum, colinc, choice.endsWith("RELATIVE"), choice.startsWith("SECTION"));
    return NIL;
  });
  for (const kind of ["fill", "linear", "tabular"])
    def(`PPRINT-${kind.toUpperCase()}`, 2, kind === "tabular" ? 5 : 4,
      function* ([destination, object, colon = T, _atsign, tabsize = 16], r, t) {
        const stream = outputStream(r, destination, t);
        yield* pprintList(object, stream, formatEnvironment(t), kind, colon !== NIL, tabsize === NIL ? 16 : index(tabsize));
        return NIL;
      });
  def("PPRINT", 1, 2, function* ([value, destination], r, t) {
    const stream = outputStream(r, destination, t);
    yield* formatWrite(stream, "\n");
    const options = streamPrintOptions(printOptions(t), stream);
    options.escape = options.pretty = true;
    yield* writePrinted(stream, yield* r.printValueSteps(value, options, t));
    return r.multipleValues();
  });
  const beginPprintBlock = function* (destination, object, prefix, perLine, t) {
    const target = outputStream(rt, destination, t), env = formatEnvironment(t);
    const options = env.options(), depth = target.layout?.depth ?? 0;
    if (object !== NIL && !(object instanceof Cons)) {
      yield* env.print(target, object, "W");
      return NIL;
    }
    if (options.level !== undefined && depth >= options.level) {
      yield* formatWrite(target, "#");
      return NIL;
    }
    const labels = options.circle ? target.formatLabels ?? new FormatLabels([object]) : null;
    const mark = object === target.printingRoot ? null : labels?.mark(object);
    if (mark) yield* formatWrite(target, mark.text);
    if (mark?.reference) return NIL;
    const column = env.column(target) ?? 0;
    const layout = new FormatLayout(column + [...prefix].length);
    layout.depth = depth + 1;
    const stream = env.capture(layout.start);
    stream.layout = layout;
    stream.formatLabels = labels;
    stream.pprintState = { target, prefix, perLine, column, object, list: object, count: 0,
      seen: new Set(), stopped: false, options };
    return stream;
  };
  const pprintState = (stream) => {
    if (!stream?.pprintState || !stream.open) fail("PROGRAM-ERROR", "Expected an active pretty printing stream");
    return stream.pprintState;
  };
  const finishPprintBlock = function* (stream, suffix) {
    const { target, prefix, perLine, column, options } = pprintState(stream);
    const { layout } = stream;
    try {
      if (target.layout) target.layout.block(prefix, suffix, perLine, layout);
      else {
        const text = layout.render(options.margin, options.miser, column, prefix, suffix,
          perLine ? [[column, prefix]] : [], true, { remaining: options.lines ?? Infinity }).text;
        yield* formatWrite(target, text);
      }
    } finally { stream.open = false; }
  };
  def("%CALL-PPRINT-LOGICAL-BLOCK", 3, Infinity, function* ([destination, object, callback, ...args], r, t) {
    const k = keys(args, ["PREFIX", "PER-LINE-PREFIX", "SUFFIX"]);
    if (k.has("PREFIX") && k.has("PER-LINE-PREFIX"))
      fail("PROGRAM-ERROR", "Choose either :PREFIX or :PER-LINE-PREFIX");
    const prefix = str(k.get("PREFIX") ?? k.get("PER-LINE-PREFIX") ?? "");
    const suffix = str(k.get("SUFFIX") ?? "");
    const stream = yield* beginPprintBlock(destination, object, prefix, k.has("PER-LINE-PREFIX"), t);
    if (stream !== NIL)
      try { yield r.requestCall(callback, [stream]); }
      finally { yield* finishPprintBlock(stream, suffix); }
    return NIL;
  });
  // AOT macro expansion refers to these primitives and its dynamic depth variable.
  const printerPackage = rt.findPackage("ZK") ?? rt.makePackage("ZK");
  const pprintDepth = rt.intern("*%PPRINT-DEPTH*", printerPackage);
  pprintDepth.value = 0;
  pprintDepth.special = true;
  rt.specials?.add(pprintDepth);
  def("%PPRINT-BLOCK-START", 6, 6, function* ([destination, object, prefix, perLine, usePrefix, usePerLine], r, t) {
    const affix = usePrefix !== NIL ? str(prefix) : usePerLine !== NIL ? str(perLine) : "";
    const stream = yield* beginPprintBlock(destination, object, affix, usePerLine !== NIL, t);
    return stream === NIL ? NIL : list(stream);
  });
  def("%PPRINT-BLOCK-END", 3, 3, function* ([state, suffix, supplied]) {
    yield* finishPprintBlock(car(state), supplied !== NIL ? str(suffix) : "");
    return NIL;
  });
  unary("%PPRINT-STOPPED-P", (stream) => truth(pprintState(stream).stopped));
  unary("%PPRINT-EXHAUSTED-P", (stream) => truth(pprintState(stream).list === NIL));
  const checkPprintTail = function* (stream, tail, count, t) {
    const state = pprintState(stream);
    if (tail !== NIL && (!(tail instanceof Cons) || state.seen.has(tail) ||
        (tail !== state.object && stream.formatLabels?.repeated.has(tail)))) {
      stream.write(". ");
      yield* formatEnvironment(t).print(stream, tail, "W");
    } else if (count === state.options.length) stream.write("...");
    else {
      if (tail !== NIL) state.seen.add(tail);
      return true;
    }
    state.stopped = true;
    return false;
  };
  def("%PPRINT-LENGTH-CHECK", 3, 3, function* ([tail, stream, counter], r, t) {
    const count = car(counter);
    if (!(yield* checkPprintTail(stream, tail, count, t))) return NIL;
    counter.car = count + 1;
    return T;
  });
  def("%PPRINT-POP", 1, 1, function* ([stream], r, t) {
    const state = pprintState(stream), tail = state.list;
    if (!(yield* checkPprintTail(stream, tail, state.count, t))) return NIL;
    state.count++;
    if (tail === NIL) return NIL;
    state.list = tail.cdr;
    return tail.car;
  });
  for (const n of ["PRIN1", "PRINC", "PRINT"])
    def(n, 1, 2, function* (a, r, t) {
      const target = outputStream(r, a[1], t);
      if (n === "PRINT") yield* formatWrite(target, "\n");
      const options = streamPrintOptions(printOptions(t), target);
      options.escape = n !== "PRINC";
      if (n === "PRINC") options.readably = false;
      const text = yield* r.printValueSteps(a[0], options, t);
      yield* writePrinted(target, text);
      if (n === "PRINT") yield* formatWrite(target, " ");
      return a[0];
    });
  def("WRITE", 1, Infinity, function* (a, r, t) {
    const k = keys(a.slice(1), [
      "STREAM",
      "ESCAPE",
      "BASE",
      "RADIX",
      "READABLY",
      "CIRCLE",
      "PRETTY",
      "LEVEL",
      "LENGTH",
      "CASE",
      "GENSYM",
      "ARRAY",
      "RIGHT-MARGIN",
      "MISER-WIDTH",
      "LINES",
      "PPRINT-DISPATCH",
    ]);
    const target = outputStream(r, k.get("STREAM"), t);
    const text = yield* r.printValueSteps(a[0], streamPrintOptions(printOptions(t, k), target), t);
    yield* writePrinted(target, text);
    return a[0];
  });
  def("TERPRI", 0, 1, function* (a, r, t) {
    yield* output(rt, a[0], "\n", t);
    return NIL;
  });
  def("FRESH-LINE", 0, 1, function* (a, r, t) {
    const stream = outputStream(r, a[0], t);
    if (formatColumn(stream) === 0) return NIL;
    yield* writeStreamSteps(rt, stream, "\n");
    return T;
  });
  def("WRITE-CHAR", 1, 2, function* (a, r, t) {
    if (!(a[0] instanceof LispCharacter)) typeError(a[0], "character");
    yield* output(rt, a[1], a[0].toString(), t);
    return a[0];
  });
  for (const n of ["WRITE-STRING", "WRITE-LINE"])
    def(n, 1, Infinity, function* (a, r, t) {
      const text = str(a[0]),
        k = keys(a.slice(2), ["START", "END"]),
        chars = [...text],
        [start, end] = bounds(
          chars.length,
          k.get("START") ?? 0,
          k.get("END") ?? NIL,
        );
      yield* output(
        rt,
        a[1],
        chars.slice(start, end).join("") + (n === "WRITE-LINE" ? "\n" : ""),
        t,
      );
      return a[0];
    });
  def("MAKE-STRING-OUTPUT-STREAM", 0, Infinity, (a) => {
    keys(a, ["ELEMENT-TYPE"]);
    return new StringStream();
  });
  unary("GET-OUTPUT-STREAM-STRING", (x) => {
    if (!(x instanceof StringStream) || x.input)
      typeError(x, "string output stream");
    const result = string(x.text);
    x.text = "";
    x._column = 0;
    return result;
  });
  def("MAKE-STRING-INPUT-STREAM", 1, 3, (a) => {
    const text = str(a[0]),
      [start, end] = bounds([...text].length, a[1] ?? 0, a[2] ?? NIL);
    return new StringStream(text, { input: true, start, end });
  });
  const inputStream = (value, task) => {
    const stream = value === T ? dynamicValue(rt, task, "*TERMINAL-IO*", rt.input)
      : value === undefined || value === NIL ? dynamicValue(rt, task, "*STANDARD-INPUT*", rt.input) : value;
    return stream;
  };
  const stringInput = (stream) => {
    if (!(stream instanceof StringStream)) typeError(stream, "stream");
    if (!stream.input || !stream.open) fail("STREAM-ERROR", "Expected an open character input stream", { stream });
    return stream;
  };
  const readEof = (stream, eofError, eofValue) => {
    if (eofError === NIL) return eofValue ?? NIL;
    fail("END-OF-FILE", "End of input stream", { stream });
  };
  def("OPEN", 1, Infinity, (a, r, t) => openFile(r, t, a[0], keys(a.slice(1), ["DIRECTION", "ELEMENT-TYPE", "IF-EXISTS", "IF-DOES-NOT-EXIST", "EXTERNAL-FORMAT"])));
  for (const n of ["READ-CHAR", "READ-CHAR-NO-HANG"])
    def(n, 0, 4, function* (a, _r, t) {
      const stream = inputStream(a[0], t);
      const text = yield* readStreamElement(rt, stream, t, { noHang: n === "READ-CHAR-NO-HANG" });
      return text === NIL ? NIL : text === undefined ? readEof(stream, a[1], a[2]) : new LispCharacter(text);
    });
  def("UNREAD-CHAR", 1, 2, (a, _r, t) => {
    if (!(a[0] instanceof LispCharacter)) typeError(a[0], "character");
    return unreadStreamCharacter(rt, inputStream(a[1], t), t, a[0].toString());
  });
  def("READ-LINE", 0, 4, (a, r, t) => {
    const stream = inputStream(a[0], t);
    return withStreamReader(r, stream, t, function* (reader) {
      let text = "";
      for (let char = yield* reader.takeSteps(); char !== undefined; char = yield* reader.takeSteps()) {
        if (char === "\n") return r.multipleValues(string(text), NIL);
        text += char;
      }
      return r.multipleValues(text ? string(text) : readEof(stream, a[1], a[2]), T);
    });
  });
  def("READ-BYTE", 1, 3, function* (a, r, t) {
    const value = yield* readStreamElement(r, a[0], t, { character: false });
    return value === undefined ? readEof(a[0], a[1], a[2]) : value;
  });
  function octet(value) {
    if (!N.isInteger(value) || value < 0 || value > 255) typeError(value, "(UNSIGNED-BYTE 8)");
    return Number(value);
  }
  def("WRITE-BYTE", 2, 2, function* (a, r) {
    yield* writeStreamSteps(r, a[1], Uint8Array.of(octet(a[0])));
    return a[0];
  });
  for (const n of ["READ-SEQUENCE", "WRITE-SEQUENCE"])
    def(n, 2, Infinity, function* (a, r, t) {
      const seq = sequence(a[0]), writing = n === "WRITE-SEQUENCE", stream = a[1];
      const opts = keys(a.slice(2), ["START", "END"]), [start, end] = bounds(seq.length, opts.get("START") ?? 0, opts.get("END") ?? NIL);
      const character = streamCharacterMode(r, stream, writing ? "output" : "input", t);
      if (writing) {
        const items = Array.from({ length: end - start }, (_, i) => seq.get(start + i));
        const data = character
          ? items.map(value => value instanceof LispCharacter ? value.toString() : typeError(value, "character")).join("")
          : Uint8Array.from(items, octet);
        yield* writeStreamSteps(r, stream, data, t);
        return a[0];
      }
      const source = resolveSynonymStream(r, stream, t);
      let at = start;
      if (source instanceof LispFileStream && !source.activeReader(t)) {
        const data = yield* source.readData(end - start);
        for (const value of data) seq.set(at++, character ? new LispCharacter(value) : value);
      } else {
        while (at < end) {
          const value = yield* readStreamElement(r, stream, t, { character });
          if (value === undefined) break;
          seq.set(at++, character ? new LispCharacter(value) : value);
        }
      }
      return N.integer(at);
    });
  const inputP = (stream, task) => streamDirection(rt, stream, "input", task);
  const outputP = (stream, task) => streamDirection(rt, stream, "output", task);
  for (const [name, test] of [["INPUT-STREAM-P", inputP], ["OUTPUT-STREAM-P", outputP], ["OPEN-STREAM-P", (stream, task) => streamOpen(rt, stream, task)]])
    def(name, 1, 1, (a, _r, t) => rt.streamClassName(a[0]) === undefined ? typeError(a[0], "stream") : truth(test(a[0], t)));
  function requireDirection(stream, checker, expected, task) {
    if (!checker(stream, task)) typeError(stream, expected);
    return stream;
  }
  for (const [name, kind] of [["TWO-WAY-STREAM", "twoWay"], ["ECHO-STREAM", "echo"]]) {
    def(`MAKE-${name}`, 2, 2, ([input, output], _r, t) => new CompositeStream(kind, {
      inputStream: requireDirection(input, inputP, "input stream", t),
      outputStream: requireDirection(output, outputP, "output stream", t),
    }));
    unary(`${name}-INPUT-STREAM`, stream => composite(stream, kind, name).inputStream);
    unary(`${name}-OUTPUT-STREAM`, stream => composite(stream, kind, name).outputStream);
  }
  for (const [name, kind, checker, expected] of [
    ["CONCATENATED-STREAM", "concatenated", inputP, "input stream"], ["BROADCAST-STREAM", "broadcast", outputP, "output stream"],
  ]) {
    def(`MAKE-${name}`, 0, Infinity, (a, _r, t) => new CompositeStream(kind, { streams: a.map(stream => requireDirection(stream, checker, expected, t)) }));
    unary(`${name}-STREAMS`, stream => fromArray(composite(stream, kind, name).streams));
  }
  def("MAKE-SYNONYM-STREAM", 1, 1, ([symbol]) => new CompositeStream("synonym", { symbol: sym(symbol) }));
  unary("SYNONYM-STREAM-SYMBOL", stream => composite(stream, "synonym", "SYNONYM-STREAM").symbol);
  function composite(stream, kind, expected) {
    return stream instanceof CompositeStream && stream.kind === kind ? stream : typeError(stream, expected);
  }
  unary("INTERACTIVE-STREAM-P", stream => rt.streamClassName(stream) === undefined ? typeError(stream, "stream") : NIL);
  def("STREAM-ELEMENT-TYPE", 1, 1, ([stream], r, t) => streamElementType(r, stream, t));
  def("STREAM-EXTERNAL-FORMAT", 1, 1, ([stream], r, t) => {
    if (r.streamClassName(stream) === undefined) typeError(stream, "stream");
    const target = streamFileEndpoint(r, stream, t);
    return target instanceof LispFileStream ? target.externalFormat : r.intern("DEFAULT", "KEYWORD");
  });
  def("CLOSE", 1, Infinity, function* (a, _r, t) {
    const abort = (keys(a.slice(1), ["ABORT"]).get("ABORT") ?? NIL) !== NIL;
    const stream = a[0];
    if (stream instanceof LispFileStream) return yield* stream.close(abort, t);
    if (!(stream instanceof StringStream || stream instanceof CompositeStream)) typeError(stream, "stream");
    const open = stream.open;
    stream.open = false;
    return truth(open);
  });
  def("FILE-POSITION", 1, 2, function* (a, r, t) {
    const target = a.length === 1 ? undefined : filePositionDesignator(a[1]);
    const stream = streamFileEndpoint(r, a[0], t);
    if (stream instanceof LispFileStream) return yield* stream.filePosition(t, target);
    if (stream instanceof CompositeStream) return stream.kind === "broadcast" ? target === undefined ? 0 : NIL : NIL;
    const s = stringInput(stream);
    if (target === undefined) return s.position;
    const position = target === "end" ? s.end : target;
    if (position > s.end) return NIL;
    s.position = position;
    s.lastRead = null;
    return T;
  });
  def("FILE-LENGTH", 1, 1, ([stream], r, t) => {
    const target = streamFileEndpoint(r, stream, t);
    if (target instanceof LispFileStream) return target.fileLength();
    if (target instanceof CompositeStream && target.kind === "broadcast") return 0;
    return typeError(stream, "file-stream");
  });
  def("FILE-STRING-LENGTH", 2, 2, function* (a, r, t) {
    const stream = streamFileEndpoint(r, a[0], t);
    const text = a[1] instanceof LispCharacter ? a[1].toString() : str(a[1]);
    if (stream instanceof CompositeStream && stream.kind === "broadcast") return 1;
    if (!(stream instanceof LispFileStream)) typeError(a[0], "file-stream");
    stream.checkOpen();
    if (typeof stream.handle.stringLength !== "function") return NIL;
    const size = yield* stream.call("stringLength", text);
    return size === NIL ? NIL : N.integer(size);
  });
  unary("%INPUT-STRING-INDEX", (x) => stringInput(x).position);
  def("LISTEN", 0, 1, function* (a, r, t) {
    const stream = inputStream(a[0], t), character = streamCharacterMode(r, stream, "input", t);
    const next = yield* peekStreamInput(r, stream, t, 0, { character, noHang: true });
    return truth(next !== undefined && next !== NIL);
  });
  def("CLEAR-INPUT", 0, 1, (a, r, t) => {
    const stream = inputStream(a[0], t);
    if (!streamOpen(r, stream, t) || !streamDirection(r, stream, "input", t)) fail("STREAM-ERROR", "Stream cannot accept input", { stream });
    return NIL;
  });
  for (const method of ["FINISH-OUTPUT", "FORCE-OUTPUT", "CLEAR-OUTPUT"])
    def(method, 0, 1, (a, r, t) => flushStream(r, outputStream(r, a[0], t), method === "CLEAR-OUTPUT" ? "clearOutput" : "flush"));
}
