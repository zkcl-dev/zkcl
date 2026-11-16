import * as O from "./objects.mjs";
import {
  NIL, T, Cons, LispSymbol, LispCharacter, LispArray, LispPathname,
} from "./objects.mjs";
import * as N from "./numbers.mjs";
import { NativeFunction, LispCondition, LispRestart } from "./execution.mjs";
import { FormatLabels, FormatLayout } from "./format.mjs";
import { printFloat } from "./format_numeric.mjs";
import { pathnameNamestring } from "./pathnames.mjs";
import {
  fail, typeError, isString, str, string, integer, index, dynamicValue, readDefaultDouble,
} from "./builtins_helpers.mjs";

/** Object graph labels, package accessibility and truncation belong to one print operation. */
export function writeToString(value, options = {}) {
  return renderValueSteps(value, options).next().value;
}
export function* renderValueSteps(value, options, custom) {
  const {
    radix = 10,
    circle = false,
    readably = false,
    runtime: rt,
    task,
  } = options;
  const escape = readably || options.escape !== false,
    level = readably ? null : (options.level ?? null),
    length = readably ? null : (options.length ?? null);
  if (!Number.isInteger(radix) || radix < 2 || radix > 36)
    typeError(radix, "radix 2..36");
  const labels = circle ? options.formatLabels ?? new FormatLabels([value]) : null;
  labels?.include(value);
  const active = new Set();
  const depthLimited = (x, depth) => (x instanceof Cons || x instanceof LispArray && !isString(x) ||
    x?.kind === "structure-instance") && level !== null && depth >= level;
  const changeCase = (text) =>
    options.printCase === "DOWNCASE"
      ? text.toLowerCase()
      : options.printCase === "CAPITALIZE"
        ? text
            .toLowerCase()
            .replace(
              /(^|[^a-z0-9])([a-z])/gu,
              (_, pre, ch) => pre + ch.toUpperCase(),
            )
        : text;
  const symbolToken = (text) => {
    if (!escape) return changeCase(text);
    const safe =
      /^[A-Z+*/<>=!?_$%&~^.-][A-Z0-9+*/<>=!?_$%&~^.-]*$/u.test(text) &&
      !/^\.+$/u.test(text);
    return safe ? changeCase(text) : `|${text.replace(/[|\\]/gu, "\\$&")}|`;
  };
  const unreadable = (x, text) => {
    if (readably)
      fail("PRINT-NOT-READABLE", "Object has no readable representation", {
        object: x,
      });
    return text;
  };
  const appendRendered = (layout, text) => {
    if (text instanceof FormatLayout) layout.append(text);
    else layout.text(text);
  };
  function* render(x, depth = 0, asLayout = false) {
    const redundant = depth === 0 && x === options.printingRoot;
    const mark = !redundant && !depthLimited(x, depth) ? labels?.mark(x) : null;
    if (mark?.reference) return mark.text;
    const text = yield* body(x, depth, asLayout);
    if (text instanceof FormatLayout && mark) {
      const layout = new FormatLayout(0);
      layout.text(mark.text);
      layout.append(text);
      return layout;
    }
    return mark ? mark.text + text : text;
  }
  function* body(x, depth, asLayout) {
    if (custom) {
      const result = yield* custom(x, depth, labels);
      if (result !== undefined) return result;
    }
    if (x === NIL) return changeCase("NIL");
    if (x === T) return changeCase("T");
    if (N.isInteger(x)) {
      const digits = integer(x).toString(radix).toUpperCase();
      return options.printRadix
        ? radix === 10
          ? `${digits}.`
          : `${{ 2: "#b", 8: "#o", 16: "#x" }[radix] ?? `#${radix}r`}${digits}`
        : digits;
    }
    if (x instanceof N.Ratio) {
      const digits = `${integer(x.numerator).toString(radix).toUpperCase()}/${integer(x.denominator).toString(radix).toUpperCase()}`;
      return options.printRadix && radix !== 10
        ? `${{ 2: "#b", 8: "#o", 16: "#x" }[radix] ?? `#${radix}r`}${digits}`
        : digits;
    }
    if (x instanceof N.Complex)
      return `#C(${yield* render(x.real, depth + 1)} ${yield* render(x.imaginary, depth + 1)})`;
    if (N.isFloat(x)) {
      return printFloat(x, rt ? readDefaultDouble(rt, task) : false, readably);
    }
    if (isString(x) || typeof x === "string") {
      const text = str(x);
      return escape ? `"${text.replace(/["\\]/gu, "\\$&")}"` : text;
    }
    if (x instanceof N.RandomState) return `#Y"${x.toHex()}"`;
    if (x instanceof LispPathname) {
      const text = pathnameNamestring(x);
      if (text === null || readably && !rt?.pathnameReadably(x, text, task))
        return unreadable(x, "#<PATHNAME>");
      return escape ? `#P"${text.replace(/["\\]/gu, "\\$&")}"` : text;
    }
    if (x instanceof LispCharacter) {
      const text = x.toString();
      return escape ? `#\\${O.characterName(x.codePoint) ?? text}` : text;
    }
    if (x instanceof LispSymbol) {
      const token = symbolToken(x.name);
      if (!escape) return token;
      if (x.package === null)
        return options.gensym === false ? token : `#:${token}`;
      const pkg =
        typeof x.package === "string" ? rt?.findPackage(x.package) : x.package;
      if (pkg?.name === "KEYWORD" || x.package === "KEYWORD")
        return `:${token}`;
      const current = rt
        ? dynamicValue(rt, task, "*PACKAGE*", rt.currentPackage)
        : null;
      if (!rt || rt.findSymbol(x.name, current) === x) return token;
      return `${symbolToken(pkg?.name ?? x.package)}${pkg?.exports.has(x.name) ? ":" : "::"}${token}`;
    }
    if (depthLimited(x, depth)) return "#";
    if (x instanceof Cons) {
      const entered = [],
        parts = [];
      let tail = x;
      const finish = (dotted) => {
        if (!asLayout) return `(${parts.join(" ")}${dotted === undefined ? "" : ` . ${dotted}`})`;
        const body = new FormatLayout(1), layout = new FormatLayout(0);
        for (const [index, item] of parts.entries()) {
          if (index) { body.text(" "); body.newline("fill"); }
          appendRendered(body, item);
        }
        if (dotted !== undefined) {
          body.text(" . ");
          appendRendered(body, dotted);
        }
        layout.block("(", ")", false, body, false);
        return layout;
      };
      try {
        while (tail instanceof Cons) {
          if (parts.length && labels?.repeated.has(tail))
            return finish(yield* render(tail, depth + 1, asLayout));
          if (length !== null && parts.length >= length) {
            parts.push("...");
            return finish();
          }
          if (active.has(tail))
            fail(
              "PRINT-NOT-READABLE",
              "Circular object requires circle printing",
            );
          active.add(tail);
          entered.push(tail);
          parts.push(yield* render(tail.car, depth + 1, asLayout));
          tail = tail.cdr;
        }
        return finish(tail === NIL ? undefined : yield* render(tail, depth + 1, asLayout));
      } finally {
        entered.forEach((v) => active.delete(v));
      }
    }
    if (x instanceof LispArray) {
      if (options.array === false && !readably)
        return unreadable(x, `#<ARRAY ${x.dimensions.join("x")}>`);
      if (active.has(x))
        fail("PRINT-NOT-READABLE", "Circular object requires circle printing");
      active.add(x);
      try {
        const size = x.rank === 1 ? x.length : x.totalSize,
          shown = length === null ? size : Math.min(size, length);
        if (x.rank === 1) {
          if (x.elementKind === "bit")
            return `#*${Array.from({ length: shown }, (_, i) => String(x.get(i))).join("")}${shown < size ? "..." : ""}`;
          const elements = [];
          for (let i = 0; i < shown; i++) elements.push(yield* render(x.get(i), depth + 1));
          if (shown < size) elements.push("...");
          return `#(${elements.join(" ")})`;
        }
        let cursor = 0;
        const dimensions = function* (axis) {
          if (axis === x.rank) return yield* render(x.get(cursor++), depth + axis);
          const elements = [];
          for (let i = 0; i < x.dimensions[axis]; i++) elements.push(yield* dimensions(axis + 1));
          return `(${elements.join(" ")})`;
        };
        return `#${x.rank}A${yield* dimensions(0)}`;
      } finally {
        active.delete(x);
      }
    }
    if (x?.kind === "structure-instance") {
      const definition = rt?.structureDefinition?.(x.classObject.name);
      if (definition) {
        const fields = [yield* render(x.classObject.name)];
        for (const [i, slot] of definition.slots.entries())
          fields.push(`:${symbolToken(slot.name.name)}`, yield* render(x.values[i], depth + 1));
        return `#S(${fields.join(" ")})`;
      }
    }
    if (
      x instanceof NativeFunction ||
      x instanceof O.Closure ||
      typeof x?.invoke === "function"
    )
      return unreadable(x, `#<FUNCTION ${x.name ?? "ANONYMOUS"}>`);
    if (x instanceof LispCondition)
      return unreadable(x, escape ? `#<${x.className}>` : x.message);
    if (x instanceof LispRestart)
      return unreadable(x, `#<RESTART ${x.name.name}>`);
    if (rt?.isStandardInstance?.(x)) return unreadable(x, `#<${x.classObject.name.name}>`);
    return unreadable(x, `#<${x?.constructor?.name ?? typeof x}>`);
  }
  const rendered = yield* render(value, 0, !!options.pretty);
  if (!(rendered instanceof FormatLayout)) return rendered;
  if (options.layout) return rendered;
  const column = options.column ?? 0;
  const margin = options.margin ?? 72;
  return rendered.render(margin, options.miser, column, "", "", [], true,
    { remaining: readably ? Infinity : options.lines ?? Infinity }).text;
}
