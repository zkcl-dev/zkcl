import {
  NIL, T, UNBOUND, Cons, LispSymbol, LispString, LispCharacter, list, fromArray, toArray, eq,
} from "./objects.mjs";
import {
  truth, fail, typeError, str, string, name, sym, index, keys, cells,
} from "./builtins_helpers.mjs";
import { atomicTypep, nameSafe } from "./types.mjs";

export function installSymbols(rt, def, unary, pred) {
  const sentinelPlists = new Map([
    [NIL, NIL],
    [T, NIL],
  ]);
  const getPlist = (s) =>
    sentinelPlists.has(s) ? sentinelPlists.get(s) : (s.plist ?? NIL);
  const setPlist = (s, plist) => {
    if (sentinelPlists.has(s)) sentinelPlists.set(s, plist);
    else s.plist = plist;
    return plist;
  };
  unary("SYMBOL-NAME", (x) => string(sym(x).name));
  unary("SYMBOL-PACKAGE", (x) => {
    const p = sym(x).package;
    return p == null
      ? NIL
      : typeof p === "string"
        ? (rt.findPackage(p) ?? NIL)
        : p;
  });
  def("SYMBOL-VALUE", 1, 1, (a, r, t) => r.getSymbolValue(a[0], t));
  def("SET", 2, 2, (a, r, t) => {
    r.setSymbolValue(a[0], a[1], t);
    return a[1];
  });
  def("BOUNDP", 1, 1, (a, r, t) => {
    const x = sym(a[0]);
    const dynamic = t?.dynamic?.get(x);
    const value = r.variableValue
      ? r.variableValue(x, t)
      : dynamic === undefined
        ? x.value
        : Array.isArray(dynamic)
          ? (dynamic.at(-1) ?? x.value)
          : dynamic;
    return truth(value !== UNBOUND);
  });
  def("MAKUNBOUND", 1, 1, (a, r, t) => {
    const x = sym(a[0]);
    r.setSymbolValue(x, UNBOUND, t);
    return x;
  });
  unary("MAKE-SYMBOL", (x) => new LispSymbol(str(x)));
  def("COPY-SYMBOL", 1, 2, (a) => {
    const src = sym(a[0]),
      s = new LispSymbol(src.name);
    if (a[1] !== undefined && a[1] !== NIL) {
      s.value = src.value;
      s.function = src.function;
      s.plist = fromArray(toArray(getPlist(src)));
    }
    return s;
  });
  let gensymCounter = 0;
  def("GENSYM", 0, 1, (a) => {
    let prefix = "G",
      n;
    if (a[0] === undefined) n = gensymCounter++;
    else if (a[0] instanceof LispString) {
      prefix = str(a[0]);
      n = gensymCounter++;
    } else n = index(a[0]);
    return new LispSymbol(`${prefix}${n}`);
  });
  def("GENTEMP", 0, 2, (a) => {
    const prefix = a[0] === undefined ? "T" : str(a[0]),
      pkg = a[1] ?? rt.currentPackage;
    let spelling;
    do {
      spelling = `${prefix}${gensymCounter++}`;
    } while (rt.findSymbol(spelling, pkg));
    return rt.intern(spelling, pkg);
  });
  const functionName = (x) =>
    x instanceof Cons ? `(SETF ${name(toArray(x)[1])})` : name(sym(x));
  def("FBOUNDP", 1, 1, (a) => {
    try {
      return truth(rt.readFunction(a[0]) !== UNBOUND);
    } catch (e) {
      if (e.className === "UNDEFINED-FUNCTION") return NIL;
      throw e;
    }
  });
  for (const n of ["SYMBOL-FUNCTION", "FDEFINITION"])
    def(n, 1, 1, (a) => rt.readFunction(a[0]));
  for (const n of ["SYMBOL-FUNCTION", "FDEFINITION"])
    def(`(SETF ${n})`, 2, 2, (a) => {
      if (!atomicTypep(a[0], rt.intern("FUNCTION", "COMMON-LISP"), rt))
        typeError(a[0], "function");
      rt.defineFunction(a[1], a[0]);
      return a[0];
    });
  unary("FMAKUNBOUND", (x) => {
    if (x instanceof LispSymbol) {
      x.function = UNBOUND;
      return x;
    }
    rt.defineFunction(x, UNBOUND);
    return x;
  });
  def("CONSTANTP", 1, 2, (a) => {
    const v = a[0];
    return truth(
      (!(v instanceof Cons) &&
        (!(v instanceof LispSymbol) ||
          v.constant ||
          v.package?.name === "KEYWORD" ||
          v.package === "KEYWORD")) ||
        (v instanceof Cons && nameSafe(v.car) === "QUOTE"),
    );
  });
  def("PROCLAIM", 1, 1, (a) => {
    const spec = toArray(a[0]),
      n = name(spec[0]);
    if (n === "SPECIAL") for (const s of spec.slice(1)) sym(s).special = true;
    else if (
      ![
        "TYPE",
        "FTYPE",
        "INLINE",
        "NOTINLINE",
        "OPTIMIZE",
        "DECLARATION",
        "DYNAMIC-EXTENT",
        "IGNORE",
        "IGNORABLE",
      ].includes(n)
    )
      fail("PROGRAM-ERROR", `Unsupported proclamation ${n}`);
    return NIL;
  });
  unary("SYMBOL-PLIST", (x) => getPlist(sym(x)));
  def("(SETF SYMBOL-PLIST)", 2, 2, (a) => {
    cells(a[0]);
    if (toArray(a[0]).length % 2)
      fail("TYPE-ERROR", "Property list must contain pairs");
    setPlist(sym(a[1]), a[0]);
    return a[0];
  });
  const getf = (plist, indicator, fallback = NIL) => {
    const c = cells(plist);
    if (c.length % 2) fail("TYPE-ERROR", "Malformed property list");
    for (let i = 0; i < c.length; i += 2)
      if (eq(c[i].car, indicator)) return c[i + 1].car;
    return fallback;
  };
  def("GETF", 2, 3, (a) => getf(a[0], a[1], a[2] ?? NIL));
  def("GET", 2, 3, (a) => getf(getPlist(sym(a[0])), a[1], a[2] ?? NIL));
  def("(SETF GET)", 3, 4, (a) => {
    const s = sym(a[1]),
      cs = cells(getPlist(s));
    for (let i = 0; i < cs.length; i += 2)
      if (eq(cs[i].car, a[2])) {
        cs[i + 1].car = a[0];
        return a[0];
      }
    setPlist(s, new Cons(a[2], new Cons(a[0], getPlist(s))));
    return a[0];
  });
  def("%PUTF", 3, 3, (a) => {
    const cs = cells(a[0]);
    for (let i = 0; i < cs.length; i += 2)
      if (eq(cs[i].car, a[1])) {
        cs[i + 1].car = a[2];
        return a[0];
      }
    return new Cons(a[1], new Cons(a[2], a[0]));
  });
  def("REMPROP", 2, 2, (a) => {
    const s = sym(a[0]),
      cs = cells(getPlist(s));
    for (let i = 0; i < cs.length; i += 2)
      if (eq(cs[i].car, a[1])) {
        if (i === 0) setPlist(s, cs[i + 1].cdr);
        else cs[i - 1].cdr = cs[i + 1].cdr;
        return T;
      }
    return NIL;
  });
  const packageObject = (x) => {
    const p = rt.findPackage(
      x instanceof LispSymbol ||
        x instanceof LispString ||
        x instanceof LispCharacter
        ? name(x)
        : x,
    );
    if (!p)
      fail(
        "PACKAGE-ERROR",
        `Package ${typeof x === "object" ? (nameSafe(x) ?? "?") : x} does not exist`,
        { package: x },
      );
    return p;
  };
  def(
    "FIND-PACKAGE",
    1,
    1,
    (a) =>
      rt.findPackage(
        a[0] instanceof LispSymbol ||
          a[0] instanceof LispString ||
          a[0] instanceof LispCharacter
          ? name(a[0])
          : a[0],
      ) ?? NIL,
  );
  def("MAKE-PACKAGE", 1, Infinity, (a) => {
    const k = keys(a.slice(1), ["NICKNAMES", "USE"]);
    return rt.makePackage(name(a[0]), {
      nicknames: k.has("NICKNAMES")
        ? toArray(k.get("NICKNAMES")).map(name)
        : [],
      use: k.has("USE") ? toArray(k.get("USE")).map(packageObject) : [],
    });
  });
  unary("PACKAGE-NAME", (x) => {
    const p = packageObject(x);
    return p.deleted ? NIL : string(p.name);
  });
  unary("PACKAGE-NICKNAMES", (x) =>
    fromArray([...packageObject(x).nicknames].map(string)),
  );
  unary("PACKAGE-USE-LIST", (x) => fromArray(packageObject(x).uses));
  unary("PACKAGE-USED-BY-LIST", (x) => {
    const p = packageObject(x);
    return fromArray(
      [...new Set(rt.packages.values())].filter((q) => q.uses.includes(p)),
    );
  });
  unary("PACKAGE-SHADOWING-SYMBOLS", (x) =>
    fromArray([...(packageObject(x).shadowing ?? [])]),
  );
  def("LIST-ALL-PACKAGES", 0, 0, () =>
    fromArray([...new Set(rt.packages.values())].filter((p) => !p.deleted)),
  );
  const packageEntries = def("%%PACKAGE-ITERATOR-ENTRIES", 2, 2, ([designator, mode]) => {
    const symbols = [];
    if (mode & 8) {
      for (const pkg of new Set(rt.packages.values()))
        for (const symbol of pkg.symbols.values()) symbols.push(symbol);
      return fromArray(symbols);
    }
    const justSymbols = (mode & 16) !== 0;
    const packages = justSymbols ? [designator] : designator === NIL || designator instanceof Cons ? toArray(designator) : [designator];
    const entry = (symbol, status, pkg) => {
      symbols.push(symbol);
      if (!justSymbols) symbols.push(rt.intern(status, "KEYWORD"), pkg);
    };
    for (const value of packages) {
      const pkg = packageObject(value);
      if (pkg.deleted) fail("PACKAGE-ERROR", "Cannot iterate over a deleted package", { package: value });
      for (const [name, symbol] of pkg.symbols) {
        const external = pkg.exports.has(name);
        if (mode & (external ? 2 : 1)) entry(symbol, external ? "EXTERNAL" : "INTERNAL", pkg);
      }
      if (mode & 4) {
        const seen = new Set();
        for (const used of pkg.uses) for (const name of used.exports)
          if (!pkg.symbols.has(name) && !seen.has(name)) {
            seen.add(name); entry(used.symbols.get(name), "INHERITED", pkg);
          }
      }
    }
    return fromArray(symbols);
  });
  rt.findPackage("COMMON-LISP").exports.delete("%%PACKAGE-ITERATOR-ENTRIES");
  rt.installers?.set(18, (args, runtime, task) => packageEntries.invoke(args, runtime, task));
  const globalPackage = (x) => {
    const p = x?.symbols instanceof Map ? x : rt.packages.get(name(x));
    if (!p || p.deleted)
      fail("PACKAGE-ERROR", "No live global package", { package: x });
    return p;
  };
  unary("ZK::%GLOBAL-PACKAGE", globalPackage);
  unary("ZK:PACKAGE-LOCAL-NICKNAMES", (x) =>
    fromArray(
      [...(packageObject(x).localNicknames ?? new Map())].map(
        ([nickname, pkg]) => new Cons(string(nickname), pkg),
      ),
    ),
  );
  unary("ZK:PACKAGE-LOCALLY-NICKNAMED-BY-LIST", (x) => {
    const target = packageObject(x);
    return fromArray(
      [...new Set(rt.packages.values())].filter((pkg) =>
        [...(pkg.localNicknames?.values() ?? [])].includes(target),
      ),
    );
  });
  def("ZK::%REPLACE-LOCAL-NICKNAMES", 2, 2, (a) => {
    const owner = packageObject(a[0]),
      entries = new Map();
    for (const pair of toArray(a[1])) {
      if (!(pair instanceof Cons)) typeError(pair, "cons");
      entries.set(name(pair.car), packageObject(pair.cdr));
    }
    owner.localNicknames = entries;
    return owner;
  });
  def("ZK:REMOVE-PACKAGE-LOCAL-NICKNAME", 1, 2, (a) =>
    truth(
      (
        packageObject(a[1] ?? rt.currentPackage).localNicknames ?? new Map()
      ).delete(name(a[0])),
    ),
  );
  const addLocalNickname = (owner, nickname, target) => {
    owner.localNicknames ??= new Map();
    if (
      ["CL", "COMMON-LISP", "KEYWORD", owner.name, ...owner.nicknames].includes(
        nickname,
      )
    )
      fail("PACKAGE-ERROR", "Reserved local package nickname", {
        package: owner,
      });
    const previous = owner.localNicknames.get(nickname);
    if (previous && previous !== target)
      fail("PACKAGE-ERROR", "Conflicting local package nickname", {
        package: owner,
      });
    owner.localNicknames.set(nickname, target);
    return owner;
  };
  def("ZK:ADD-PACKAGE-LOCAL-NICKNAME", 2, 3, (a) =>
    addLocalNickname(
      packageObject(a[2] ?? rt.currentPackage),
      name(a[0]),
      globalPackage(a[1]),
    ),
  );
  def("ZK::%DEFINE-LOCAL-NICKNAMES", 2, 2, (a) => {
    const owner = packageObject(a[0]),
      entries = new Map();
    for (const spec of toArray(a[1])) {
      const [nickname, target] = toArray(spec);
      const spelling = name(nickname);
      if (entries.has(spelling))
        fail("PACKAGE-ERROR", "Duplicate local package nickname");
      entries.set(spelling, globalPackage(target));
    }
    owner.localNicknames = new Map();
    for (const [nickname, target] of entries)
      addLocalNickname(owner, nickname, target);
    return owner;
  });
  def("INTERN", 1, 2, (a) => {
    const spelling = str(a[0]),
      p = packageObject(a[1] ?? rt.currentPackage),
      existing = rt.findSymbol(spelling, p);
    if (existing) {
      const s = Array.isArray(existing) ? existing[0] : existing;
      const status = p.symbols.has(spelling)
        ? p.exports.has(spelling)
          ? "EXTERNAL"
          : "INTERNAL"
        : "INHERITED";
      return rt.multipleValues(s, rt.intern(status, "KEYWORD"));
    }
    return rt.multipleValues(rt.intern(spelling, p), NIL);
  });
  def("FIND-SYMBOL", 1, 2, (a) => {
    const spelling = str(a[0]),
      p = packageObject(a[1] ?? rt.currentPackage),
      existing = rt.findSymbol(spelling, p);
    if (!existing) return rt.multipleValues(NIL, NIL);
    const status = p.symbols.has(spelling)
      ? p.exports.has(spelling)
        ? "EXTERNAL"
        : "INTERNAL"
      : "INHERITED";
    return rt.multipleValues(
      Array.isArray(existing) ? existing[0] : existing,
      rt.intern(status, "KEYWORD"),
    );
  });
  unary("FIND-ALL-SYMBOLS", (x) => {
    const spelling = name(x),
      found = new Set();
    for (const p of rt.packages.values())
      if (p.symbols.has(spelling)) found.add(p.symbols.get(spelling));
    return fromArray([...found]);
  });
  const many = (x) => (x === NIL ? [] : x instanceof Cons ? toArray(x) : [x]);
  for (const n of ["EXPORT", "UNEXPORT", "IMPORT", "SHADOWING-IMPORT"])
    def(n, 1, 2, (a) => {
      const p = packageObject(a[1] ?? rt.currentPackage);
      for (const item of many(a[0])) {
        const s = sym(item),
          present = p.symbols.get(s.name);
        if (n === "EXPORT") {
          const accessible = rt.findSymbol(s.name, p);
          if (accessible !== s)
            fail("PACKAGE-ERROR", "Exported symbol is not accessible");
          p.symbols.set(s.name, s);
          p.exports.add(s.name);
        } else if (n === "UNEXPORT") p.exports.delete(s.name);
        else {
          if (present && present !== s && n !== "SHADOWING-IMPORT")
            fail("PACKAGE-ERROR", "Import symbol name conflict");
          p.symbols.set(s.name, s);
          if (s.package === null) s.package = p;
          if (n === "SHADOWING-IMPORT") {
            p.shadowing ??= new Set();
            p.shadowing.add(s);
          }
        }
      }
      return T;
    });
  def("SHADOW", 1, 2, (a) => {
    const p = packageObject(a[1] ?? rt.currentPackage);
    p.shadowing ??= new Set();
    for (const item of many(a[0])) {
      const spelling = name(item);
      let s = p.symbols.get(spelling);
      if (!s) {
        s = new LispSymbol(spelling, p);
        p.symbols.set(spelling, s);
      }
      p.shadowing.add(s);
    }
    return T;
  });
  for (const n of ["USE-PACKAGE", "UNUSE-PACKAGE"])
    def(n, 1, 2, (a) => {
      const p = packageObject(a[1] ?? rt.currentPackage);
      for (const item of many(a[0])) {
        const q = packageObject(item);
        if (n === "USE-PACKAGE") {
          for (const spelling of q.exports) {
            const old = rt.findSymbol(spelling, p),
              next = q.symbols.get(spelling);
            if (
              old &&
              old !== next &&
              ![...(p.shadowing ?? [])].some((s) => s.name === spelling)
            )
              fail("PACKAGE-ERROR", "Use-package symbol name conflict");
          }
          if (!p.uses.includes(q)) p.uses.push(q);
        } else p.uses = p.uses.filter((x) => x !== q);
      }
      return T;
    });
  def("UNINTERN", 1, 2, (a) => {
    const s = sym(a[0]),
      p = packageObject(a[1] ?? rt.currentPackage);
    if (p.symbols.get(s.name) !== s) return NIL;
    p.symbols.delete(s.name);
    p.exports.delete(s.name);
    p.shadowing?.delete(s);
    if (s.package === p) s.package = null;
    return T;
  });
  def("RENAME-PACKAGE", 2, 3, (a) => {
    const p = packageObject(a[0]),
      spelling = name(a[1]),
      nicknames = a[2] === undefined ? [] : toArray(a[2]).map(name);
    for (const key of [spelling, ...nicknames])
      if (rt.packages.has(key) && rt.packages.get(key) !== p)
        fail("PACKAGE-ERROR", "Package name already exists");
    for (const [key, q] of rt.packages) if (q === p) rt.packages.delete(key);
    p.name = spelling;
    p.nicknames = new Set(nicknames);
    for (const key of [spelling, ...nicknames]) rt.packages.set(key, p);
    return p;
  });
  def("DELETE-PACKAGE", 1, 1, (a) => {
    const p = rt.findPackage(name(a[0]));
    if (!p) return NIL;
    for (const q of rt.packages.values())
      q.uses = q.uses.filter((x) => x !== p);
    for (const [key, q] of rt.packages) if (q === p) rt.packages.delete(key);
    for (const s of p.symbols.values()) if (s.package === p) s.package = null;
    p.deleted = true;
    p.symbols.clear();
    return T;
  });
  const methodCombinationDoc = (kind) => kind === rt.intern("METHOD-COMBINATION", "COMMON-LISP") ||
    kind === rt.intern("METHOD-COMBINATION", "KEYWORD");
  const compilerMacroDoc = rt.intern("COMPILER-MACRO", "COMMON-LISP");
  const documentationObject = value => atomicTypep(value, rt.intern("FUNCTION", "COMMON-LISP"), rt) ? value : sym(value);
  def("DOCUMENTATION", 2, 2, (a) => {
    if (methodCombinationDoc(a[1])) return rt.methodCombinations.documentation(sym(a[0]));
    if (a[1] === compilerMacroDoc) return rt.compilerMacroDocumentation(a[0]);
    return documentationObject(a[0]).documentation ?? NIL;
  });
  def("(SETF DOCUMENTATION)", 3, 3, (a) => {
    if (methodCombinationDoc(a[2])) return rt.methodCombinations.setDocumentation(sym(a[1]), a[0]);
    if (a[2] === compilerMacroDoc) return rt.setCompilerMacroDocumentation(a[1], a[0]);
    documentationObject(a[1]).documentation = a[0];
    return a[0];
  });
  const modules = [];
  def("PROVIDE", 1, 1, (a) => {
    const spelling = name(a[0]);
    if (!modules.includes(spelling)) modules.push(spelling);
    return T;
  });
}
