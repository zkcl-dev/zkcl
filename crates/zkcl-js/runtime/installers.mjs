import { NIL, toArray } from "./objects.mjs";
import {
  primary, fail, name, sym, index, call, arity,
} from "./builtins_helpers.mjs";

// These are serialized installer IDs, not indices into Rust's native function vector.
// Source of truth: zkcl-runtime/src/macros/standard.rs::INSTALLERS.
export const BUILTIN_NATIVE_NAMES = Object.freeze({
  0: "SWITCH-PACKAGE",
  1: "SET-PACKAGE-DOCUMENTATION",
  2: "DEFINE-SYMBOL-MACRO",
  3: "REGISTER-STRUCTURE",
  4: "MAKE-STRUCTURE",
  5: "STRUCTURE-PREDICATE",
  6: "COPY-STRUCTURE",
  7: "READ-STRUCTURE-SLOT",
  8: "WRITE-STRUCTURE-SLOT",
  9: "DEFINE-GENERIC",
  10: "DEFINE-METHOD",
  11: "REGISTER-CLASS",
  13: "INITIALIZE-SHARED-SLOT",
  14: "DEFINE-TYPE",
  15: "DEFINE-SETF-EXPANDER",
  16: "DEFINE-SHORT-DEFSETF",
  17: "DEFINE-LONG-DEFSETF",
  18: "PACKAGE-ITERATOR-ENTRIES",
  19: "HASH-TABLE-STEP",
  20: "SET-MACRO-FUNCTION",
  21: "DEFINE-CONSTANT",
  22: "DEFINE-PACKAGE",
  24: "PRINT-DEPTH",
  25: "RESIDUAL-MACRO-ERROR",
  26: "INSTALL-TRANSFORM",
  27: "INSTALL-VOP",
});
export function installBuiltinNative(id, args, rt, task) {
  const handler = rt.installers?.get(id);
  if (handler) return handler(args, rt, task);
  if (!(id in BUILTIN_NATIVE_NAMES))
    fail("PROGRAM-ERROR", `Unknown builtin native installer ${id}`);
  if (id === 0) {
    arity(args, 1);
    const pkg = rt.findPackage(name(args[0]));
    if (!pkg) fail("PACKAGE-ERROR", "Package does not exist");
    const variable = rt.intern("*PACKAGE*", "COMMON-LISP");
    rt.setSymbolValue(variable, pkg, task);
    return pkg;
  }
  if (id === 1) {
    arity(args, 2);
    args[0].documentation = args[1];
    return args[1];
  }
  if (id === 22) {
    arity(args, 2);
    const spelling = name(args[0]),
      nicknames = toArray(args[1]).map(name),
      existing = rt.findPackage(spelling);
    for (const nickname of nicknames) {
      const owner = rt.findPackage(nickname);
      if (owner && owner !== existing)
        fail("PACKAGE-ERROR", `Conflicting package nickname ${nickname}`);
    }
    if (!existing) return rt.makePackage(spelling, { nicknames, use: [] });
    return primary(
      call(rt, task, rt.intern("RENAME-PACKAGE", "COMMON-LISP"), [
        existing,
        args[0],
        args[1],
      ]),
    );
  }
  if (id === 15) {
    arity(args, 3);
    rt.defineSetfExpander(args[0], args[1]);
    if (args[2] !== NIL) args[0].setfDocumentation = args[2];
    return args[0];
  }
  if (id === 16) {
    arity(args, 3);
    rt.defineShortDefsetf(args[0], args[1]);
    if (args[2] !== NIL) args[0].setfDocumentation = args[2];
    return args[0];
  }
  if (id === 14) {
    arity(args, 2);
    return rt.defineType(args[0], args[1]);
  }
  if (id === 17) {
    arity(args, 6);
    rt.defineLongDefsetf(
      args[0],
      args[1],
      index(args[2]),
      index(args[3]),
      args[4] !== NIL,
    );
    if (args[5] !== NIL) args[0].setfDocumentation = args[5];
    return args[0];
  }
  if (id === 20) {
    arity(args, 2);
    rt.defineFunction(args[0], args[1], true);
    return args[0];
  }
  if (id === 21) {
    arity(args, 2, 3);
    rt.checkMutableSymbol?.(args[0]);
    sym(args[0]).value = args[1];
    args[0].constant = true;
    return args[0];
  }
  if (id === 25)
    fail("PROGRAM-ERROR", "A macro remained unexpanded in a compiled file");
  if (id === 26 || id === 27) {
    arity(args, 3);
    return NIL;
  }
  fail(
    "UNDEFINED-FUNCTION",
    `JavaScript builtin installer ${BUILTIN_NATIVE_NAMES[id]} is not implemented`,
  );
}
