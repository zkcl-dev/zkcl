import * as O from "./objects.mjs";
import { NIL, LispString, LispCharacter } from "./objects.mjs";
import {
  truth, fail, typeError, isString, str, string, name, integer, index, boxInteger,
  keys, sequence, bounds,
} from "./builtins_helpers.mjs";

export function installCharacters(rt, def, unary, pred) {
  const character = (x) =>
    x instanceof LispCharacter ? x : typeError(x, "character");
  unary("CHAR-CODE", (x) => character(x).codePoint);
  unary("CHAR-INT", (x) => character(x).codePoint);
  unary("CODE-CHAR", (x) => {
    const n = integer(x);
    return n < 0n || n > 0x10ffffn || (n >= 0xd800n && n <= 0xdfffn)
      ? NIL
      : new LispCharacter(Number(n));
  });
  unary("CHARACTER", (x) =>
    x instanceof LispCharacter
      ? x
      : [...name(x)].length === 1
        ? new LispCharacter(name(x))
        : typeError(x, "character designator"),
  );
  def("CHAR", 2, 2, (a) => {
    if (!isString(a[0])) typeError(a[0], "string");
    return a[0].get(index(a[1]));
  });
  def("SCHAR", 2, 2, (a) => {
    if (!isString(a[0])) typeError(a[0], "string");
    return a[0].get(index(a[1]));
  });
  for (const n of ["CHAR", "SCHAR"])
    def(`(SETF ${n})`, 3, 3, (a) => {
      if (!isString(a[1])) typeError(a[1], "string");
      return a[1].set(index(a[2]), character(a[0]));
    });
  unary("STRING", (x) => (isString(x) ? x : string(name(x))));
  def("MAKE-STRING", 1, Infinity, (a) => {
    const k = keys(a.slice(1), ["INITIAL-ELEMENT", "ELEMENT-TYPE"]),
      ch = character(k.get("INITIAL-ELEMENT") ?? new LispCharacter(" "));
    return string(ch.toString().repeat(index(a[0])));
  });
  for (const [op, rel, fold] of [
    ["CHAR=", (c) => c === 0, false],
    ["CHAR/=", (c) => c !== 0, false],
    ["CHAR<", (c) => c < 0, false],
    ["CHAR>", (c) => c > 0, false],
    ["CHAR<=", (c) => c <= 0, false],
    ["CHAR>=", (c) => c >= 0, false],
    ["CHAR-EQUAL", (c) => c === 0, true],
    ["CHAR-NOT-EQUAL", (c) => c !== 0, true],
    ["CHAR-LESSP", (c) => c < 0, true],
    ["CHAR-GREATERP", (c) => c > 0, true],
    ["CHAR-NOT-GREATERP", (c) => c <= 0, true],
    ["CHAR-NOT-LESSP", (c) => c >= 0, true],
  ])
    def(op, 1, Infinity, (a) => {
      const v = a.map((x) => {
        const ch = character(x);
        return fold ? ch.toString().toUpperCase().codePointAt(0) : ch.codePoint;
      });
      if (op === "CHAR/=" || op === "CHAR-NOT-EQUAL")
        return truth(new Set(v).size === v.length);
      return truth(v.slice(1).every((x, i) => rel(v[i] - x)));
    });
  unary("CHAR-UPCASE", (x) => {
    const c = character(x),
      v = [...c.toString().toUpperCase()];
    return v.length === 1 ? new LispCharacter(v[0]) : c;
  });
  unary("CHAR-DOWNCASE", (x) => {
    const c = character(x),
      v = [...c.toString().toLowerCase()];
    return v.length === 1 ? new LispCharacter(v[0]) : c;
  });
  pred("ALPHA-CHAR-P", (x) => /^\p{L}$/u.test(character(x).toString()));
  pred("ALPHANUMERICP", (x) => /^[\p{L}\p{N}]$/u.test(character(x).toString()));
  pred("UPPER-CASE-P", (x) => {
    const c = character(x).toString();
    return c === c.toUpperCase() && c !== c.toLowerCase();
  });
  pred("LOWER-CASE-P", (x) => {
    const c = character(x).toString();
    return c === c.toLowerCase() && c !== c.toUpperCase();
  });
  pred("BOTH-CASE-P", (x) => {
    const c = character(x).toString();
    return c.toLowerCase() !== c.toUpperCase();
  });
  pred("STANDARD-CHAR-P", (x) => {
    const n = character(x).codePoint;
    return n === 10 || (n >= 32 && n <= 126);
  });
  pred("GRAPHIC-CHAR-P", (x) => {
    const n = character(x).codePoint;
    return n >= 32 && n !== 127;
  });
  def("DIGIT-CHAR", 1, 2, (a) => {
    const radix = index(a[1] ?? 10),
      weight = index(a[0]);
    if (radix < 2 || radix > 36) typeError(a[1], "radix 2..36");
    return weight < radix
      ? new LispCharacter(weight.toString(radix).toUpperCase())
      : NIL;
  });
  def("DIGIT-CHAR-P", 1, 2, (a) => {
    const radix = index(a[1] ?? 10),
      s = character(a[0]).toString().toUpperCase();
    if (radix < 2 || radix > 36) typeError(a[1], "radix 2..36");
    const n = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".indexOf(s);
    return n >= 0 && n < radix ? n : NIL;
  });
  unary("CHAR-NAME", (x) => {
    const spelling = O.characterName(character(x).codePoint);
    return spelling === undefined ? NIL : string(spelling);
  });
  unary("NAME-CHAR", (x) => {
    const code = O.namedCharacterCode(name(x));
    return code === undefined ? NIL : new LispCharacter(code);
  });
  for (const [op, relation, fold] of [
    ["STRING=", 0, false],
    ["STRING/=", 1, false],
    ["STRING<", 2, false],
    ["STRING>", 3, false],
    ["STRING<=", 4, false],
    ["STRING>=", 5, false],
    ["STRING-EQUAL", 0, true],
    ["STRING-NOT-EQUAL", 1, true],
    ["STRING-LESSP", 2, true],
    ["STRING-GREATERP", 3, true],
    ["STRING-NOT-GREATERP", 4, true],
    ["STRING-NOT-LESSP", 5, true],
  ])
    def(op, 2, Infinity, (a) => {
      const xs = [...name(a[0])],
        ys = [...name(a[1])],
        k = keys(a.slice(2), ["START1", "END1", "START2", "END2"]),
        [s1, e1] = bounds(
          xs.length,
          k.get("START1") ?? 0,
          k.get("END1") ?? NIL,
        ),
        [s2, e2] = bounds(
          ys.length,
          k.get("START2") ?? 0,
          k.get("END2") ?? NIL,
        );
      let i = s1,
        j = s2,
        c = 0;
      while (i < e1 && j < e2) {
        const x = fold ? xs[i].toUpperCase() : xs[i],
          y = fold ? ys[j].toUpperCase() : ys[j];
        c = x < y ? -1 : x > y ? 1 : 0;
        if (c) break;
        i++;
        j++;
      }
      if (!c) c = i < e1 ? 1 : j < e2 ? -1 : 0;
      const yes = [c === 0, c !== 0, c < 0, c > 0, c <= 0, c >= 0][relation];
      return relation === 0 ? truth(yes) : yes ? i : NIL;
    });
  for (const mode of ["UPCASE", "DOWNCASE", "CAPITALIZE"])
    for (const destructive of [false, true])
      def(`${destructive ? "N" : ""}STRING-${mode}`, 1, Infinity, (a) => {
        const original = name(a[0]),
          chars = [...original],
          k = keys(a.slice(1), ["START", "END"]),
          [start, end] = bounds(
            chars.length,
            k.get("START") ?? 0,
            k.get("END") ?? NIL,
          );
        let inWord = false;
        for (let i = start; i < end; i++) {
          const ch = chars[i],
            alnum = /^[\p{L}\p{N}]$/u.test(ch),
            changed =
              mode === "UPCASE"
                ? ch.toUpperCase()
                : mode === "DOWNCASE"
                  ? ch.toLowerCase()
                  : alnum
                    ? inWord
                      ? ch.toLowerCase()
                      : ch.toUpperCase()
                    : ch;
          if ([...changed].length === 1) chars[i] = changed;
          inWord = alnum;
        }
        if (!destructive) return string(chars.join(""));
        if (!(a[0] instanceof LispString)) typeError(a[0], "string");
        chars.forEach((ch, i) => a[0].set(i, new LispCharacter(ch)));
        return a[0];
      });
  for (const n of ["STRING-TRIM", "STRING-LEFT-TRIM", "STRING-RIGHT-TRIM"])
    def(n, 2, 2, (a) => {
      const bag = sequence(a[0]),
        set = new Set(
          Array.from(
            { length: bag.length },
            (_, i) => character(bag.get(i)).codePoint,
          ),
        ),
        chars = [...name(a[1])];
      let start = 0,
        end = chars.length;
      if (n !== "STRING-RIGHT-TRIM")
        while (start < end && set.has(chars[start].codePointAt(0))) start++;
      if (n !== "STRING-LEFT-TRIM")
        while (end > start && set.has(chars[end - 1].codePointAt(0))) end--;
      return string(chars.slice(start, end).join(""));
    });
  def("PARSE-INTEGER", 1, Infinity, (a) => {
    const text = [...str(a[0])],
      k = keys(a.slice(1), ["START", "END", "RADIX", "JUNK-ALLOWED"]),
      [start, end] = bounds(
        text.length,
        k.get("START") ?? 0,
        k.get("END") ?? NIL,
      ),
      radix = index(k.get("RADIX") ?? 10);
    if (radix < 2 || radix > 36) typeError(k.get("RADIX"), "radix 2..36");
    let i = start;
    while (i < end && /\s/u.test(text[i])) i++;
    let sign = 1n;
    if (text[i] === "+" || text[i] === "-") {
      if (text[i] === "-") sign = -1n;
      i++;
    }
    let n = 0n,
      count = 0;
    for (; i < end; i++) {
      const digit = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ".indexOf(
        text[i].toUpperCase(),
      );
      if (digit < 0 || digit >= radix) break;
      n = n * BigInt(radix) + BigInt(digit);
      count++;
    }
    const stopped = i;
    if (k.get("JUNK-ALLOWED") === undefined || k.get("JUNK-ALLOWED") === NIL) {
      while (i < end && /\s/u.test(text[i])) i++;
      if (!count || i !== end) fail("PARSE-ERROR", "String is not an integer");
      return rt.multipleValues(boxInteger(sign * n), i);
    }
    return rt.multipleValues(count ? boxInteger(sign * n) : NIL, stopped);
  });
}
