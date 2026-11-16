import { NIL, T, UNBOUND } from './objects.mjs';
import { LispCondition } from './execution.mjs';

const instanceStorageOwners = new WeakSet();

export function hasStandardStorage(object) { return instanceStorageOwners.has(object); }

// The class definition and the storage of the class metaobject are independent.
export class LispClass {
  constructor(name, { builtin = false } = {}) {
    this.name = name;
    this.builtin = builtin;
    this.directSuperclasses = [];
    this.directSlots = [];
    this.cpl = [this];
    this.slots = new Map();
    this.locations = [];
    this.defaultInitargs = new Map();
    this.directDefaultInitargs = new Map();
    this.children = new Set();
    this.subclasses = new Set();
    this.version = 0;
    this.accessorMethods = [];
    this.directMethods = new Set();
    this.dependents = new Set();
    this.documentation = NIL;
    this.forwardReferenced = false;
    this.finalized = false;
  }
}

export class StandardInstance {
  constructor(classObject, definition) { initializeStorage(this, classObject, definition); }
}

export class SlotDefinition {
  constructor(classObject, fields = {}) {
    this.classObject = classObject;
    Object.assign(this, fields);
    this.location = NIL;
  }
}

export function initializeStorage(object, classObject, definition = classObject) {
  // Layouts are replaced on publication. Instances keep their original layout
  // until migration, so they can share the map without copying it.
  attachStorage(object, classObject, { version: definition.version, layout: definition.slots, values: new Map() });
}

export function attachStorage(object, classObject, storage) {
  object.classObject = classObject;
  object.storage = storage;
  instanceStorageOwners.add(object);
}

export class ClassPrecedenceError extends LispCondition {
  constructor(cls) {
    super('PROGRAM-ERROR', 'Inconsistent class precedence constraints');
    this.precedenceClass = cls;
  }
}

export function classPrecedence(cls, parents = c => c.directSuperclasses) {
  const constraints = new Map();
  const pending = [cls];
  while (pending.length) {
    const current = pending.pop();
    if (constraints.has(current)) continue;
    const direct = parents(current);
    constraints.set(current, { direct, incoming: 0, successors: new Set() });
    for (let i = direct.length - 1; i >= 0; i--) pending.push(direct[i]);
  }
  for (const [c, { direct }] of constraints) {
    let previous = c;
    for (const parent of direct) {
      const successors = constraints.get(previous).successors;
      if (!successors.has(parent)) {
        successors.add(parent);
        constraints.get(parent).incoming++;
      }
      previous = parent;
    }
  }
  const ready = new Set([...constraints.keys()].filter(c => constraints.get(c).incoming === 0));
  const order = [];
  while (ready.size) {
    let next;
    if (ready.size > 1) {
      // ANSI chooses a direct superclass of the rightmost possible class
      // already in the precedence list when several candidates are ready.
      for (let i = order.length - 1; i >= 0 && next === undefined; i--)
        next = constraints.get(order[i]).direct.find(parent => ready.has(parent));
    }
    next ??= ready.values().next().value;
    ready.delete(next);
    order.push(next);
    for (const child of constraints.get(next).successors)
      if (--constraints.get(child).incoming === 0) ready.add(child);
  }
  if (order.length !== constraints.size) throw new ClassPrecedenceError(cls);
  return order;
}

export function collectDefaultInitargs(cpl, definition) {
  const defaults = new Map();
  for (const parent of cpl)
    for (const [key, entry] of definition(parent).directDefaultInitargs)
      if (!defaults.has(key)) defaults.set(key, entry);
  return defaults;
}

export function groupDirectSlots(cpl, definition) {
  const groups = new Map();
  for (const owner of cpl) for (const slot of definition(owner).directSlots) {
    const group = groups.get(slot.name);
    if (group) group.push(slot);
    else groups.set(slot.name, [slot]);
  }
  return groups;
}

export function effectiveSlotFields(name, direct, and) {
  const first = direct[0], initial = direct.find(slot => slot.hasInitform);
  const types = direct.map(slot => slot.type).filter(type => type !== T);
  return {
    name, allocation: first.allocation, cell: first.cell, conditionField: first.conditionField,
    initargs: [...new Set(direct.flatMap(slot => slot.initargs))],
    hasInitform: initial !== undefined, initfunction: initial?.initfunction ?? NIL, initform: initial?.initform ?? NIL,
    type: types.length > 1 ? and(types) : types[0] ?? T,
    documentation: direct.find(slot => slot.documentation !== NIL)?.documentation ?? NIL,
    readers: [], writers: [],
  };
}

export function assignSlotLocations(slots) {
  const locations = [];
  for (const slot of slots) if (slot.allocation === 'INSTANCE') {
    slot.location = locations.length;
    locations.push(slot);
  }
  return locations;
}

export function slotLayout(slots) {
  const locations = assignSlotLocations(slots);
  for (const slot of slots) slot.finalized = true;
  return { slots: new Map(slots.map(slot => [slot.name, slot])), locations };
}

export function rawSlotValue(record, object, slot) {
  if (slot.allocation === 'CLASS') return slot.cell.value;
  if (slot.conditionField) return object[slot.conditionField] ?? UNBOUND;
  return record.storage.values.get(slot.name) ?? UNBOUND;
}

export function writeRawSlot(record, object, slot, value) {
  if (slot.allocation === 'CLASS') slot.cell.value = value;
  else if (slot.conditionField) object[slot.conditionField] = value;
  else if (value === UNBOUND) record.storage.values.delete(slot.name);
  else record.storage.values.set(slot.name, value);
  return value;
}
