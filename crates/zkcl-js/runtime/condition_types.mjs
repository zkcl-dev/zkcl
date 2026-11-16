import { Cons, LispSymbol, NIL, toArray } from './objects.mjs';
import { LispCondition } from './execution.mjs';
import { LispClass } from './clos_layout.mjs';
import { NumericError } from './numbers.mjs';

const TYPE_NESTING_LIMIT = 256;

function accepts(type, cpl) {
  if (type instanceof LispClass) return cpl.includes(type);
  return type.union ? type.members.some(member => accepts(member, cpl))
    : type.members.every(member => accepts(member, cpl));
}

function collectSearchRoots(type, roots) {
  if (type instanceof LispClass) roots.push(type);
  else if (type.union) for (const member of type.members) collectSearchRoots(member, roots);
  else collectSearchRoots(type.members[0], roots);
}

function collectParents(type, parents, choices, cursor) {
  if (type instanceof LispClass) parents.add(type);
  else if (type.union) {
    const index = cursor.next++;
    choices[index] ??= { arm: 0, count: type.members.length };
    collectParents(type.members[choices[index].arm], parents, choices, cursor);
  } else for (const member of type.members) collectParents(member, parents, choices, cursor);
}

function advanceUnionArm(choices) {
  while (choices.length) {
    const choice = choices.at(-1);
    if (++choice.arm < choice.count) return true;
    choices.pop();
  }
  return false;
}

export function conditionClassResolver(rt, conditionClass, definition, makeIntersection) {
  const condition = conditionClass.name;
  const and = rt.intern('AND', 'COMMON-LISP'), or = rt.intern('OR', 'COMMON-LISP');
  return function* (original, task) {
    const invalid = () => new LispCondition('TYPE-ERROR', 'Expected a constructible condition type', {
      datum: original, expectedType: condition,
    });
    const leaves = new Set();
    const cpl = cls => definition(cls, task).cpl;
    const parse = function* (specifier, depth) {
      if (depth >= TYPE_NESTING_LIMIT)
        throw new LispCondition('PROGRAM-ERROR', 'Condition type expansion exceeds the nesting limit');
      if (specifier instanceof LispSymbol || specifier instanceof LispClass) {
        const cls = rt.findClass(specifier, false);
        if (cls && cpl(cls).includes(conditionClass)) {
          leaves.add(cls);
          return cls;
        }
      }
      if (rt.expandTypeSteps) {
        const [expanded, changed] = yield* rt.expandTypeSteps(specifier, NIL);
        if (changed) return yield* parse(expanded, depth + 1);
      }
      if (!(specifier instanceof Cons) || (specifier.car !== and && specifier.car !== or)) throw invalid();
      let operands;
      try {
        operands = toArray(specifier.cdr);
      } catch (error) {
        if (!(error instanceof NumericError)) throw error;
        throw invalid();
      }
      if (!operands.length) throw invalid();
      const members = [];
      for (const operand of operands) members.push(yield* parse(operand, depth + 1));
      return { union: specifier.car === or, members };
    };
    const type = yield* parse(original, 0);
    if (type instanceof LispClass) return type;

    // The zkcl compound-type extension preserves OR nested inside AND.
    // Prefer named operands, then the closest existing subclass of a search root.
    for (const cls of leaves) if (accepts(type, cpl(cls))) return cls;
    const pending = [], seen = new Set();
    collectSearchRoots(type, pending);
    for (let i = 0; i < pending.length; i++) {
      const cls = pending[i];
      if (seen.has(cls)) continue;
      seen.add(cls);
      if (definition(cls, task).finalized && accepts(type, cpl(cls))) return cls;
      for (const child of cls.children) pending.push(child);
    }

    // If two parents impose conflicting precedence orders, consider the
    // next OR arm without expanding branches unrelated to the current choice.
    const choices = [];
    let conflict;
    do {
      const parents = new Set();
      collectParents(type, parents, choices, { next: 0 });
      const precedence = new Set();
      for (const parent of parents) for (const ancestor of cpl(parent).slice(1)) precedence.add(ancestor);
      const direct = [...parents].filter(parent => !precedence.has(parent));
      const [cls, error] = yield* makeIntersection(direct, task);
      if (cls) return cls;
      conflict = error;
    } while (advanceUnionArm(choices));
    throw conflict;
  };
}
