import {
  NIL, T, Cons, LispSymbol, LispCharacter, LispArray, list, fromArray, toArray, eq, eql,
} from "./objects.mjs";
import { LispClass } from "./clos.mjs";
import {
  fail, typeError, string, integer, index, requestedValue, keys, car, cdr, cells,
  sequence, bounds, sequenceResult, eqlScalarKey, editSequence,
} from "./builtins_helpers.mjs";
import { requestedTypep, atomicTypep, nameSafe } from "./types.mjs";

function* sequenceKey(rt, key, value) {
  return key === NIL ? value : yield* requestedValue(rt, key, [value]);
}

function sequenceFunction(rt, fn) {
  if (fn instanceof LispSymbol || typeof fn === "string") return rt.readFunction(fn);
  const functionType = rt.intern("FUNCTION", "COMMON-LISP");
  if (atomicTypep(fn, functionType, rt)) return fn;
  typeError(fn, list(rt.intern("OR", "COMMON-LISP"), functionType, rt.intern("SYMBOL", "COMMON-LISP")));
}

function sequenceKeyFunction(rt, keywords) {
  const key = keywords.get("KEY") ?? NIL;
  return key === NIL ? NIL : sequenceFunction(rt, key);
}

function sequenceTest(rt, keywords) {
  if (keywords.has("TEST") && keywords.has("TEST-NOT"))
    fail("PROGRAM-ERROR", "Cannot use :TEST and :TEST-NOT together");
  const test = keywords.get("TEST") ?? keywords.get("TEST-NOT");
  return test === undefined ? undefined : sequenceFunction(rt, test);
}

function* sequenceMatches(rt, test, negate, item, value) {
  const matched = test === undefined
    ? eql(item, value)
    : (yield* requestedValue(rt, test, [item, value])) !== NIL;
  return matched !== negate;
}

export function installSequences(def, unary) {
  unary("LENGTH", (x) => sequence(x).length);
  unary("COPY-SEQ", (x) => {
    const s = sequence(x);
    return sequenceResult(
      s.kind,
      Array.from({ length: s.length }, (_, i) => s.get(i)),
      s.elementType,
    );
  });
  def("ELT", 2, 2, (a) => {
    const s = sequence(a[0]),
      i = index(a[1]);
    if (i >= s.length) fail("TYPE-ERROR", "Sequence index out of bounds");
    return s.get(i);
  });
  def("(SETF ELT)", 3, 3, (a) => {
    const s = sequence(a[1]),
      i = index(a[2]);
    if (i >= s.length) fail("TYPE-ERROR", "Sequence index out of bounds");
    s.set(i, a[0]);
    return a[0];
  });
  def("SUBSEQ", 2, 3, (a) => {
    const s = sequence(a[0]),
      [start, end] = bounds(s.length, a[1], a[2] ?? NIL);
    return sequenceResult(
      s.kind,
      Array.from({ length: end - start }, (_, i) => s.get(start + i)),
      s.elementType,
    );
  });
  def("(SETF SUBSEQ)", 3, 4, (a) => {
    const s = sequence(a[1]),
      src = sequence(a[0]),
      [start, end] = bounds(s.length, a[2], a[3] ?? NIL),
      values = Array.from(
        { length: Math.min(end - start, src.length) },
        (_, i) => src.get(i),
      );
    values.forEach((x, i) => s.set(start + i, x));
    return a[0];
  });
  for (const destructive of [false, true])
    unary(destructive ? "NREVERSE" : "REVERSE", (x) => {
      const s = sequence(x),
        values = Array.from({ length: s.length }, (_, i) =>
          s.get(s.length - i - 1),
        );
      if (!destructive) return sequenceResult(s.kind, values, s.elementType);
      if (s.kind === "list") {
        let out = NIL;
        for (const c of s.spine) {
          c.cdr = out;
          out = c;
        }
        return out;
      }
      values.forEach((v, i) => s.set(i, v));
      return x;
    });
  def("FILL", 2, Infinity, (a) => {
    const s = sequence(a[0]),
      k = keys(a.slice(2), ["START", "END"]),
      [start, end] = bounds(s.length, k.get("START") ?? 0, k.get("END") ?? NIL);
    for (let i = start; i < end; i++) s.set(i, a[1]);
    return a[0];
  });
  def("REPLACE", 2, Infinity, (a) => {
    const dst = sequence(a[0]),
      src = sequence(a[1]),
      k = keys(a.slice(2), ["START1", "END1", "START2", "END2"]),
      [s1, e1] = bounds(dst.length, k.get("START1") ?? 0, k.get("END1") ?? NIL),
      [s2, e2] = bounds(src.length, k.get("START2") ?? 0, k.get("END2") ?? NIL),
      v = Array.from({ length: Math.min(e1 - s1, e2 - s2) }, (_, i) =>
        src.get(s2 + i),
      );
    v.forEach((x, i) => dst.set(s1 + i, x));
    return a[0];
  });
  def("MAKE-SEQUENCE", 2, Infinity, function* (a, r) {
    const k = keys(a.slice(2), ["INITIAL-ELEMENT"]);
    const type = yield* sequenceResultType(r, a[0]);
    const length = index(a[1]);
    const initial = k.get("INITIAL-ELEMENT");
    checkSequenceResultLength(type, length);
    if (type.kind === "vector")
      return new LispArray([length], { elementType: type.elementType, initialElement: initial });
    if (type.kind === "string") {
      const value = initial ?? new LispCharacter(" ");
      if (!(value instanceof LispCharacter)) typeError(value, "character");
      return string(value.toString().repeat(length));
    }
    let result = NIL;
    for (let i = 0; i < length; i++) result = new Cons(initial ?? NIL, result);
    return yield* checkSequenceResultElements(r, type, result);
  });
  def("CONCATENATE", 1, Infinity, function* (a, r) {
    const type = yield* sequenceResultType(r, a[0]);
    const v = [];
    for (const x of a.slice(1)) {
      const s = sequence(x);
      for (let i = 0; i < s.length; i++) v.push(s.get(i));
    }
    return yield* buildSequenceResult(r, type, v);
  });
  def("MAP", 3, Infinity, function* (a, r) {
    const fn = sequenceFunction(r, a[1]);
    const seqs = a.slice(2).map(mapInput),
      len = Math.min(...seqs.map((s) => s.length)),
      out = [];
    if (len === Infinity) typeError(a[2], "sequence with finite length");
    const type = a[0] === NIL ? null : yield* sequenceResultType(r, a[0]);
    for (let i = 0; i < len; i++) {
      const value = yield* requestedValue(r, fn, seqs.map((s) => s.get(i)));
      if (type !== null) out.push(value);
    }
    return type === null ? NIL : yield* buildSequenceResult(r, type, out);
  });
  def("MAP-INTO", 2, Infinity, function* (a, r) {
    const fn = sequenceFunction(r, a[1]),
      dst = sequence(a[0]),
      seqs = a.slice(2).map(sequence),
      hasFillPointer = a[0] instanceof LispArray && a[0].fillPointer !== null,
      capacity = hasFillPointer ? a[0].totalSize : dst.length,
      len = Math.min(capacity, ...seqs.map((s) => s.length));
    for (let i = 0; i < len; i++)
      dst.set(
        i,
        yield* requestedValue(
          r,
          fn,
          seqs.map((s) => s.get(i)),
        ),
      );
    if (hasFillPointer) a[0].fillPointer = len;
    return a[0];
  });
  for (const n of ["MAPCAR", "MAPLIST", "MAPC", "MAPL", "MAPCAN", "MAPCON"])
    def(n, 2, Infinity, function* (a, r) {
      const seqs = a.slice(1).map(cells),
        len = Math.min(...seqs.map((s) => s.length)),
        out = [];
      for (let i = 0; i < len; i++)
        out.push(
          yield* requestedValue(
            r,
            a[0],
            seqs.map((s) =>
              n === "MAPLIST" || n === "MAPL" || n === "MAPCON"
                ? s[i]
                : s[i].car,
            ),
          ),
        );
      if (n === "MAPC" || n === "MAPL") return a[1];
      if (n === "MAPCAN" || n === "MAPCON") {
        let result = NIL,
          last;
        for (let i = 0; i < out.length; i++) {
          const item = out[i];
          if (item === NIL) continue;
          if (last) last.cdr = item;
          else result = item;
          if (i < out.length - 1) last = cells(item).at(-1);
        }
        return result;
      }
      return fromArray(out);
    });
  for (const n of ["EVERY", "SOME", "NOTANY", "NOTEVERY"])
    def(n, 2, Infinity, function* (a, r) {
      const fn = sequenceFunction(r, a[0]),
        seqs = a.slice(1).map(sequence),
        len = Math.min(...seqs.map((s) => s.length));
      for (let i = 0; i < len; i++) {
        const value = yield* requestedValue(
          r,
          fn,
          seqs.map((s) => s.get(i)),
        );
        if (n === "SOME" && value !== NIL) return value;
        if (n === "NOTANY" && value !== NIL) return NIL;
        if (n === "EVERY" && value === NIL) return NIL;
        if (n === "NOTEVERY" && value === NIL) return T;
      }
      return n === "EVERY" || n === "NOTANY" ? T : NIL;
    });
  for (const n of ["SEARCH", "MISMATCH"])
    def(n, 2, Infinity, function* (a, r) {
      const k = keys(a.slice(2), ["START1", "END1", "START2", "END2", "FROM-END", "KEY", "TEST", "TEST-NOT"]);
      const test = sequenceTest(r, k);
      const negate = k.has("TEST-NOT");
      const key = sequenceKeyFunction(r, k);
      const fromEnd = (k.get("FROM-END") ?? NIL) !== NIL;
      const seq1 = sequence(a[0]);
      const seq2 = sequence(a[1]);
      const [start1, end1] = bounds(seq1.length, k.get("START1") ?? 0, k.get("END1") ?? NIL);
      const [start2, end2] = bounds(seq2.length, k.get("START2") ?? 0, k.get("END2") ?? NIL);
      const left = Array.from({ length: end1 - start1 }, (_, i) => seq1.get(start1 + i));
      const right = Array.from({ length: end2 - start2 }, (_, i) => seq2.get(start2 + i));
      if (key !== NIL)
        for (const values of [left, right])
          for (let i = 0; i < values.length; i++)
            values[i] = yield* sequenceKey(r, key, values[i]);
      if (n === "MISMATCH") {
        const limit = Math.min(left.length, right.length);
        for (let i = 0; i < limit; i++) {
          const first = left[fromEnd ? left.length - 1 - i : i];
          const second = right[fromEnd ? right.length - 1 - i : i];
          if (!(yield* sequenceMatches(r, test, negate, first, second)))
            return fromEnd ? end1 - i : start1 + i;
        }
        return left.length === right.length ? NIL : fromEnd ? end1 - limit : start1 + limit;
      }
      const lastStart = right.length - left.length;
      for (let step = 0; step <= lastStart; step++) {
        const candidate = fromEnd ? lastStart - step : step;
        let matched = 0;
        while (matched < left.length &&
               (yield* sequenceMatches(r, test, negate, left[matched], right[candidate + matched])))
          matched++;
        if (matched === left.length) return start2 + candidate;
      }
      return NIL;
    });
  for (const destructive of [false, true])
    def(destructive ? "DELETE-DUPLICATES" : "REMOVE-DUPLICATES", 1, Infinity, function* (a, r) {
      const k = keys(a.slice(1), ["START", "END", "FROM-END", "TEST", "TEST-NOT", "KEY"]);
      const s = sequence(a[0]);
      const [start, end] = bounds(s.length, k.get("START") ?? 0, k.get("END") ?? NIL);
      const key = sequenceKeyFunction(r, k);
      const test = sequenceTest(r, k);
      const negate = k.has("TEST-NOT");
      const fromEnd = (k.get("FROM-END") ?? NIL) !== NIL;
      const selected = new Set();
      if (key === NIL && test === undefined) {
        const scalars = new Set(), identities = new Set();
        for (let offset = 0; offset < end - start; offset++) {
          const i = fromEnd ? start + offset : end - offset - 1;
          const element = s.get(i);
          const scalar = eqlScalarKey(element);
          const seen = scalar === null ? identities : scalars;
          const value = scalar ?? element;
          if (seen.has(value)) selected.add(i);
          else seen.add(value);
        }
        return editSequence(a[0], s, selected, destructive);
      }
      for (let i = start; i < end; i++) {
        const begin = fromEnd ? start : i + 1;
        const finish = fromEnd ? i : end;
        if (begin === finish) continue;
        const value = yield* sequenceKey(r, key, s.get(i));
        for (let j = begin; j < finish; j++) {
          if (fromEnd && selected.has(j)) continue;
          const peer = yield* sequenceKey(r, key, s.get(j));
          if (yield* sequenceMatches(r, test, negate, value, peer)) {
            selected.add(i);
            break;
          }
        }
      }
      return editSequence(a[0], s, selected, destructive);
    });
  for (const n of [
    "FIND",
    "POSITION",
    "COUNT",
    "REMOVE",
    "DELETE",
    "REMOVE-IF",
    "REMOVE-IF-NOT",
    "DELETE-IF",
    "DELETE-IF-NOT",
    "SUBSTITUTE",
    "SUBSTITUTE-IF",
    "SUBSTITUTE-IF-NOT",
    "NSUBSTITUTE",
    "NSUBSTITUTE-IF",
    "NSUBSTITUTE-IF-NOT",
    "FIND-IF",
    "FIND-IF-NOT",
    "POSITION-IF",
    "POSITION-IF-NOT",
    "COUNT-IF",
    "COUNT-IF-NOT",
  ]) {
    const substitutes = n.includes("SUBSTITUTE");
    const destructive = n.startsWith("DELETE") || n.startsWith("NSUBSTITUTE");
    const predicate = n.includes("-IF");
    const find = n.startsWith("FIND");
    const position = n.startsWith("POSITION");
    const count = n.startsWith("COUNT");
    const edits = !find && !position && !count;
    const required = substitutes ? 3 : 2;
    const accepted = ["START", "END", "FROM-END", "KEY"];
    if (!predicate) accepted.push("TEST", "TEST-NOT");
    if (edits) accepted.push("COUNT");
    def(n, required, Infinity, function* (a, r) {
      const k = keys(a.slice(required), accepted);
      const original = a[required - 1];
      const s = sequence(original);
      const [start, end] = bounds(s.length, k.get("START") ?? 0, k.get("END") ?? NIL);
      const key = sequenceKeyFunction(r, k);
      const test = sequenceTest(r, k);
      const target = predicate ? sequenceFunction(r, a[required - 2]) : a[required - 2];
      const negate = predicate ? n.endsWith("-NOT") : k.has("TEST-NOT");
      const fromEnd = (k.get("FROM-END") ?? NIL) !== NIL;
      const limit = edits && (k.get("COUNT") ?? NIL) !== NIL
        ? Math.max(0, Number(integer(k.get("COUNT"))))
        : Infinity;
      const selected = new Set();
      let hits = 0;
      for (let step = 0; step < end - start && hits < limit; step++) {
        const i = fromEnd ? end - step - 1 : start + step;
        const element = s.get(i);
        const value = yield* sequenceKey(r, key, element);
        const matched = predicate
          ? ((yield* requestedValue(r, target, [value])) !== NIL) !== negate
          : yield* sequenceMatches(r, test, negate, target, value);
        if (!matched) continue;
        if (find) return element;
        if (position) return i;
        hits++;
        if (edits) selected.add(i);
      }
      if (find || position) return NIL;
      if (count) return hits;
      return editSequence(original, s, selected, destructive, substitutes ? a[0] : undefined);
    });
  }
  for (const n of [
    "MEMBER",
    "MEMBER-IF",
    "MEMBER-IF-NOT",
    "ASSOC",
    "ASSOC-IF",
    "ASSOC-IF-NOT",
    "RASSOC",
    "RASSOC-IF",
    "RASSOC-IF-NOT",
  ]) {
    const member = n.startsWith("MEMBER");
    const predicate = n.includes("-IF");
    const associativeValue = n.startsWith("RASSOC") ? cdr : car;
    def(n, 2, Infinity, function* (a, r) {
      const k = keys(a.slice(2), predicate ? ["KEY"] : ["TEST", "TEST-NOT", "KEY"]);
      const key = sequenceKeyFunction(r, k);
      const test = sequenceTest(r, k);
      const target = predicate ? sequenceFunction(r, a[0]) : a[0];
      const negate = predicate ? n.endsWith("-NOT") : k.has("TEST-NOT");
      for (const c of cells(a[1])) {
        if (!member && c.car === NIL) continue;
        const value = yield* sequenceKey(r, key, member ? c.car : associativeValue(c.car));
        const matched = predicate
          ? ((yield* requestedValue(r, target, [value])) !== NIL) !== negate
          : yield* sequenceMatches(r, test, negate, target, value);
        if (matched) return member ? c : c.car;
      }
      return NIL;
    });
  }
  for (const n of [
    "ADJOIN", "UNION", "NUNION", "INTERSECTION", "NINTERSECTION",
    "SET-DIFFERENCE", "NSET-DIFFERENCE", "SET-EXCLUSIVE-OR", "NSET-EXCLUSIVE-OR",
    "SUBSETP",
  ]) {
    const adjoin = n === "ADJOIN";
    const union = n.endsWith("UNION");
    const intersection = n.endsWith("INTERSECTION");
    const exclusive = n.endsWith("SET-EXCLUSIVE-OR");
    const subsetp = n === "SUBSETP";
    def(n, 2, Infinity, function* (a, r) {
      const k = keys(a.slice(2), ["KEY", "TEST", "TEST-NOT"]);
      const key = sequenceKeyFunction(r, k);
      const test = sequenceTest(r, k);
      const negate = k.has("TEST-NOT");
      const left = adjoin ? [a[0]] : cells(a[0]).map((cell) => cell.car);
      const right = cells(a[1]).map((cell) => cell.car);
      const result = union ? [...right] : [];
      const retainedKeys = [];
      const scan = function* (candidates, value, reverse = false, keyed = false) {
        for (const element of candidates) {
          const peer = keyed ? element : yield* sequenceKey(r, key, element);
          if (yield* sequenceMatches(r, test, negate,
            reverse ? peer : value, reverse ? value : peer)) return true;
        }
        return false;
      };
      for (const element of left) {
        const value = yield* sequenceKey(r, key, element);
        const matched = yield* scan(right, value);
        if (adjoin) return matched ? a[1] : new Cons(element, a[1]);
        if (subsetp && !matched) return NIL;
        if (subsetp || matched !== intersection) continue;
        if (intersection && (yield* scan(result, value))) continue;
        if (union) {
          if (yield* scan(retainedKeys, value, true, true)) continue;
          retainedKeys.push(value);
        }
        result.push(element);
      }
      if (exclusive)
        for (const element of right) {
          const value = yield* sequenceKey(r, key, element);
          if (!(yield* scan(left, value, true))) result.push(element);
        }
      return subsetp ? T : fromArray(result);
    });
  }
  def("REDUCE", 2, Infinity, function* (a, r) {
    const s = sequence(a[1]),
      k = keys(a.slice(2), [
        "START",
        "END",
        "FROM-END",
        "KEY",
        "INITIAL-VALUE",
      ]),
      [start, end] = bounds(s.length, k.get("START") ?? 0, k.get("END") ?? NIL),
      reverse = (k.get("FROM-END") ?? NIL) !== NIL;
    const key = sequenceKeyFunction(r, k);
    const fn = sequenceFunction(r, a[0]);
    const get = (step) => s.get(reverse ? end - step - 1 : start + step);
    let step = 0;
    let acc = k.get("INITIAL-VALUE");
    if (acc === undefined) {
      if (start === end) return yield* requestedValue(r, fn, []);
      acc = yield* sequenceKey(r, key, get(step++));
    }
    for (; step < end - start; step++) {
      const value = yield* sequenceKey(r, key, get(step));
      acc = yield* requestedValue(r, fn, reverse ? [value, acc] : [acc, value]);
    }
    return acc;
  });
  def("MERGE", 4, Infinity, function* (a, r) {
    const k = keys(a.slice(4), ["KEY"]);
    const type = yield* sequenceResultType(r, a[0]);
    const first = sequence(a[1]);
    const second = sequence(a[2]);
    const key = sequenceKeyFunction(r, k);
    const predicate = sequenceFunction(r, a[3]);
    const left = Array.from({ length: first.length }, (_, i) => first.get(i));
    const right = Array.from({ length: second.length }, (_, i) => second.get(i));
    checkSequenceResultLength(type, left.length + right.length);
    let leftIndex = 0, rightIndex = 0;
    const result = [];
    if (left.length !== 0 && right.length !== 0) {
      const leftKeys = [], rightKeys = [];
      for (const value of left) leftKeys.push(yield* sequenceKey(r, key, value));
      for (const value of right) rightKeys.push(yield* sequenceKey(r, key, value));
      while (leftIndex < left.length && rightIndex < right.length) {
        const before = yield* requestedValue(r, predicate, [rightKeys[rightIndex], leftKeys[leftIndex]]);
        if (before !== NIL) result.push(right[rightIndex++]);
        else result.push(left[leftIndex++]);
      }
    }
    return yield* buildSequenceResult(r, type, result.concat(left.slice(leftIndex), right.slice(rightIndex)));
  });
  for (const n of ["SORT", "STABLE-SORT"])
    def(n, 2, Infinity, function* (a, r) {
      const s = sequence(a[0]),
        k = keys(a.slice(2), ["KEY"]),
        key = sequenceKeyFunction(r, k),
        predicate = sequenceFunction(r, a[1]),
        entries = [];
      if (s.length <= 1) return a[0];
      for (let i = 0; i < s.length; i++) {
        const value = s.get(i);
        entries.push({
          value,
          cell: s.spine?.[i],
          key: yield* sequenceKey(r, key, value),
        });
      }
      let sorted = entries;
      for (let width = 1; width < sorted.length; width *= 2) {
        const merged = [];
        for (let start = 0; start < sorted.length; start += 2 * width) {
          let left = start,
            right = Math.min(start + width, sorted.length);
          const leftEnd = right,
            rightEnd = Math.min(start + 2 * width, sorted.length);
          while (left < leftEnd || right < rightEnd) {
            if (left === leftEnd) merged.push(sorted[right++]);
            else if (right === rightEnd) merged.push(sorted[left++]);
            else if (
              (yield* requestedValue(r, predicate, [
                sorted[right].key,
                sorted[left].key,
              ])) !== NIL
            )
              merged.push(sorted[right++]);
            else merged.push(sorted[left++]);
          }
        }
        sorted = merged;
      }
      if (s.spine !== undefined) {
        let tail = NIL;
        for (let i = sorted.length - 1; i >= 0; i--) {
          const { cell } = sorted[i];
          cell.cdr = tail;
          tail = cell;
        }
        return tail;
      }
      sorted.forEach((entry, i) => s.set(i, entry.value));
      return a[0];
    });
}
function mapInput(value) {
  if (!(value instanceof Cons)) return sequence(value);
  const seen = new Set();
  let end = value;
  while (end instanceof Cons && !seen.has(end)) {
    seen.add(end);
    end = end.cdr;
  }
  if (end !== NIL && !(end instanceof Cons)) typeError(end, "proper list");
  let cursor = value;
  return {
    length: end === NIL ? seen.size : Infinity,
    get() {
      if (!(cursor instanceof Cons)) typeError(cursor, "cons");
      const element = cursor.car;
      cursor = cursor.cdr;
      return element;
    },
  };
}

function* sequenceResultType(rt, spec) {
  const invalid = () => fail("TYPE-ERROR", "Expected a recognizable list or vector type", {
    datum: spec,
    expectedType: NIL,
  });
  const expand = function* (type) {
    if (rt.expandTypeSteps) [type] = yield* rt.expandTypeSteps(type);
    return type instanceof LispClass ? type.name : type;
  };
  const lengths = [];
  let kind, elementType, checkedList = false;
  function add(nextKind, nextType, min = 0, max = Infinity) {
    if (kind !== undefined && (kind !== nextKind || elementType !== nextType)) invalid();
    kind = nextKind;
    elementType = nextType;
    lengths.push([min, max]);
  }
  function* collect(type, depth = 0) {
    if (depth === 256) invalid();
    type = yield* expand(type);
    const [head, ...args] = type instanceof Cons ? toArray(type) : [type];
    const op = nameSafe(head);
    if (op === "OR") {
      for (const member of args) yield* collect(member, depth + 1);
      return;
    }
    if (op === "LIST" || op === "NULL") {
      if (type instanceof Cons) invalid();
      add("list", undefined, 0, op === "NULL" ? 0 : Infinity);
      return;
    }
    if (op === "CONS") {
      let minimum = 1;
      let cdrType = args[1] ?? T;
      if (args.length > 2) invalid();
      checkedList ||= args.length !== 0;
      for (;;) {
        cdrType = yield* expand(cdrType);
        const [tail, , next, ...extra] = cdrType instanceof Cons ? toArray(cdrType) : [cdrType];
        const tailName = nameSafe(tail);
        if (tailName === "NULL") return add("list", undefined, minimum, minimum);
        if (["T", "*", "LIST"].includes(tailName)) return add("list", undefined, minimum);
        if (tailName !== "CONS" || extra.length !== 0 || minimum >= 256) invalid();
        minimum++;
        cdrType = next ?? T;
      }
    }
    let vectorType = "T";
    let size;
    if (["STRING", "SIMPLE-STRING", "BASE-STRING", "SIMPLE-BASE-STRING"].includes(op)) {
      if (args.length > 1) invalid();
      vectorType = "CHARACTER";
      [size] = args;
    } else if (["BIT-VECTOR", "SIMPLE-BIT-VECTOR", "SIMPLE-VECTOR"].includes(op)) {
      if (args.length > 1) invalid();
      if (op !== "SIMPLE-VECTOR") vectorType = "BIT";
      [size] = args;
    } else if (["ARRAY", "SIMPLE-ARRAY", "VECTOR"].includes(op)) {
      if (args.length > 2) invalid();
      const [element, shape] = args;
      if (element !== undefined && nameSafe(element) !== "*") {
        vectorType = new LispArray([0], { elementType: yield* expand(element) }).elementType;
      }
      if (op === "VECTOR") size = shape;
      else if (shape instanceof Cons) {
        if (shape.cdr !== NIL) invalid();
        size = shape.car;
      } else if (!eql(shape, 1)) invalid();
    } else invalid();
    const fixed = size === undefined || nameSafe(size) === "*" ? null : index(size);
    add(vectorType === "CHARACTER" ? "string" : "vector", vectorType, fixed ?? 0, fixed ?? Infinity);
  }
  yield* collect(spec);
  if (kind === undefined) invalid();
  return { spec, kind, elementType, lengths, checkedList };
}

function checkSequenceResultLength(type, length) {
  if (!type.lengths.some(([min, max]) => length >= min && length <= max))
    fail("TYPE-ERROR", `Length ${length} does not fit the requested sequence type`, {
      datum: length,
      expectedType: type.spec,
    });
}

function* checkSequenceResultElements(rt, type, result) {
  if (type.checkedList && !(yield* requestedTypep(result, type.spec, rt)))
    typeError(result, type.spec);
  return result;
}

function* buildSequenceResult(rt, type, values) {
  checkSequenceResultLength(type, values.length);
  return yield* checkSequenceResultElements(
    rt, type, sequenceResult(type.kind, values, type.elementType),
  );
}

export function installTreeOperations(def) {
  def("TREE-EQUAL", 2, Infinity, function* (a, r) {
    const k = keys(a.slice(2), ["TEST", "TEST-NOT"]);
    const test = sequenceTest(r, k);
    const negate = k.has("TEST-NOT");
    const pending = [[a[0], a[1]]];
    while (pending.length !== 0) {
      const [left, right] = pending.pop();
      const leftCons = left instanceof Cons;
      const rightCons = right instanceof Cons;
      if (leftCons !== rightCons) return NIL;
      if (leftCons) pending.push([left.cdr, right.cdr], [left.car, right.car]);
      else if (!(yield* sequenceMatches(r, test, negate, left, right))) return NIL;
    }
    return T;
  });
  for (const n of ["SUBST", "NSUBST", "SUBST-IF", "NSUBST-IF", "SUBST-IF-NOT", "NSUBST-IF-NOT", "SUBLIS", "NSUBLIS"]) {
    const alist = n.endsWith("SUBLIS");
    const destructive = n.startsWith("N");
    const predicate = n.includes("-IF");
    const required = alist ? 2 : 3;
    def(n, required, Infinity, function* (a, r) {
      const k = keys(a.slice(required), predicate ? ["KEY"] : ["KEY", "TEST", "TEST-NOT"]);
      const key = sequenceKeyFunction(r, k);
      const test = sequenceTest(r, k);
      const target = predicate ? sequenceFunction(r, a[1]) : a[1];
      const negate = predicate ? n.endsWith("-NOT") : k.has("TEST-NOT");
      const entries = alist
        ? cells(a[0]).filter((cell) => cell.car !== NIL).map(({ car: entry }) => [car(entry), cdr(entry)])
        : null;
      const replacement = function* (node) {
        const value = yield* sequenceKey(r, key, node);
        if (entries !== null) {
          for (const [item, result] of entries)
            if (yield* sequenceMatches(r, test, negate, value, item)) return result;
        } else {
          const matched = predicate
            ? ((yield* requestedValue(r, target, [value])) !== NIL) !== negate
            : yield* sequenceMatches(r, test, negate, target, value);
          if (matched) return a[0];
        }
      };
      const stack = [];
      let node = a[required - 1];
      for (;;) {
        let value = yield* replacement(node);
        if (value === undefined && node instanceof Cons) {
          stack.push({ node, car: undefined });
          node = node.car;
          continue;
        }
        value ??= node;
        while (stack.length !== 0 && stack.at(-1).car !== undefined) {
          const frame = stack.pop();
          const pair = frame.node;
          if (destructive) {
            pair.car = frame.car;
            pair.cdr = value;
            value = pair;
          } else {
            value = eq(pair.car, frame.car) && eq(pair.cdr, value) ? pair : new Cons(frame.car, value);
          }
        }
        if (stack.length === 0) return value;
        const frame = stack.at(-1);
        frame.car = value;
        node = frame.node.cdr;
      }
    });
  }
}
