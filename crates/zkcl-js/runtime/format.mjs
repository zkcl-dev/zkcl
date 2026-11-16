import { NIL, T, Cons, LispArray, LispCharacter, LispPathname, LispString, LispSymbol, eql } from "./objects.mjs";
import { LispCondition } from "./execution.mjs";
import * as N from "./numbers.mjs";
import { englishOrRoman, floatingField } from "./format_numeric.mjs";

const closing = { "(": ")", "[": "]", "{": "}", "<": ">" };
const directives = "ASWDBOXRFEG$PC%&|~\n_IT*?^/";
const nestingLimit = 128;
const escape = Symbol("format escape");
const escapeIteration = Symbol("format iteration escape");
const logicalTail = Symbol("logical block tail");
const nextArgument = Symbol("format V parameter");
const remainingArguments = Symbol("format # parameter");
const width = (text) => [...text].length;
const pad = (character, count) => character.repeat(Math.max(0, count));
const asciiUpper = (text) => text.replace(/[a-z]/g, (letter) => letter.toUpperCase());

/** Identity labels shared by object fields and list spines in a logical block. */
export class FormatLabels {
  constructor(values) {
    this.known = new Set();
    this.repeated = new Set();
    this.labels = new Map();
    this.visit([...values]);
  }
  include(value) {
    if (!this.known.has(value)) this.visit([value]);
  }
  visit(pending) {
    while (pending.length) {
      const value = pending.pop();
      if (!(value instanceof Cons || value instanceof LispString || value instanceof LispArray ||
          value instanceof LispPathname || value instanceof N.RandomState || value instanceof LispCondition ||
          value instanceof LispSymbol && value.package === null ||
          ["structure-instance", "standard-instance"].includes(value?.kind))) continue;
      if (this.known.has(value)) {
        this.repeated.add(value);
        continue;
      }
      this.known.add(value);
      if (value instanceof Cons) pending.push(value.car, value.cdr);
      else if (value instanceof LispArray && value.elementKind === "general")
        for (let i = 0; i < (value.rank === 1 ? value.length : value.totalSize); i++) pending.push(value.get(i));
      else if (value?.kind === "structure-instance") pending.push(...value.values);
    }
  }
  mark(value) {
    if (!this.repeated.has(value)) return null;
    if (this.labels.has(value)) return { text: `#${this.labels.get(value)}#`, reference: true };
    const label = this.labels.size + 1;
    this.labels.set(value, label);
    return { text: `#${label}=`, reference: false };
  }
}

function formatError(message, position) {
  throw new LispCondition("FORMAT-ERROR", message, { position });
}

/** Parse paired directives before executing any of their branches. */
export function parseFormat(text) {
  const characters = [...text];
  let position = 0;
  function directive() {
    const at = position - 1;
    const params = [];
    const parameter = () => {
      const ch = characters[position];
      if (ch === "v" || ch === "V" || ch === "#") {
        position++;
        return ch === "#" ? remainingArguments : nextArgument;
      }
      if (ch === "'") {
        position++;
        if (position === characters.length) formatError("Incomplete character parameter", at);
        return new LispCharacter(characters[position++]);
      }
      if (ch === "+" || ch === "-" || /[0-9]/.test(ch ?? "")) {
        const start = position++;
        if ((ch === "+" || ch === "-") && !/[0-9]/.test(characters[position] ?? ""))
          formatError("Signed parameter requires digits", at);
        while (/[0-9]/.test(characters[position] ?? "")) position++;
        return N.integer(BigInt(characters.slice(start, position).join("")));
      }
      return NIL;
    };
    let value = parameter();
    if (value !== NIL || characters[position] === ",") {
      params.push(value);
      while (characters[position] === ",") {
        position++;
        params.push(parameter());
      }
    }
    let colon = false, atsign = false;
    while (characters[position] === ":" || characters[position] === "@") {
      if (characters[position++] === ":") {
        if (colon) formatError("Duplicate colon modifier", at);
        colon = true;
      } else {
        if (atsign) formatError("Duplicate at-sign modifier", at);
        atsign = true;
      }
    }
    if (position >= characters.length) formatError("Incomplete FORMAT directive", at);
    const op = asciiUpper(characters[position++]);
    let name;
    if (op === "/") {
      const start = position;
      while (position < characters.length && characters[position] !== "/") position++;
      if (start === position || position === characters.length)
        formatError("Expected a function name between slashes", at);
      name = asciiUpper(characters.slice(start, position++).join(""));
    }
    if (op === "\n" && !colon)
      while ([" ", "\t", "\r"].includes(characters[position])) position++;
    return { op, params, colon, atsign, at, name };
  }
  function parts(depth, terminators = "") {
    if (depth >= nestingLimit) formatError("FORMAT control nests too deeply", position);
    const nodes = [];
    while (position < characters.length) {
      if (characters[position] !== "~") {
        const start = position++;
        while (position < characters.length && characters[position] !== "~") position++;
        nodes.push(characters.slice(start, position).join(""));
        continue;
      }
      position++;
      const node = directive();
      if (");]}>".includes(node.op)) {
        if (!terminators.includes(node.op)) formatError(`Unexpected ~${node.op}`, node.at);
        return { nodes, end: node };
      }
      if (Object.hasOwn(closing, node.op)) {
        const close = closing[node.op];
        const separators = close === "]" || close === ">" ? ";" : "";
        node.sections = [];
        do {
          const section = parts(depth + 1, close + separators);
          if (!section.end) formatError(`Missing ~${close}`, node.at);
          node.sections.push(section);
        } while (node.sections.at(-1).end.op !== close);
      } else if (!directives.includes(node.op)) {
        formatError(`Unknown directive ~${node.op}`, node.at);
      }
      nodes.push(node);
    }
    return { nodes };
  }
  const syntax = parts(0).nodes;
  let specialJustification, pretty;
  const visit = (nodes, enclosedByJustification = false) => {
    for (const node of nodes) {
      if (typeof node === "string") continue;
      const justify = node.op === "<" && !node.sections.at(-1).end.colon;
      const needsLayout = "WI_".includes(node.op) || node.op === "T" && node.colon || node.op === "<" && !justify;
      if (needsLayout) {
        pretty = node;
        if (enclosedByJustification)
          formatError("Pretty printing directives are undefined inside justification", node.at);
      }
      if (justify && node.sections[0].end.op === ";" && node.sections[0].end.colon)
        specialJustification = node;
      for (const section of node.sections ?? []) visit(section.nodes, enclosedByJustification || justify);
    }
  };
  visit(syntax);
  if (pretty && specialJustification)
    formatError("Conditional line prefixes cannot share a control string with pretty printing directives", specialJustification.at);
  return syntax;
}

class Cursor {
  constructor(values, moreSublists = undefined) {
    this.values = values;
    this.position = 0;
    this.moreSublists = moreSublists;
  }
  get remaining() { return this.values.length - this.position; }
  rest() { return this.values.slice(this.position); }
  move(position, node) {
    if (position < 0 || position > this.values.length)
      formatError("FORMAT argument index is out of range", node.at);
    this.position = position;
  }
  next(node) {
    if (!this.remaining) formatError("FORMAT ran out of arguments", node?.at);
    const value = this.values[this.position++];
    if (value?.[logicalTail]) throw value;
    return value;
  }
}

class Parameters {
  constructor(node, cursor) {
    this.node = node;
    this.values = node.params.map((parameter) => {
      if (parameter === remainingArguments) return cursor.remaining;
      if (parameter !== nextArgument) return parameter;
      const value = cursor.next(node);
      if (value !== NIL && !(value instanceof LispCharacter) && !N.isInteger(value))
        this.fail("V parameter requires an integer, character, or NIL");
      return value;
    });
  }
  fail(message) { formatError(message, this.node.at); }
  count(max) {
    if (this.values.length > max) this.fail(`Too many parameters for ~${this.node.op}`);
  }
  get(index) { return this.values[index] ?? NIL; }
  integer(index, fallback) {
    const value = this.get(index);
    if (value === NIL) return fallback;
    if (!N.isInteger(value)) this.fail("Expected an integer parameter");
    const number = Number(N.toBigInt(value));
    if (!Number.isSafeInteger(number)) this.fail("Integer parameter is too large");
    return number;
  }
  size(index, fallback = 0) {
    const value = this.get(index);
    if (N.isInteger(value) && N.toBigInt(value) < 0n) return 0;
    return this.integer(index, fallback);
  }
  character(index, fallback) {
    const value = this.get(index);
    if (value === NIL) return fallback;
    if (!(value instanceof LispCharacter)) this.fail("Expected a character parameter");
    return value.toString();
  }
}

function splitWidths(params) {
  params.count(4);
  return {
    mincol: params.size(0),
    colinc: Math.max(1, params.size(1, 1)),
    minpad: params.size(2),
    padchar: params.character(3, " "),
  };
}

function paddedWidth(size, { mincol, colinc }) {
  return mincol + Math.max(0, Math.ceil((size - mincol) / colinc)) * colinc;
}

function caseConvert(text, { colon, atsign }) {
  if (colon && atsign) return text.toUpperCase();
  const lower = text.toLowerCase();
  if (colon) return lower.replace(/[\p{L}\p{N}]+/gu, (word) => {
    const [first, ...rest] = word;
    return first.toUpperCase() + rest.join("");
  });
  if (atsign) return lower.replace(/[\p{L}\p{N}]/u, (letter) => letter.toUpperCase());
  return lower;
}

function shouldEscape(node, params, cursor) {
  params.count(3);
  if (node.atsign) params.fail("~@^ is undefined");
  if (node.colon && cursor.moreSublists === undefined)
    params.fail("~:^ requires an enclosing colon iteration");
  const a = params.get(0), b = params.get(1), c = params.get(2);
  if (c !== NIL) {
    const ordered = (from, to) => {
      if (N.isInteger(from) && N.isInteger(to)) return N.toBigInt(from) <= N.toBigInt(to);
      if (from instanceof LispCharacter && to instanceof LispCharacter) return from.codePoint <= to.codePoint;
      return params.fail("~^ ordering requires parameters of the same type");
    };
    return ordered(a, b) && ordered(b, c);
  }
  if (b !== NIL) return eql(a, b);
  if (a !== NIL) return eql(a, 0);
  return node.colon ? !cursor.moreSublists : !cursor.remaining;
}

function tabPadding(current, origin, colnum, colinc, relative) {
  if (current === undefined) return relative ? colnum : 2;
  const column = current - origin;
  if (relative) {
    if (colinc <= 0) return colnum;
    const remainder = ((column + colnum) % colinc + colinc) % colinc;
    return colnum + (remainder ? colinc - remainder : 0);
  }
  if (column < colnum) return colnum - column;
  return colinc ? colinc - (column - colnum) % colinc : 0;
}

/** Layout operations collected in a logical block, including callback output. */
export class FormatLayout {
  constructor(column, section = column, fill = false) {
    this.items = [];
    this.start = column;
    this.column = column;
    this.section = section;
    this.fill = fill;
    this.depth = 0;
  }
  text(text, literal = false) {
    if (literal && this.fill) {
      for (const match of text.matchAll(/ +|[^ ]+/gu)) {
        this.text(match[0]);
        if (match[0].startsWith(" ")) this.newline("fill");
      }
      return;
    }
    this.items.push({ text });
    const newline = text.lastIndexOf("\n");
    this.column = newline < 0 ? this.column + width(text) : width(text.slice(newline + 1));
  }
  newline(kind) {
    this.items.push({ kind });
    this.section = this.column;
  }
  indent(amount, relative) { this.items.push({ amount, relative }); }
  tab(colnum, colinc, relative, section) {
    this.items.push({ colnum, colinc, relative, section });
    this.column += tabPadding(this.column, section ? this.section : 0, colnum, colinc, relative);
  }
  block(prefix, suffix, perLine, inner, measureSuffix = true) {
    this.items.push({ prefix, suffix, perLine, inner, measureSuffix });
    this.column = inner.column + width(suffix);
  }
  append(layout) {
    for (const item of layout.items) {
      if (item.text !== undefined) this.text(item.text);
      else if (item.kind) this.newline(item.kind);
      else if (item.colnum !== undefined) this.tab(item.colnum, item.colinc, item.relative, item.section);
      else if (item.amount !== undefined) this.items.push(item);
      else {
        this.items.push(item);
        this.column += width(item.prefix) + item.inner.column - item.inner.start + width(item.suffix);
      }
    }
  }
  measure(from = 0, stopAtBreak = false) {
    let size = 0;
    for (let index = from; index < this.items.length; index++) {
      const item = this.items[index];
      if (item.kind === "mandatory" || item.text?.includes("\n")) return Infinity;
      if (item.kind && stopAtBreak) break;
      if (item.text !== undefined) size += width(item.text);
      else if (item.inner)
        size += width(item.prefix) + item.inner.measure() + width(item.suffix);
      else if (item.colnum !== undefined) size += item.colnum;
    }
    return size;
  }
  render(margin, miser, column = this.start, prefix = "", suffix = "", prefixes = [], measureSuffix = true,
    lines = { remaining: Infinity }) {
    let text = prefix, truncated = false;
    const shorten = () => { truncated = true; text = `${text.replace(/ +$/u, "")} ..`; };
    column += width(prefix);
    const start = column;
    let section = start, indent = start, broken = false, trim = false;
    const misering = miser !== undefined && margin - start <= miser;
    const suffixWidth = measureSuffix ? width(suffix) : 0;
    const linear = start + this.measure() + suffixWidth > margin || misering;
    for (let index = 0; index < this.items.length; index++) {
      const item = this.items[index];
      if (item.text !== undefined) {
        let from = 0, newline = item.text.indexOf("\n");
        while (newline >= 0) {
          if (--lines.remaining < 1) {
            text += item.text.slice(0, newline);
            shorten();
            break;
          }
          from = newline + 1;
          newline = item.text.indexOf("\n", from);
        }
        if (truncated) break;
        text += item.text;
        if (!from) column += width(item.text);
        else {
          column = width(item.text.slice(from));
          section = column;
          broken = true;
        }
        trim = !from || from < item.text.length;
      } else if (item.kind) {
        const split = item.kind === "mandatory" ||
          (item.kind === "miser" && misering) ||
          (item.kind === "linear" && linear) ||
          (item.kind === "fill" && (misering || broken ||
            column + this.measure(index + 1, true) + suffixWidth > margin));
        if (split) {
          if (--lines.remaining < 1) { shorten(); break; }
          if (trim) text = text.replace(/ +$/u, "");
          const target = misering ? start : indent;
          let leader = "";
          for (const [offset, p] of prefixes) leader += pad(" ", offset - width(leader)) + p;
          text += `\n${leader}${pad(" ", target - width(leader))}`;
          column = Math.max(target, width(leader));
          broken = false;
          trim = false;
        }
        section = column;
      } else if (item.amount !== undefined) {
        indent = Math.max(0, (item.relative ? column : start) + item.amount);
      } else if (item.colnum !== undefined) {
        const count = tabPadding(column, item.section ? section : 0, item.colnum, item.colinc, item.relative);
        text += pad(" ", count);
        column += count;
        trim = true;
      } else if (item.inner) {
        const nested = item.inner.render(margin, miser, column, item.prefix, item.suffix,
          item.perLine ? [...prefixes, [column, item.prefix]] : prefixes, item.measureSuffix, lines);
        text += nested.text;
        column = nested.column;
        if (nested.truncated) { truncated = true; break; }
        broken ||= nested.text.includes("\n");
        trim = true;
      }
    }
    return { text: text + suffix, column: column + width(suffix), truncated };
  }
}

class Formatter {
  constructor(env) { this.env = env; }
  control(value) {
    if (this.env.isString(value)) return parseFormat(this.env.string(value));
    if (this.env.isFunction(value)) return value;
    return this.env.typeError(value, "or string function");
  }
  *text(out, text, literal = false) {
    if (text) yield* this.env.write(out, text, literal);
  }
  column(out) { return out.layout?.column ?? this.env.column(out); }
  capture(out) {
    const capture = this.env.capture(this.column(out));
    capture.formatLabels = out.formatLabels;
    capture.printingRoot = out.printingRoot;
    return capture;
  }
  list(value) { return this.env.list(value); }
  *callControl(control, cursor, out, consume) {
    const args = cursor.rest();
    if (args.at(-1)?.[logicalTail]) args.pop();
    const tail = yield* this.env.call(control, [out, ...args]);
    if (!consume) return;
    const unused = this.list(tail).length;
    if (unused > args.length) formatError("Formatter returned more arguments than it received");
    cursor.position += args.length - unused;
  }
  *run(control, args, out, consumeFunction = false) {
    const cursor = new Cursor(args);
    if (Array.isArray(control)) yield* this.parts(control, cursor, out, 0);
    else yield* this.callControl(control, cursor, out, consumeFunction);
    return cursor.rest();
  }
  *parts(nodes, cursor, out, depth) {
    if (depth >= nestingLimit) formatError("FORMAT execution nests too deeply");
    for (const node of nodes) {
      if (typeof node === "string") yield* this.text(out, node, true);
      else {
        const params = new Parameters(node, cursor);
        const flow = node.sections
          ? yield* this.group(node, params, cursor, out, depth + 1)
          : yield* this.directive(node, params, cursor, out, depth);
        if (flow) return flow;
      }
    }
  }
  *directive(node, params, cursor, out, depth) {
    const { op, colon, atsign } = node;
    switch (op) {
      case "A": case "S": {
        const field = splitWidths(params), value = cursor.next(node);
        if (!(colon && value === NIL) && !field.mincol && !field.minpad) {
          yield* this.env.print(out, value, op);
          return;
        }
        const capture = this.capture(out);
        if (colon && value === NIL) capture.write("()");
        else yield* this.env.print(capture, value, op);
        const padding = pad(field.padchar, field.minpad + field.colinc * Math.max(0,
          Math.ceil((field.mincol - field.minpad - width(capture.text)) / field.colinc)));
        yield* this.text(out, atsign ? padding + capture.text : capture.text + padding);
        return;
      }
      case "W":
        params.count(0);
        yield* this.env.print(out, cursor.next(node), op, { pretty: colon, unlimited: atsign });
        return;
      case "D": case "B": case "O": case "X": case "R": {
        const value = cursor.next(node);
        let base = { D: 10, B: 2, O: 8, X: 16 }[op];
        if (op === "R") {
          base = params.integer(0);
          if (base === undefined) {
            params.count(1);
            if (!N.isInteger(value)) this.env.typeError(value, "integer");
            yield* this.text(out, englishOrRoman(N.toBigInt(value), colon, atsign, (message) => params.fail(message)));
            return;
          }
          if (base < 2 || base > 36) params.fail("~R radix must be between 2 and 36");
          params.values.shift();
        }
        params.count(4);
        const mincol = params.size(0), padchar = params.character(1, " ");
        let text;
        if (N.isInteger(value)) {
          const number = N.toBigInt(value), magnitude = number < 0n ? -number : number;
          text = magnitude.toString(base).toUpperCase();
          if (colon) {
            const comma = params.character(2, ","), interval = params.size(3, 3);
            if (!interval) params.fail("Digit group size must be positive");
            const groups = [];
            for (let end = text.length; end > 0; end -= interval)
              groups.push(text.slice(Math.max(0, end - interval), end));
            text = groups.reverse().join(comma);
          }
          if (number < 0n) text = "-" + text;
          else if (atsign) text = "+" + text;
        } else {
          const capture = this.capture(out);
          yield* this.env.print(capture, value, "A", { radix: base, printRadix: false });
          text = capture.text;
        }
        yield* this.text(out, pad(padchar, mincol - width(text)) + text);
        return;
      }
      case "F": case "E": case "G": case "$": {
        const value = cursor.next(node);
        params.count({ F: 5, E: 7, G: 7, "$": 4 }[op]);
        if (colon && op !== "$") params.fail(`Colon is undefined for ~${op}`);
        if (!N.isReal(value)) {
          const capture = this.capture(out);
          yield* this.env.print(capture, value, "A", { radix: 10, printRadix: false });
          yield* this.text(out, pad(" ", params.size(op === "$" ? 2 : 0) - width(capture.text)) + capture.text);
          return;
        }
        yield* this.text(out, floatingField(op, params, value, this.env.options()));
        return;
      }
      case "C": {
        params.count(0);
        const value = cursor.next(node);
        if (!(value instanceof LispCharacter)) this.env.typeError(value, "character");
        if (colon) yield* this.text(out, this.env.characterName(value) ?? value.toString());
        else if (atsign) yield* this.env.print(out, value, "S");
        else yield* this.text(out, value.toString());
        return;
      }
      case "P": {
        params.count(0);
        if (colon && !cursor.position) params.fail("~:P needs a previous argument");
        const singular = eql(colon ? cursor.values[cursor.position - 1] : cursor.next(node), 1);
        yield* this.text(out, atsign ? (singular ? "y" : "ies") : (singular ? "" : "s"));
        return;
      }
      case "%": case "&": case "|": case "~": {
        params.count(1);
        if (colon || atsign) params.fail(`Modifiers are undefined for ~${op}`);
        const count = params.size(0, 1) - (op === "&" && this.column(out) === 0 ? 1 : 0);
        yield* this.text(out, pad({ "|": "\f", "~": "~" }[op] ?? "\n", count));
        return;
      }
      case "\n":
        params.count(0);
        if (colon && atsign) params.fail("~:@ followed by newline is undefined");
        if (atsign) yield* this.text(out, "\n");
        return;
      case "_":
        params.count(0);
        if (this.env.options().pretty)
          out.layout?.newline(colon ? (atsign ? "mandatory" : "fill") : (atsign ? "miser" : "linear"));
        return;
      case "I":
        params.count(1);
        if (atsign) params.fail("~@I is undefined");
        if (this.env.options().pretty) out.layout?.indent(params.integer(0, 0), colon);
        return;
      case "T": {
        params.count(2);
        const column = params.size(0, 1), increment = params.size(1, 1);
        if (out.layout && this.env.options().pretty) out.layout.tab(column, increment, atsign, colon);
        else if (!colon) yield* this.text(out, pad(" ", tabPadding(this.column(out), 0, column, increment, atsign)));
        return;
      }
      case "*": {
        params.count(1);
        if (colon && atsign) params.fail("~:@* is undefined");
        const offset = params.size(0, atsign ? 0 : 1);
        cursor.move(atsign ? offset : cursor.position + (colon ? -offset : offset), node);
        return;
      }
      case "?": {
        params.count(0);
        if (colon) params.fail("~:? is undefined");
        const control = this.control(cursor.next(node));
        const local = atsign ? cursor : new Cursor(this.list(cursor.next(node)));
        if (!Array.isArray(control)) yield* this.callControl(control, local, out, atsign);
        else if ((yield* this.parts(control, local, out, depth + 1)) === escapeIteration) return escapeIteration;
        return;
      }
      case "^":
        if (shouldEscape(node, params, cursor)) return colon ? escapeIteration : escape;
        return;
      case "/":
        yield* this.env.call(this.env.namedFunction(node.name), [
          out, cursor.next(node), colon ? T : NIL, atsign ? T : NIL, ...params.values,
        ]);
        return;
    }
  }
  *group(node, params, cursor, out, depth) {
    const { op, colon, atsign, sections } = node;
    if (op === "(") {
      params.count(0);
      const capture = this.capture(out);
      const flow = yield* this.parts(sections[0].nodes, cursor, capture, depth);
      yield* this.text(out, caseConvert(capture.text, node));
      return flow;
    }
    if (op === "[") {
      const fallback = sections.findIndex(({ end }) => end.op === ";" && end.colon);
      let index;
      if (colon || atsign) {
        params.count(0);
        if (colon && atsign || sections.length !== (colon ? 2 : 1) || fallback >= 0)
          params.fail("Invalid conditional clauses");
        const value = cursor.next(node);
        if (colon) index = value === NIL ? 0 : 1;
        else if (value !== NIL) { cursor.position--; index = 0; }
      } else {
        params.count(1);
        if (fallback >= 0 && fallback + 2 !== sections.length)
          params.fail("The default conditional clause must be last");
        const value = params.get(0) === NIL ? cursor.next(node) : params.get(0);
        if (!N.isInteger(value)) this.env.typeError(value, "integer");
        index = Number(N.toBigInt(value));
        if (index < 0 || index >= (fallback < 0 ? sections.length : fallback + 1))
          index = fallback < 0 ? undefined : fallback + 1;
      }
      if (index !== undefined) return yield* this.parts(sections[index].nodes, cursor, out, depth);
      return;
    }
    if (op === "{") {
      params.count(1);
      const limit = params.size(0, Infinity), { nodes, end } = sections[0];
      const body = nodes.length ? nodes : this.control(cursor.next(node));
      const local = new Cursor(atsign ? cursor.rest() : this.list(cursor.next(node)));
      const seen = new Set();
      for (let i = 0; i < limit && (local.remaining || (i === 0 && end.colon)); i++) {
        if (!colon && limit === Infinity && Array.isArray(body)) {
          if (seen.has(local.position)) params.fail("Unbounded iteration revisits the same arguments");
          seen.add(local.position);
        }
        const target = colon ? new Cursor(local.remaining ? this.list(local.next(node)) : [], local.remaining > 0) : local;
        const start = target.position;
        let flow;
        if (Array.isArray(body)) flow = yield* this.parts(body, target, out, depth);
        else yield* this.callControl(body, target, out, !colon);
        if (flow === escapeIteration || (!colon && flow === escape)) break;
        if (!Array.isArray(body) && !colon && target.position === start && limit === Infinity) break;
      }
      if (atsign) cursor.position += local.position;
      return;
    }
    if (sections.at(-1).end.colon) return yield* this.logicalBlock(node, params, cursor, out, depth);
    return yield* this.justify(node, params, cursor, out, depth);
  }
  *justify(node, params, cursor, out, depth) {
    const field = splitWidths(params), fragments = [];
    let conditionalPrefix;
    for (const { nodes, end } of node.sections) {
      const capture = this.env.capture(0);
      const flow = yield* this.parts(nodes, cursor, capture, depth);
      if (flow === escapeIteration) return flow;
      if (flow === escape) break;
      if (end.colon) {
        if (fragments.length || conditionalPrefix) params.fail("The line prefix must be first");
        const prefix = new Parameters(end, cursor);
        prefix.count(2);
        conditionalPrefix = [capture.text, prefix.size(0), prefix.size(1, 72)];
      } else fragments.push(capture.text);
    }
    if (!fragments.length) return;
    const leading = node.colon || (fragments.length === 1 && !node.atsign);
    const gaps = fragments.length - 1 + Number(leading) + Number(node.atsign);
    const contents = fragments.reduce((size, text) => size + width(text), 0);
    const total = paddedWidth(contents + gaps * field.minpad, field);
    if (conditionalPrefix) {
      const [prefix, spare, margin] = conditionalPrefix;
      if ((this.column(out) ?? 0) + total + spare > margin) yield* this.text(out, prefix);
    }
    let remaining = total - contents, gapsLeft = gaps;
    const formatter = this;
    function* padding() {
      const units = Math.floor(remaining / gapsLeft--);
      remaining -= units;
      yield* formatter.text(out, pad(field.padchar, units));
    }
    if (leading) yield* padding();
    for (const [index, text] of fragments.entries()) {
      if (index) yield* padding();
      yield* this.text(out, text);
    }
    if (node.atsign) yield* padding();
  }
  *logicalBlock(node, params, cursor, out, depth) {
    params.count(0);
    const sections = node.sections;
    if (sections.length > 3) params.fail("Too many logical block clauses");
    const literal = ({ nodes }) => {
      if (nodes.some((part) => typeof part !== "string")) params.fail("Logical block affixes must be literal text");
      return nodes.join("");
    };
    let prefix = sections.length > 1 ? literal(sections[0]) : (node.colon ? "(" : "");
    const suffix = sections.length > 2 ? literal(sections[2]) : (node.colon ? ")" : "");
    const argument = node.atsign ? NIL : cursor.next(node);
    if (argument !== NIL && !(argument instanceof Cons)) {
      yield* this.env.print(out, argument, "W");
      return;
    }
    const options = this.env.options();
    const blockDepth = out.layout?.depth ?? 0;
    let values = node.atsign ? cursor.rest() : [];
    if (node.atsign) cursor.position = cursor.values.length;
    if (options.level !== undefined && blockDepth >= options.level) {
      yield* this.text(out, "#");
      return;
    }
    const labels = options.circle ? out.formatLabels ?? new FormatLabels(node.atsign ? values : [argument]) : null;
    const mark = node.atsign || argument === out.printingRoot ? null : labels?.mark(argument);
    if (mark?.reference) {
      yield* this.text(out, mark.text);
      return;
    }
    if (mark) prefix = mark.text + prefix;
    let tail;
    if (node.atsign) {
      if (options.length !== undefined && values.length > options.length) {
        values.length = options.length;
        tail = { [logicalTail]: true, text: "..." };
      }
    } else {
      let item = argument;
      const seen = new Set();
      while (item instanceof Cons && !seen.has(item) && values.length !== options.length &&
          (item === argument || !labels?.repeated.has(item))) {
        seen.add(item);
        values.push(item.car);
        item = item.cdr;
      }
      if (item !== NIL) {
        tail = { [logicalTail]: true };
        if (values.length === options.length) tail.text = "...";
        else if (seen.has(item) && !options.circle) tail.text = ". #<CIRCULAR-LIST>";
        else tail.value = item;
      }
    }
    if (tail) values.push(tail);
    const local = new Cursor(values);
    const column = this.column(out) ?? 0;
    const stream = this.env.capture(column + width(prefix));
    stream.formatLabels = labels;
    stream.layout = new FormatLayout(column + width(prefix), column + width(prefix),
      options.pretty && sections.at(-1).end.atsign);
    stream.layout.depth = blockDepth + 1;
    try {
      try {
        yield* this.parts(sections[Number(sections.length > 1)].nodes, local, stream, depth);
      } catch (error) {
        if (error !== tail) throw error;
        if (tail.text !== undefined) yield* this.text(stream, tail.text);
        else {
          yield* this.text(stream, ". ");
          yield* this.env.print(stream, tail.value, "W");
        }
      }
    } finally {
      stream.open = false;
      const perLine = sections.length > 1 && sections[0].end.atsign;
      if (out.layout) out.layout.block(prefix, suffix, perLine, stream.layout);
      else {
        const text = stream.layout.render(options.margin, options.miser, column, prefix, suffix,
          perLine ? [[column, prefix]] : [], true, { remaining: options.lines ?? Infinity }).text;
        yield* this.text(out, text);
      }
    }
  }
}

export function* executeFormat(control, args, out, env) {
  const formatter = new Formatter(env);
  return yield* formatter.run(Array.isArray(control) ? control : formatter.control(control), args, out);
}

/** The standard list pretty printers use the same stream layout as FORMAT. */
export function* pprintList(values, out, env, kind, colon, tabsize = 16) {
  if (values !== NIL && !(values instanceof Cons)) {
    yield* env.print(out, values, "W");
    return;
  }
  const opener = colon ? "~:<" : "~<";
  const separator = kind === "tabular" ? ` ~0,${tabsize}:@T~:_` : kind === "linear" ? " ~_" : " ~:_";
  const control = parseFormat(opener + "~@{~W~^" + separator + "~}~:>");
  yield* executeFormat(control, [values], out, env);
}
