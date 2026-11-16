import { NIL, T, LispSymbol, LispString, Cons, fromArray } from "./objects.mjs";
import { NativeFunction, LispCondition } from "./execution.mjs";
import { SingleFloat, DoubleFloat } from "./numbers.mjs";
import { parseFormat } from "./format.mjs";

const list = (...xs) => fromArray(xs);
const fresh = (name = "LOOP") => new LispSymbol(name, null);
function invalid(message) {
  throw new LispCondition("PROGRAM-ERROR", message);
}
function array(value) {
  const out = [],
    seen = new Set();
  while (value !== NIL) {
    if (!(value instanceof Cons) || seen.has(value))
      invalid("Expected a proper list");
    seen.add(value);
    out.push(value.car);
    value = value.cdr;
  }
  return out;
}
function variable(x) {
  if (!(x instanceof LispSymbol) || x.constant || x.package?.name === "KEYWORD")
    invalid("Expected a variable name");
  return x;
}
const keyword = (x) => (x instanceof LispSymbol ? x.name : null);
const helpers = new WeakMap();

/** All iteration syntax lowers to the compiler's ordinary lexical control forms. */
class Loop {
  constructor(rt, args) {
    this.rt = rt;
    this.args = args;
    this.pos = 0;
    this.block = NIL;
    this.finish = fresh("LOOP FINISH");
    this.first = fresh("LOOP FIRST");
    this.temporaries = [list(this.first, T)];
    this.prologue = [];
    this.induction = [];
    this.initially = [];
    this.finally = [];
    this.body = [];
    this.variables = new Set();
    this.accumulators = [];
    this.result = null;
    this.boolean = null;
    this.main = false;
    this.appendCopy = null;
  }
  f(name, ...args) {
    return list(this.rt.intern(name, "COMMON-LISP"), ...args);
  }
  peek() {
    return keyword(this.args[this.pos]);
  }
  take() {
    if (this.pos >= this.args.length)
      invalid("LOOP clause requires another form");
    return this.args[this.pos++];
  }
  kw() {
    const k = keyword(this.take());
    if (k === null) invalid("LOOP expects a clause keyword");
    return k;
  }
  consume(k) {
    if (this.peek() !== k) return false;
    this.pos++;
    return true;
  }
  record(v) {
    variable(v);
    if (this.variables.has(v)) invalid("LOOP variable is bound more than once");
    this.variables.add(v);
    return v;
  }
  prologueOnly() {
    if (this.main) invalid("LOOP variable clauses must precede main clauses");
  }
  type() {
    if (
      this.consume("OF-TYPE") ||
      ["FIXNUM", "FLOAT", "T", "NIL"].includes(this.peek())
    )
      return this.take();
    return null;
  }
  zero(type) {
    const name = keyword(type instanceof Cons ? type.car : type);
    return ["DOUBLE-FLOAT", "LONG-FLOAT"].includes(name)
      ? new DoubleFloat(0)
      : ["FLOAT", "SHORT-FLOAT", "SINGLE-FLOAT"].includes(name)
        ? new SingleFloat(0)
        : 0;
  }
  default(pattern, type) {
    if (pattern === NIL) return NIL;
    if (pattern instanceof Cons)
      return this.f(
        "CONS",
        this.default(pattern.car, type instanceof Cons ? type.car : type),
        this.default(pattern.cdr, type instanceof Cons ? type.cdr : type),
      );
    variable(pattern);
    const name = keyword(type instanceof Cons ? type.car : type);
    return [
      "NUMBER",
      "REAL",
      "RATIONAL",
      "INTEGER",
      "FIXNUM",
      "SIGNED-BYTE",
      "UNSIGNED-BYTE",
      "BIT",
      "FLOAT",
      "SHORT-FLOAT",
      "SINGLE-FLOAT",
      "DOUBLE-FLOAT",
      "LONG-FLOAT",
    ].includes(name)
      ? this.zero(type)
      : NIL;
  }
  destructure(pattern, value, pairs = []) {
    if (pattern === NIL) return pairs;
    if (pattern instanceof Cons) {
      this.destructure(pattern.car, this.f("CAR", value), pairs);
      this.destructure(pattern.cdr, this.f("CDR", value), pairs);
    } else pairs.push([this.record(pattern), value]);
    return pairs;
  }
  forms(clause) {
    const forms = [];
    while (this.args[this.pos] instanceof Cons) forms.push(this.take());
    if (!forms.length) invalid(`LOOP ${clause} requires a compound form`);
    return forms;
  }
  leave(test, inverse = false) {
    return this.f(
      "IF",
      test,
      inverse ? NIL : this.f("GO", this.finish),
      inverse ? this.f("GO", this.finish) : NIL,
    );
  }
  input(g, value, name = "LOOP INPUT") {
    const v = fresh(name);
    g.inputs.push(list(v, value));
    return v;
  }
  stage(g, value) {
    const v = fresh("LOOP NEXT");
    this.temporaries.push(list(v, NIL));
    g.steps.push(this.f("SETQ", v, value));
    return v;
  }
  group() {
    return {
      inputs: [],
      variables: [],
      steps: [],
      exhaustion: [],
      assignments: [],
      destructures: [],
      bounds: [],
    };
  }
  // AND clauses stage against the old induction state. Exhaustion runs before
  // committing any user variable, so FINALLY observes the last complete step.
  add(g) {
    this.prologue.push(g.inputs, g.variables);
    this.induction.push(
      ...g.steps,
      ...g.exhaustion,
      this.f("PSETQ", ...g.assignments),
      ...g.destructures,
      ...g.bounds,
    );
  }
  pattern(g, pattern, name) {
    const pairs = this.destructure(pattern, name);
    for (const [v, value] of pairs) {
      g.variables.push(list(v, NIL));
      g.destructures.push(this.f("SETQ", v, value));
    }
  }
  withClause() {
    this.prologueOnly();
    const bindings = [],
      parts = [];
    do {
      const pattern = this.take(),
        type = this.type(),
        value = this.consume("=") ? this.take() : this.default(pattern, type);
      if (pattern === NIL) bindings.push(list(fresh("LOOP IGNORED"), value));
      else if (pattern instanceof Cons) {
        const temp = fresh("LOOP WITH");
        bindings.push(list(temp, value));
        parts.push(
          ...this.destructure(pattern, temp).map(([v, x]) => list(v, x)),
        );
      } else bindings.push(list(this.record(pattern), value));
    } while (this.consume("AND"));
    this.prologue.push(bindings, parts);
  }
  // ON exposes its final atom to FINALLY; destructured ON only commits conses.
  listIteration(g, v, expression, mode) {
    const initial = this.input(g, expression),
      cursor = fresh("LOOP CURSOR");
    g.variables.push(list(cursor, initial), list(v, NIL));
    const step = this.consume("BY")
      ? this.f("FUNCALL", this.input(g, this.take()), cursor)
      : this.f("CDR", cursor);
    const next = this.stage(g, this.f("IF", this.first, cursor, step));
    const check = this.leave(this.f(mode === "IN" ? "ENDP" : "ATOM", next));
    (mode === "ON" ? g.bounds : g.exhaustion).push(check);
    const value =
      mode === "IN"
        ? this.stage(
            g,
            this.f("IF", this.f("CONSP", next), this.f("CAR", next), NIL),
          )
        : next;
    g.assignments.push(cursor, next, v, value);
  }
  arithmetic(g, v, op) {
    let start = null,
      limit = null,
      step = null,
      down = null;
    const arithmetic = [
      "FROM",
      "UPFROM",
      "DOWNFROM",
      "TO",
      "UPTO",
      "BELOW",
      "DOWNTO",
      "ABOVE",
      "BY",
    ];
    const direction = (request) => {
      if (request !== null) {
        if (down !== null && down !== request)
          invalid("Conflicting LOOP arithmetic directions");
        down = request;
      }
    };
    for (;;) {
      const value = this.input(g, this.take());
      if (["FROM", "UPFROM", "DOWNFROM"].includes(op) && start === null) {
        start = value;
        direction(op === "FROM" ? null : op === "DOWNFROM");
      } else if (
        ["TO", "UPTO", "BELOW", "DOWNTO", "ABOVE"].includes(op) &&
        limit === null
      ) {
        limit = { value, inclusive: !["BELOW", "ABOVE"].includes(op) };
        direction(op === "TO" ? null : ["DOWNTO", "ABOVE"].includes(op));
      } else if (op === "BY" && step === null) step = value;
      else invalid("Duplicate LOOP arithmetic parameter");
      if (!arithmetic.includes(this.peek())) break;
      op = this.kw();
    }
    if (down && start === null)
      invalid("Descending LOOP iteration requires a start");
    g.variables.push(list(v, start ?? 0));
    const next = this.stage(
      g,
      this.f("IF", this.first, v, this.f(down ? "-" : "+", v, step ?? 1)),
    );
    if (limit)
      g.exhaustion.push(
        this.leave(
          this.f(
            down
              ? limit.inclusive
                ? "<"
                : "<="
              : limit.inclusive
                ? ">"
                : ">=",
            next,
            limit.value,
          ),
        ),
      );
    g.assignments.push(v, next);
  }
  across(g, v) {
    const vector = this.input(g, this.take()),
      length = fresh("LOOP LENGTH"),
      index = fresh("LOOP INDEX");
    g.variables.push(
      list(length, this.f("LENGTH", vector)),
      list(index, 0),
      list(v, NIL),
    );
    g.exhaustion.push(this.leave(this.f(">=", index, length)));
    g.assignments.push(
      v,
      this.f("AREF", vector, index),
      index,
      this.f("1+", index),
    );
  }
  being(g, v, pattern) {
    if (!this.consume("EACH")) this.consume("THE");
    const kind = this.kw();
    if (
      [
        "SYMBOL",
        "SYMBOLS",
        "PRESENT-SYMBOL",
        "PRESENT-SYMBOLS",
        "EXTERNAL-SYMBOL",
        "EXTERNAL-SYMBOLS",
      ].includes(kind)
    ) {
      if (pattern) invalid("LOOP symbol iteration requires a simple variable");
      const pkg =
        this.consume("IN") || this.consume("OF")
          ? this.take()
          : this.rt.intern("*PACKAGE*", "COMMON-LISP");
      const mode = kind.startsWith("EXTERNAL")
        ? 2
        : kind.startsWith("PRESENT")
          ? 3
          : 7;
      this.listIteration(
        g,
        v,
        list(helpers.get(this.rt).packages, pkg, mode | 16),
        "IN",
      );
      return;
    }
    if (!["HASH-KEY", "HASH-KEYS", "HASH-VALUE", "HASH-VALUES"].includes(kind))
      invalid("Invalid LOOP BEING subclause");
    if (!this.consume("IN") && !this.consume("OF"))
      invalid("LOOP hash iteration requires IN or OF");
    const table = this.input(g, this.take()),
      keys = kind.startsWith("HASH-KEY");
    let other = null;
    if (this.consume("USING")) {
      const pair = array(this.take());
      if (
        pair.length !== 2 ||
        !(
          keys ? ["HASH-VALUE", "HASH-VALUES"] : ["HASH-KEY", "HASH-KEYS"]
        ).includes(keyword(pair[0]))
      )
        invalid("Invalid LOOP USING clause");
      if (pair[1] !== NIL) {
        if (pair[1] instanceof Cons) {
          other = fresh("LOOP USING");
          g.variables.push(list(other, NIL));
          this.pattern(g, pair[1], other);
        } else {
          other = this.record(pair[1]);
          g.variables.push(list(other, NIL));
        }
      }
    }
    const cursor = fresh("LOOP HASH CURSOR"),
      next = fresh("LOOP HASH NEXT"),
      key = fresh("LOOP HASH KEY"),
      value = fresh("LOOP HASH VALUE");
    g.variables.push(
      list(cursor, 0),
      list(next, NIL),
      list(key, NIL),
      list(value, NIL),
      list(v, NIL),
    );
    g.steps.push(
      this.f(
        "MULTIPLE-VALUE-SETQ",
        list(next, key, value),
        list(helpers.get(this.rt).hash, table, cursor),
      ),
    );
    g.exhaustion.push(this.leave(this.f("NULL", next)));
    g.assignments.push(cursor, next, v, keys ? key : value);
    if (other) g.assignments.push(other, keys ? value : key);
  }
  forClause() {
    this.prologueOnly();
    const g = this.group();
    do {
      const designator = this.take(),
        pattern = designator instanceof Cons ? designator : null,
        v = designator === NIL || pattern ? fresh("LOOP VARIABLE") : designator;
      this.record(v);
      this.type();
      const op = this.kw();
      if (op === "IN" || op === "ON")
        this.listIteration(
          g,
          v,
          this.take(),
          op === "ON" && pattern ? "DESTRUCTURED-ON" : op,
        );
      else if (op === "=") {
        const init = this.take(),
          then = this.consume("THEN") ? this.take() : init;
        g.variables.push(list(v, NIL));
        g.assignments.push(
          v,
          this.stage(g, this.f("IF", this.first, init, then)),
        );
      } else if (op === "ACROSS") this.across(g, v);
      else if (op === "BEING") this.being(g, v, pattern);
      else if (
        [
          "FROM",
          "UPFROM",
          "DOWNFROM",
          "TO",
          "UPTO",
          "BELOW",
          "DOWNTO",
          "ABOVE",
          "BY",
        ].includes(op)
      ) {
        if (pattern) invalid("Arithmetic LOOP requires a simple variable");
        this.arithmetic(g, v, op);
      } else invalid(`Invalid LOOP FOR subclause ${op}`);
      if (pattern) this.pattern(g, pattern, v);
    } while (this.consume("AND"));
    this.add(g);
  }
  repeat() {
    this.prologueOnly();
    const g = this.group(),
      count = this.input(g, this.take()),
      next = this.stage(
        g,
        this.f("IF", this.first, count, this.f("1-", count)),
      );
    g.assignments.push(count, next);
    g.bounds.push(this.leave(this.f(">", next, 0), true));
    this.add(g);
  }
  value(it) {
    return it && this.consume("IT") ? it : this.take();
  }
  accumulator(kind, target, type) {
    let acc = target
      ? this.accumulators.find((a) => a.value === target)
      : this.result;
    if (!target && this.boolean !== null)
      invalid("LOOP implicit accumulation conflicts with boolean termination");
    if (acc) {
      if (acc.kind !== kind) invalid("Incompatible LOOP accumulation clauses");
      return acc;
    }
    const v = target ? this.record(target) : fresh("LOOP RESULT");
    acc = { kind, value: v, tail: kind === "list" ? fresh("LOOP TAIL") : null };
    this.prologue.push([list(v, kind === "sum" ? this.zero(type) : NIL)]);
    if (acc.tail) this.temporaries.push(list(acc.tail, NIL));
    this.accumulators.push(acc);
    if (!target) this.result = acc;
    return acc;
  }
  accumulate(k, it) {
    let value = this.value(it);
    const target = this.consume("INTO") ? variable(this.take()) : null;
    const isList = [
        "COLLECT",
        "COLLECTING",
        "APPEND",
        "APPENDING",
        "NCONC",
        "NCONCING",
      ].includes(k),
      kind = isList
        ? "list"
        : ["COUNT", "COUNTING", "SUM", "SUMMING"].includes(k)
          ? "sum"
          : "extremum";
    const type = isList ? null : this.type(),
      acc = this.accumulator(kind, target, type),
      v = acc.value;
    if (isList) {
      if (k.startsWith("COLLECT")) value = this.f("LIST", value);
      else if (k.startsWith("APPEND")) {
        this.appendCopy ??= fresh("LOOP COPY");
        value = this.f("FUNCALL", this.f("FUNCTION", this.appendCopy), value);
      }
      const chunk = fresh("LOOP CHUNK");
      return this.f(
        "LET",
        list(list(chunk, value)),
        this.f(
          "WHEN",
          chunk,
          this.f(
            "IF",
            acc.tail,
            this.f("RPLACD", acc.tail, chunk),
            this.f("SETQ", v, chunk),
          ),
          this.f("SETQ", acc.tail, this.f("LAST", chunk)),
        ),
      );
    }
    if (kind === "sum") {
      const counted = k.startsWith("COUNT"),
        add = this.f("SETQ", v, this.f("+", v, counted ? 1 : value));
      return counted ? this.f("IF", value, add) : add;
    }
    const candidate = fresh("LOOP CANDIDATE");
    return this.f(
      "LET",
      list(list(candidate, value)),
      this.f(
        "IF",
        this.f(
          "OR",
          this.f("NULL", v),
          this.f(
            "<",
            ...(k.startsWith("MIN") ? [candidate, v] : [v, candidate]),
          ),
        ),
        this.f("SETQ", v, candidate),
      ),
    );
  }
  selection(it) {
    const forms = [this.clause(this.kw(), it, true)];
    while (this.consume("AND")) forms.push(this.clause(this.kw(), null, true));
    return this.f("PROGN", ...forms);
  }
  clause(k, it = null, selected = false) {
    if (k === "DO" || k === "DOING") return this.f("PROGN", ...this.forms(k));
    if (k === "RETURN")
      return this.f("RETURN-FROM", this.block, this.value(it));
    if (["IF", "WHEN", "UNLESS"].includes(k)) {
      const test = this.take(),
        temp = fresh("LOOP TEST"),
        yes = this.selection(temp),
        no = this.consume("ELSE") ? this.selection(temp) : NIL;
      this.consume("END");
      return this.f(
        "LET",
        list(list(temp, test)),
        this.f("IF", temp, ...(k === "UNLESS" ? [no, yes] : [yes, no])),
      );
    }
    if (!selected && (k === "WHILE" || k === "UNTIL"))
      return this.leave(this.value(null), k === "WHILE");
    if (!selected && ["ALWAYS", "NEVER", "THEREIS"].includes(k)) {
      if (this.result)
        invalid(
          "LOOP boolean termination conflicts with implicit accumulation",
        );
      const value = this.value(null);
      if (k === "THEREIS") {
        this.boolean ??= false;
        const temp = fresh("LOOP FOUND");
        return this.f(
          "LET",
          list(list(temp, value)),
          this.f("IF", temp, this.f("RETURN-FROM", this.block, temp)),
        );
      }
      this.boolean = true;
      return this.f(
        "IF",
        k === "ALWAYS" ? this.f("NOT", value) : value,
        this.f("RETURN-FROM", this.block, NIL),
      );
    }
    if (
      [
        "COLLECT",
        "COLLECTING",
        "APPEND",
        "APPENDING",
        "NCONC",
        "NCONCING",
        "SUM",
        "SUMMING",
        "COUNT",
        "COUNTING",
        "MINIMIZE",
        "MINIMIZING",
        "MAXIMIZE",
        "MAXIMIZING",
      ].includes(k)
    )
      return this.accumulate(k, it);
    invalid(`Invalid LOOP clause ${k}`);
  }
  expand() {
    if (this.consume("NAMED")) {
      this.block = this.take();
      if (!(this.block instanceof LispSymbol))
        invalid("LOOP NAMED requires a symbol");
    }
    while (this.pos < this.args.length) {
      const k = this.kw();
      if (k === "WITH") this.withClause();
      else if (k === "FOR" || k === "AS") this.forClause();
      else if (k === "REPEAT") this.repeat();
      else if (k === "INITIALLY") this.initially.push(...this.forms(k));
      else if (k === "FINALLY") this.finally.push(...this.forms(k));
      else if (!this.main && (k === "WHILE" || k === "UNTIL"))
        this.induction.push(this.clause(k));
      else {
        this.main = true;
        this.body.push(this.clause(k));
      }
    }
    const start = fresh("LOOP START"),
      finish = this.f("GO", this.finish);
    // The lexical finish macro uses the same GO machinery as user TAGBODY,
    // including unwinding cleanup and rejecting transfers after lexical extent.
    let body = this.f(
      "MACROLET",
      list(
        list(
          this.rt.intern("LOOP-FINISH", "COMMON-LISP"),
          NIL,
          this.f("QUOTE", finish),
        ),
      ),
      this.f(
        "TAGBODY",
        ...this.initially,
        start,
        ...this.induction,
        this.f("SETQ", this.first, NIL),
        ...this.body,
        this.f("GO", start),
        this.finish,
        ...this.finally,
        this.f(
          "RETURN-FROM",
          this.block,
          this.result?.value ?? (this.boolean ? T : NIL),
        ),
      ),
    );
    for (const batch of this.prologue.toReversed())
      if (batch.length) body = this.f("LET", fromArray(batch), body);
    body = this.f(
      "BLOCK",
      this.block,
      this.f("LET", fromArray(this.temporaries), body),
    );
    if (this.appendCopy) {
      const source = fresh("LOOP COPY SOURCE");
      body = this.f(
        "LABELS",
        list(
          list(
            this.appendCopy,
            list(source),
            this.f(
              "IF",
              this.f("CONSP", source),
              this.f(
                "CONS",
                this.f("CAR", source),
                this.f(
                  "FUNCALL",
                  this.f("FUNCTION", this.appendCopy),
                  this.f("CDR", source),
                ),
              ),
              source,
            ),
          ),
        ),
        body,
      );
    }
    return body;
  }
}

function loop(rt, args) {
  const loop = new Loop(rt, args);
  if (!args.length || args[0] instanceof Cons) {
    if (args.some((x) => !(x instanceof Cons)))
      invalid("Simple LOOP requires compound forms");
    const start = fresh("LOOP START");
    return loop.f(
      "BLOCK",
      NIL,
      loop.f("TAGBODY", start, ...args, loop.f("GO", start)),
    );
  }
  return loop.expand();
}
function iterate(rt, name, args) {
  const c = new Loop(rt, []),
    f = c.f.bind(c);
  if (!args.length) invalid(`${name} requires a specification`);
  const start = fresh("ITERATE START"),
    done = fresh("ITERATE DONE");
  let body = args.slice(name === "DO" || name === "DO*" ? 2 : 1),
    declarations = [];
  while (
    body[0] instanceof Cons &&
    body[0].car === rt.intern("DECLARE", "COMMON-LISP")
  )
    declarations.push(body.shift());
  if (name === "DO" || name === "DO*") {
    if (args.length < 2) invalid(`${name} requires a termination clause`);
    const bindings = array(args[0]).map((x) =>
        x instanceof Cons ? array(x) : [x],
      ),
      end = array(args[1]);
    if (!end.length) invalid("DO requires a termination test");
    const names = new Set();
    for (const b of bindings) {
      if (b.length < 1 || b.length > 3) invalid("Invalid DO binding");
      variable(b[0]);
      if (names.has(b[0])) invalid("Duplicate DO variable");
      names.add(b[0]);
    }
    const steps = bindings
      .filter((b) => b.length === 3)
      .flatMap((b) => [b[0], b[2]]);
    return f(
      "BLOCK",
      NIL,
      f(
        name === "DO" ? "LET" : "LET*",
        fromArray(bindings.map((b) => list(b[0], b[1] ?? NIL))),
        ...declarations,
        f(
          "TAGBODY",
          start,
          f("IF", end[0], f("GO", done)),
          ...body,
          f(name === "DO" ? "PSETQ" : "SETQ", ...steps),
          f("GO", start),
          done,
        ),
        ...end.slice(1),
      ),
    );
  }
  const spec = array(args[0]);
  if (spec.length < 2 || spec.length > 3)
    invalid(`Invalid ${name} specification`);
  const v = variable(spec[0]),
    temp = fresh("ITERATE INPUT");
  if (name === "DOLIST")
    return f(
      "BLOCK",
      NIL,
      f(
        "LET",
        list(list(temp, spec[1])),
        f(
          "LET",
          list(list(v, NIL)),
          ...declarations,
          f(
            "TAGBODY",
            start,
            f("IF", f("ENDP", temp), f("GO", done)),
            f("SETQ", v, f("CAR", temp)),
            ...body,
            f("SETQ", temp, f("CDR", temp)),
            f("GO", start),
            done,
          ),
          f("SETQ", v, NIL),
          spec[2] ?? NIL,
        ),
      ),
    );
  return f(
    "BLOCK",
    NIL,
    f(
      "LET",
      list(list(temp, spec[1])),
      f(
        "LET",
        list(list(v, 0)),
        ...declarations,
        f(
          "TAGBODY",
          start,
          f("IF", f(">=", v, temp), f("GO", done)),
          ...body,
          f("SETQ", v, f("1+", v)),
          f("GO", start),
          done,
        ),
        spec[2] ?? NIL,
      ),
    ),
  );
}

function assertion(rt, args) {
  if (!args.length) invalid("ASSERT requires a test form");
  const f = (name, ...xs) => list(rt.intern(name, "COMMON-LISP"), ...xs);
  const q = (value) => f("QUOTE", value);
  const places = args.length > 1 ? array(args[1]) : [];
  const error =
    args.length > 2
      ? f("ERROR", ...args.slice(2))
      : f("ERROR", new LispString("The assertion ~S failed."), q(args[0]));
  const retry = fresh("ASSERT RETRY"),
    done = fresh("ASSERT DONE");
  const skip = q(fresh("KEEP ASSERT PLACE")),
    inputs = [],
    assignments = [],
    parameters = [rt.intern("&OPTIONAL", "COMMON-LISP")];
  const query = rt.intern("*QUERY-IO*", "COMMON-LISP"),
    debug = rt.intern("*DEBUG-IO*", "COMMON-LISP");
  for (const place of places) {
    const value = fresh("ASSERT REPLACEMENT");
    parameters.push(list(value, skip));
    const tagged = f("AND", f("CONSP", value), f("EQ", f("CAR", value), skip));
    const replacement = f(
      "IF",
      tagged,
      f("VALUES-LIST", f("CDR", value)),
      value,
    );
    assignments.push(
      f("UNLESS", f("EQ", value, skip), f("SETF", place, replacement)),
    );
    const display = f(
      "FORMAT",
      debug,
      new LispString("~&New value for ~S: "),
      q(place),
    );
    const input = f(
      "PROGN",
      display,
      f("FINISH-OUTPUT", debug),
      f("CONS", skip, f("MULTIPLE-VALUE-LIST", f("EVAL", f("READ", debug)))),
    );
    inputs.push(
      f(
        "IF",
        f("Y-OR-N-P", new LispString("Supply a new value for ~S? "), q(place)),
        input,
        skip,
      ),
    );
  }
  const continuation = [
    rt.intern("CONTINUE", "COMMON-LISP"),
    places.length ? fromArray(parameters) : NIL,
    rt.intern("REPORT", "KEYWORD"),
    new LispString("Retry the assertion."),
  ];
  if (places.length)
    continuation.push(
      rt.intern("INTERACTIVE", "KEYWORD"),
      f(
        "LAMBDA",
        NIL,
        f(
          "LET",
          list(list(query, debug)),
          f("DECLARE", f("SPECIAL", query)),
          f("LIST", ...inputs),
        ),
      ),
    );
  continuation.push(...assignments);
  return f(
    "TAGBODY",
    retry,
    f("IF", args[0], f("GO", done)),
    f("RESTART-CASE", error, fromArray(continuation)),
    f("GO", retry),
    done,
  );
}

function standardIoSyntax(rt, args) {
  const f = (name, ...xs) => list(rt.intern(name, "COMMON-LISP"), ...xs);
  const defaults = {
    "*PACKAGE*": rt.findPackage("CL-USER"),
    "*PRINT-ARRAY*": T,
    "*PRINT-BASE*": 10,
    "*PRINT-CASE*": rt.intern("UPCASE", "KEYWORD"),
    "*PRINT-CIRCLE*": NIL,
    "*PRINT-ESCAPE*": T,
    "*PRINT-GENSYM*": T,
    "*PRINT-LENGTH*": NIL,
    "*PRINT-LEVEL*": NIL,
    "*PRINT-LINES*": NIL,
    "*PRINT-MISER-WIDTH*": NIL,
    "*PRINT-PPRINT-DISPATCH*": f("COPY-PPRINT-DISPATCH", NIL),
    "*PRINT-PRETTY*": NIL,
    "*PRINT-RADIX*": NIL,
    "*PRINT-READABLY*": T,
    "*PRINT-RIGHT-MARGIN*": NIL,
    "*READ-BASE*": 10,
    "*READ-DEFAULT-FLOAT-FORMAT*": f(
      "QUOTE",
      rt.intern("SINGLE-FLOAT", "COMMON-LISP"),
    ),
    "*READ-EVAL*": T,
    "*READ-SUPPRESS*": NIL,
    "*READTABLE*": f("COPY-READTABLE", NIL),
  };
  const entries = Object.entries(defaults).map(([name, value]) => [
    rt.intern(name, "COMMON-LISP"),
    value,
  ]);
  return f(
    "LET",
    fromArray(entries.map(([v, value]) => list(v, value))),
    f("DECLARE", f("SPECIAL", ...entries.map(([v]) => v))),
    f("PROGN", ...args),
  );
}

function withOutputToString(rt, args) {
  if (!args.length) invalid("WITH-OUTPUT-TO-STRING requires a binding");
  const [stream, destination = NIL, ...options] = array(args[0]);
  variable(stream);
  const f = (name, ...xs) => list(rt.intern(name, "COMMON-LISP"), ...xs);
  if (options.length && (options.length !== 2 || options[0] !== rt.intern("ELEMENT-TYPE", "KEYWORD")))
    invalid("Invalid WITH-OUTPUT-TO-STRING option");
  const elementType = options.length ? options[1] : f("QUOTE", rt.intern("CHARACTER", "COMMON-LISP"));
  return f("%CALL-WITH-OUTPUT-TO-STRING", destination, elementType, f("LAMBDA", list(stream), ...args.slice(1)));
}

function withOpen(rt, args, file) {
  if (!args.length) invalid('An open stream binding is required');
  const [stream, expression, ...options] = array(args[0]);
  variable(stream);
  if (expression === undefined || !file && options.length) invalid('Invalid open stream binding');
  const f = (name, ...xs) => list(rt.intern(name, 'COMMON-LISP'), ...xs);
  const done = fresh('STREAM-COMPLETED'), abort = rt.intern('ABORT', 'KEYWORD');
  const body = args.slice(1), declarations = [];
  while (body[0] instanceof Cons && body[0].car === rt.intern('DECLARE', 'COMMON-LISP')) declarations.push(body.shift());
  const open = file ? f('OPEN', expression, ...options) : expression;
  return f('LET', list(list(stream, open), list(done, NIL)), ...declarations,
    f('UNWIND-PROTECT', f('MULTIPLE-VALUE-PROG1', f('PROGN', ...body), f('SETQ', done, T)),
      f('WHEN', stream, f('CLOSE', stream, abort, f('NOT', done)))));
}

function pprintLogicalBlock(rt, args) {
  if (!args.length) invalid("PPRINT-LOGICAL-BLOCK requires a binding");
  const binding = array(args[0]);
  if (binding.length < 2 || binding.length % 2)
    invalid("Invalid PPRINT-LOGICAL-BLOCK binding");
  const [source, object, ...options] = binding;
  if (source !== NIL && source !== T) variable(source);
  const f = (name, ...xs) => list(rt.intern(name, "COMMON-LISP"), ...xs);
  const stream = source === NIL || source === T ? fresh("PPRINT STREAM") : source;
  const block = fresh("PPRINT BLOCK"), result = fresh("PPRINT OBJECT");
  const pop = f("LET", list(list(result, f("%PPRINT-POP", stream))),
    f("IF", f("%PPRINT-STOPPED-P", stream), f("RETURN-FROM", block, NIL), result));
  const exit = f("IF", f("%PPRINT-EXHAUSTED-P", stream), f("RETURN-FROM", block, NIL), NIL);
  const definitions = list(f("PPRINT-POP", NIL, f("QUOTE", pop)),
    f("PPRINT-EXIT-IF-LIST-EXHAUSTED", NIL, f("QUOTE", exit)));
  const body = args.slice(1), declarations = [];
  while (body[0] instanceof Cons && body[0].car === rt.intern("DECLARE", "COMMON-LISP"))
    declarations.push(body.shift());
  const lambda = f("LAMBDA", list(stream), ...declarations,
    f("BLOCK", block, f("MACROLET", definitions, ...body)));
  return f("%CALL-PPRINT-LOGICAL-BLOCK", source, object, lambda, ...options);
}

export function expandBuiltinMacro(rt, form) {
  if (!(form instanceof Cons) || !(form.car instanceof LispSymbol))
    return undefined;
  const name = form.car.name;
  if (rt.findSymbol(name, "COMMON-LISP") !== form.car) return undefined;
  const args = array(form.cdr);
  if (name === "FORMATTER") {
    if (args.length !== 1 || !(args[0] instanceof LispString))
      invalid("FORMATTER requires a literal control string");
    parseFormat(args[0].toString());
    return list(rt.intern("%COMPILE-FORMATTER", "COMMON-LISP"), args[0]);
  }
  if (name === "LOOP") return loop(rt, args);
  if (name === "ASSERT") return assertion(rt, args);
  if (["DO-SYMBOLS", "DO-EXTERNAL-SYMBOLS", "DO-ALL-SYMBOLS"].includes(name)) {
    const [control, ...body] = args;
    if (!(control instanceof Cons)) invalid(`${name} requires a control specification`);
    const [variable, second, third, ...extra] = array(control), all = name === "DO-ALL-SYMBOLS";
    if (extra.length || all && third !== undefined) invalid(`Invalid ${name} control specification`);
    const pkg = all ? NIL : second ?? rt.intern("*PACKAGE*", "COMMON-LISP");
    const mode = all ? 8 : name === "DO-EXTERNAL-SYMBOLS" ? 18 : 23;
    return iterate(rt, "DOLIST", [list(variable, list(helpers.get(rt).packages, pkg, mode), (all ? second : third) ?? NIL), ...body]);
  }
  if (name === "WITH-PACKAGE-ITERATOR") {
    const [control, ...body] = args;
    if (!(control instanceof Cons)) invalid("WITH-PACKAGE-ITERATOR requires an iterator specification");
    const [iterator, packageList, ...kinds] = array(control);
    if (!(iterator instanceof LispSymbol) || iterator === NIL || packageList === undefined || !kinds.length)
      invalid("WITH-PACKAGE-ITERATOR expects an iterator name, package list, and symbol types");
    let mask = 0;
    for (const kind of kinds) {
      const bit = ["INTERNAL", "EXTERNAL", "INHERITED"].findIndex(name => kind === rt.intern(name, "KEYWORD"));
      if (bit < 0) invalid("Invalid WITH-PACKAGE-ITERATOR symbol type");
      mask |= 1 << bit;
    }
    const entries = fresh("PACKAGE ENTRIES"), entry = fresh("ENTRY");
    const f = (name, ...values) => list(rt.intern(name, "COMMON-LISP"), ...values);
    const step = f("IF", entries, f("LET", list(list(entry, entries)),
      f("SETQ", entries, f("CDDDR", entries)), f("VALUES", T, f("CAR", entry), f("CADR", entry), f("CADDR", entry))), NIL);
    return f("LET", list(list(entries, list(helpers.get(rt).packages, packageList, mask))),
      f("FLET", list(list(iterator, NIL, step)), ...body));
  }
  if (name === "WITH-HASH-TABLE-ITERATOR") {
    const [spec, ...body] = args;
    if (!(spec instanceof Cons)) invalid("WITH-HASH-TABLE-ITERATOR requires an iterator specification");
    const [iterator, form, ...extra] = array(spec);
    if (!(iterator instanceof LispSymbol) || iterator === NIL || form === undefined || extra.length)
      invalid("WITH-HASH-TABLE-ITERATOR expects an iterator name and a table form");
    const table = fresh("HASH TABLE"), cursor = fresh("HASH CURSOR"), next = fresh("NEXT"), key = fresh("KEY"), value = fresh("VALUE");
    const f = (name, ...values) => list(rt.intern(name, "COMMON-LISP"), ...values);
    const step = f("MULTIPLE-VALUE-BIND", list(next, key, value), f("%%HASH-TABLE-STEP", table, cursor),
      f("IF", next, f("PROGN", f("SETQ", cursor, next), f("VALUES", T, key, value)), NIL));
    return f("LET", list(list(table, form), list(cursor, 0)), f("%%HASH-TABLE-STEP", table, -1),
      f("MACROLET", list(list(iterator, NIL, f("QUOTE", step))), ...body));
  }
  if (name === "WITH-OUTPUT-TO-STRING") return withOutputToString(rt, args);
  if (name === 'WITH-OPEN-FILE' || name === 'WITH-OPEN-STREAM') return withOpen(rt, args, name === 'WITH-OPEN-FILE');
  if (name === "PPRINT-LOGICAL-BLOCK") return pprintLogicalBlock(rt, args);
  if (name === "PPRINT-POP" || name === "PPRINT-EXIT-IF-LIST-EXHAUSTED")
    invalid(`${name} requires an enclosing PPRINT-LOGICAL-BLOCK`);
  if (name === "WITH-STANDARD-IO-SYNTAX") return standardIoSyntax(rt, args);
  if (["DO", "DO*", "DOLIST", "DOTIMES"].includes(name))
    return iterate(rt, name, args);
  if (name === "LOOP-FINISH")
    invalid("LOOP-FINISH is only valid within an extended LOOP");
  return undefined;
}
export function installMacros(rt) {
  if (helpers.has(rt)) return rt;
  // Restart names are standard CL symbols even when no function uses the name.
  rt.exportSymbol(rt.intern("CONTINUE", "COMMON-LISP"), "COMMON-LISP");
  const hash = rt.intern("%%HASH-TABLE-STEP", "COMMON-LISP"),
    packages = rt.intern("%%PACKAGE-ITERATOR-ENTRIES", "COMMON-LISP");
  helpers.set(rt, { hash, packages });
  for (const name of [
    "LOOP",
    "LOOP-FINISH",
    "DO",
    "DO*",
    "DOLIST",
    "DOTIMES",
    "WITH-STANDARD-IO-SYNTAX",
    "WITH-HASH-TABLE-ITERATOR",
    "WITH-PACKAGE-ITERATOR",
    "DO-SYMBOLS",
    "DO-EXTERNAL-SYMBOLS",
    "DO-ALL-SYMBOLS",
    "ASSERT",
    "FORMATTER",
    "WITH-OUTPUT-TO-STRING",
    'WITH-OPEN-FILE',
    'WITH-OPEN-STREAM',
    "PPRINT-LOGICAL-BLOCK",
    "PPRINT-POP",
    "PPRINT-EXIT-IF-LIST-EXHAUSTED",
  ]) {
    const sym = rt.intern(name, "COMMON-LISP");
    rt.exportSymbol(sym, "COMMON-LISP");
    rt.defineFunction(
      sym,
      new NativeFunction(name, (args) => {
        if (args.length !== 2)
          invalid(`Macro function ${name} requires exactly two arguments`);
        return expandBuiltinMacro(rt, args[0]);
      }),
      true,
    );
  }
  return rt;
}
