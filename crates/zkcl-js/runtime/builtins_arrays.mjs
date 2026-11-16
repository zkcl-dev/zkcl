import {
  NIL, T, LispString, LispCharacter, LispArray, list, fromArray, toArray,
} from "./objects.mjs";
import * as N from "./numbers.mjs";
import {
  truth, fail, typeError, string, integer, index, keys, sequence, array,
  arrayObject, rowIndex, arrayGet, arraySet,
} from "./builtins_helpers.mjs";
import { atomicTypep, nameSafe } from "./types.mjs";

export function installArrays(rt, def, unary, pred) {
  def("VECTOR", 0, Infinity, (a) => array(a));
  def("MAKE-ARRAY", 1, Infinity, (a) => {
    const dims = N.isInteger(a[0]) ? [index(a[0])] : toArray(a[0]).map(index),
      k = keys(a.slice(1), [
        "ELEMENT-TYPE",
        "INITIAL-ELEMENT",
        "INITIAL-CONTENTS",
        "FILL-POINTER",
        "ADJUSTABLE",
        "DISPLACED-TO",
        "DISPLACED-INDEX-OFFSET",
      ]);
    if (k.has("INITIAL-ELEMENT") && k.has("INITIAL-CONTENTS"))
      fail("PROGRAM-ERROR", "Cannot supply both initial element and contents");
    const et = k.get("ELEMENT-TYPE") ?? T,
      etn = nameSafe(et);
    let contents;
    if (k.has("INITIAL-CONTENTS")) {
      const flatten = (value, axis) => {
        if (axis === dims.length) return [value];
        const s = sequence(value);
        if (s.length !== dims[axis])
          fail(
            "PROGRAM-ERROR",
            "Initial array contents do not match dimensions",
          );
        return Array.from({ length: s.length }, (_, i) =>
          flatten(s.get(i), axis + 1),
        ).flat();
      };
      contents = flatten(k.get("INITIAL-CONTENTS"), 0);
    }
    if (
      (etn === "CHARACTER" || etn === "BASE-CHAR" || etn === "STANDARD-CHAR") &&
      dims.length === 1 &&
      (k.get("DISPLACED-TO") === undefined || k.get("DISPLACED-TO") === NIL) &&
      (k.get("FILL-POINTER") === undefined || k.get("FILL-POINTER") === NIL) &&
      (k.get("ADJUSTABLE") === undefined || k.get("ADJUSTABLE") === NIL)
    ) {
      const values =
        contents ??
        Array(dims[0]).fill(k.get("INITIAL-ELEMENT") ?? new LispCharacter(0));
      return string(
        values
          .map((v) =>
            v instanceof LispCharacter
              ? v.toString()
              : typeError(v, "character"),
          )
          .join(""),
      );
    }
    const item = k.get("INITIAL-ELEMENT"),
      fp = k.get("FILL-POINTER");
    return new LispArray(dims, {
      elementType: et,
      initialElement: item,
      initialContents: contents,
      adjustable:
        k.get("ADJUSTABLE") !== undefined && k.get("ADJUSTABLE") !== NIL,
      fillPointer:
        fp === undefined || fp === NIL ? null : fp === T ? true : index(fp),
      displacedTo:
        k.get("DISPLACED-TO") === undefined || k.get("DISPLACED-TO") === NIL
          ? null
          : k.get("DISPLACED-TO"),
      displacedIndexOffset: index(k.get("DISPLACED-INDEX-OFFSET") ?? 0),
    });
  });
  for (const n of ["AREF", "BIT", "SBIT"])
    def(n, 1, Infinity, (a) => arrayGet(a[0], rowIndex(a[0], a.slice(1))));
  for (const n of ["AREF", "BIT", "SBIT"])
    def(`(SETF ${n})`, 2, Infinity, (a) => {
      if (
        (n === "BIT" || n === "SBIT") &&
        (!N.isInteger(a[0]) || (integer(a[0]) !== 0n && integer(a[0]) !== 1n))
      )
        typeError(a[0], "bit");
      return arraySet(a[1], rowIndex(a[1], a.slice(2)), a[0]);
    });
  for (const n of ["ROW-MAJOR-AREF", "SVREF"])
    def(n, 2, 2, (a) => arrayGet(a[0], index(a[1])));
  for (const n of ["ROW-MAJOR-AREF", "SVREF"])
    def(`(SETF ${n})`, 3, 3, (a) => arraySet(a[1], index(a[2]), a[0]));
  unary("ARRAY-RANK", (x) =>
    x instanceof LispString ? 1 : arrayObject(x).rank,
  );
  unary("ARRAY-DIMENSIONS", (x) =>
    fromArray(x instanceof LispString ? [x.length] : arrayObject(x).dimensions),
  );
  unary("ARRAY-TOTAL-SIZE", (x) =>
    x instanceof LispString ? x.length : arrayObject(x).totalSize,
  );
  def("ARRAY-DIMENSION", 2, 2, (a) => {
    const dims =
        a[0] instanceof LispString
          ? [a[0].length]
          : arrayObject(a[0]).dimensions,
      i = index(a[1]);
    if (i >= dims.length) typeError(a[1], "array axis");
    return dims[i];
  });
  unary("ARRAY-ELEMENT-TYPE", (x) =>
    x instanceof LispString
      ? rt.intern("CHARACTER", "COMMON-LISP")
      : arrayObject(x).elementType === "OCTET"
        ? list(rt.intern("UNSIGNED-BYTE", "COMMON-LISP"), 8)
        : typeof x.elementType === "string"
          ? rt.intern(x.elementType, "COMMON-LISP")
          : x.elementType,
  );
  def("UPGRADED-ARRAY-ELEMENT-TYPE", 1, 2, (a) => {
    const prototype = new LispArray([0], { elementType: a[0] });
    return prototype.elementType === "OCTET"
      ? list(rt.intern("UNSIGNED-BYTE", "COMMON-LISP"), 8)
      : rt.intern(prototype.elementType, "COMMON-LISP");
  });
  pred("ADJUSTABLE-ARRAY-P", (x) => x instanceof LispArray && x.adjustable);
  pred(
    "ARRAY-HAS-FILL-POINTER-P",
    (x) => x instanceof LispArray && x.fillPointer !== null,
  );
  unary("ARRAY-DISPLACEMENT", (x) => {
    x = arrayObject(x);
    return rt.multipleValues(x.displacedTo ?? NIL, x.displacedIndexOffset);
  });
  unary("FILL-POINTER", (x) => {
    x = arrayObject(x);
    if (x.fillPointer === null) typeError(x, "array with fill pointer");
    return x.fillPointer;
  });
  def("(SETF FILL-POINTER)", 2, 2, (a) => {
    const x = arrayObject(a[1]),
      i = index(a[0]);
    if (x.fillPointer === null) typeError(x, "array with fill pointer");
    if (i > x.dimensions[0]) typeError(a[0], "valid fill pointer");
    x.fillPointer = i;
    return a[0];
  });
  def("VECTOR-PUSH", 2, 2, (a) => {
    const x = arrayObject(a[1]);
    if (x.fillPointer === null) typeError(x, "array with fill pointer");
    if (x.fillPointer === x.dimensions[0]) return NIL;
    const i = x.fillPointer++;
    x.set(i, a[0]);
    return i;
  });
  unary("VECTOR-POP", (x) => {
    x = arrayObject(x);
    if (x.fillPointer === null) typeError(x, "array with fill pointer");
    if (!x.fillPointer) fail("TYPE-ERROR", "Cannot pop an empty vector");
    return x.get(--x.fillPointer);
  });
  def("ARRAY-ROW-MAJOR-INDEX", 1, Infinity, (a) => rowIndex(a[0], a.slice(1)));
  def("ARRAY-IN-BOUNDS-P", 1, Infinity, (a) => {
    const dims =
      a[0] instanceof LispString ? [a[0].length] : arrayObject(a[0]).dimensions;
    if (dims.length !== a.length - 1)
      fail("PROGRAM-ERROR", "Wrong number of array subscripts");
    return truth(
      dims.every((dim, i) => {
        const n = integer(a[i + 1]);
        return n >= 0n && n < BigInt(dim);
      }),
    );
  });
  for (const type of ["SIMPLE-VECTOR", "BIT-VECTOR", "SIMPLE-BIT-VECTOR", "SIMPLE-STRING"])
    pred(`${type}-P`, (x) => atomicTypep(x, rt.intern(type, "COMMON-LISP"), rt));
}
