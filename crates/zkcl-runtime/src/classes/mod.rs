use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::condition::{self, Class, Fault};
use crate::heap::{ClassKind, RootSlot, Word};
use crate::vm::{self, classes, instances};
use crate::{EvalError, Root, Runtime};

mod image;

#[derive(Default)]
pub(crate) struct Registry {
    /// Canonical class object for each class name, or an explicit FIND-CLASS
    /// association (None means the name was dissociated).
    pub(crate) objects: HashMap<usize, RootSlot>,
    pub(crate) associations: HashMap<usize, Option<RootSlot>>,
    claimed_names: HashSet<usize>,
    object_definitions: HashMap<usize, Rc<ClassDefinition>>,
    /// The system class graph is immutable. DEFSTRUCT and user classes can change.
    system_precedence: HashMap<usize, Rc<[Rc<RootSlot>]>>,
    subclasses: HashMap<usize, Vec<usize>>,
    // Direct protocol calls can edit a backpointer independently of the inheritance graph.
    subclass_edits: HashMap<usize, HashMap<usize, (Rc<RootSlot>, bool)>>,
    pub(crate) prototypes: HashMap<usize, RootSlot>,
    pending: Vec<Rc<ClassUpdate>>,
    pub(crate) next_serial: usize,
    pub(crate) builtin_condition_direct_slots: HashMap<Class, Rc<[DirectSlot]>>,
    pub(crate) builtin_condition_effective_slots: HashMap<Class, Rc<[EffectiveSlot]>>,
    object_declarations: HashMap<usize, Rc<RegisteredClass>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeClassKind {
    Standard,
    Condition,
}

#[derive(Clone)]
pub(crate) enum SlotKind {
    Instance,
    Class,
    Custom(Rc<RootSlot>),
}

#[derive(Clone)]
pub(crate) struct ClassDeclaration {
    pub(crate) kind: RuntimeClassKind,
    pub(crate) name: Rc<RootSlot>,
    pub(crate) slots: Rc<[DirectSlot]>,
    pub(crate) default_initargs: Vec<DefaultInitarg>,
    pub(crate) documentation: Option<Rc<RootSlot>>,
    pub(crate) report: Option<Rc<RootSlot>>,
}

pub(crate) struct DirectSlot {
    pub(crate) name: Rc<RootSlot>,
    pub(crate) initargs: Vec<Rc<RootSlot>>,
    pub(crate) initform: Option<Rc<RootSlot>>,
    pub(crate) initform_source: Option<Rc<RootSlot>>,
    pub(crate) readers: Option<Rc<RootSlot>>,
    pub(crate) writers: Option<Rc<RootSlot>>,
    pub(crate) metaobject: OnceCell<RootSlot>,
    pub(crate) allocation: SlotKind,
    pub(crate) type_specifier: Option<Rc<RootSlot>>,
    pub(crate) documentation: Option<Rc<RootSlot>>,
}

#[derive(Clone)]
pub(crate) struct DefaultInitarg {
    pub(crate) name: Rc<RootSlot>,
    pub(crate) initform: Rc<RootSlot>,
    pub(crate) initform_source: Rc<RootSlot>,
}

struct RegisteredClass {
    class: Rc<RootSlot>,
    direct_superclasses: Vec<Rc<RootSlot>>,
    declaration: Rc<ClassDeclaration>,
    active: bool,
    shared_slots: std::cell::RefCell<HashMap<usize, Rc<SharedSlot>>>,
    accessors: Rc<std::cell::RefCell<Vec<(RootSlot, RootSlot)>>>,
}

pub(crate) struct SharedSlot {
    owner: Rc<RootSlot>,
    pub(crate) cell: Rc<RootSlot>,
    initialized: std::cell::Cell<bool>,
}

impl SharedSlot {
    fn new(runtime: &Runtime, owner: &Rc<RootSlot>, at: usize) -> Result<Rc<Self>, Fault> {
        let cell = runtime
            .heap
            .cell(|| Word::UNBOUND)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        Ok(Rc::new(Self {
            owner: Rc::clone(owner),
            cell: Rc::new(runtime.root_slot(cell)),
            initialized: std::cell::Cell::new(false),
        }))
    }
}

pub(crate) struct ClassChanges {
    pub(crate) shared: Vec<SharedInitialization>,
    pub(crate) removed_accessors: Vec<(RootSlot, RootSlot)>,
}

pub(crate) struct SharedInitialization {
    pub(crate) class: Rc<RootSlot>,
    pub(crate) slot_name: Rc<RootSlot>,
    pub(crate) cell: Rc<RootSlot>,
    pub(crate) initform: Rc<RootSlot>,
}

#[derive(Clone)]
pub(crate) struct ClassDefinition {
    pub(crate) kind: RuntimeClassKind,
    pub(crate) slots: Rc<[EffectiveSlot]>,
    slot_indices: HashMap<usize, usize>,
    pub(crate) representation: Representation,
    pub(crate) prototype: OnceCell<Rc<RootSlot>>,
    pub(crate) default_initargs: Vec<DefaultInitarg>,
    pub(crate) instance_slots: usize,
    pub(crate) instance_layout: Rc<RootSlot>,
    pub(crate) documentation: Option<Rc<RootSlot>>,
    pub(crate) report: Option<Rc<RootSlot>>,
    pub(crate) precedence: Rc<[Rc<RootSlot>]>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Representation {
    Instance,
    FuncallableInstance,
    GenericFunction,
    Method,
    Specializer,
    DirectSlot,
    EffectiveSlot,
    StandardClass,
    FuncallableStandardClass,
    ForwardReferencedClass,
}

impl ClassDefinition {
    fn collect_shared_initializers(
        &self,
        runtime: &Runtime,
        scheduled: &mut HashSet<(usize, usize)>,
        initializations: &mut Vec<SharedInitialization>,
    ) {
        for slot in self.slots.iter() {
            if let SlotAllocation::Class(shared) = &slot.allocation
                && !shared.initialized.get()
                && scheduled.insert((
                    class_serial(runtime, shared.owner.get()),
                    symbol_id(runtime, slot.name.get()),
                ))
            {
                if let Some(initform) = &slot.initform {
                    initializations.push(SharedInitialization {
                        class: Rc::clone(&shared.owner),
                        slot_name: Rc::clone(&slot.name),
                        cell: Rc::clone(&shared.cell),
                        initform: Rc::clone(initform),
                    });
                } else {
                    shared.initialized.set(true);
                }
            }
        }
    }

    pub(crate) fn slot(&self, runtime: &Runtime, name: Word) -> Option<&EffectiveSlot> {
        self.slot_indices
            .get(&runtime.heap.symbol_attributes(name)?.id)
            .map(|&i| &self.slots[i])
    }
}

#[derive(Clone)]
pub(crate) struct EffectiveSlot {
    pub(crate) name: Rc<RootSlot>,
    pub(crate) allocation: SlotAllocation,
    pub(crate) initargs: Vec<Rc<RootSlot>>,
    pub(crate) initform: Option<Rc<RootSlot>>,
    pub(crate) initform_source: Option<Rc<RootSlot>>,
    pub(crate) metaobject: Rc<OnceCell<RootSlot>>,
    pub(crate) type_constraints: Vec<Rc<RootSlot>>,
    pub(crate) documentation: Option<Rc<RootSlot>>,
}

#[derive(Clone)]
pub(crate) enum SlotAllocation {
    Instance(usize),
    Class(Rc<SharedSlot>),
    Custom(Rc<RootSlot>),
}

fn symbol_id(runtime: &Runtime, name: Word) -> usize {
    runtime
        .heap
        .symbol_attributes(name)
        .expect("class or slot name")
        .id
}

pub(crate) fn declared(runtime: &Runtime, name: Word) -> bool {
    let Some(id) = runtime.heap.symbol_attributes(name).map(|name| name.id) else {
        return false;
    };
    let registry = runtime.classes.borrow();
    match registry.associations.get(&id) {
        Some(association) => association.is_some(),
        None => registry.objects.get(&id).is_some_and(|c| {
            registry
                .object_declarations
                .contains_key(&class_serial(runtime, c.get()))
        }),
    }
}

pub(crate) fn has_declaration(runtime: &Runtime, name: Word) -> bool {
    let Some(id) = runtime.heap.symbol_attributes(name).map(|n| n.id) else {
        return false;
    };
    let registry = runtime.classes.borrow();
    registry.objects.get(&id).is_some_and(|c| {
        registry
            .object_declarations
            .contains_key(&class_serial(runtime, c.get()))
    })
}

pub(crate) fn class_definition(runtime: &Runtime, class: Word) -> Option<Rc<ClassDefinition>> {
    let info = runtime.heap.class_info(class)?;
    runtime
        .classes
        .borrow()
        .object_definitions
        .get(&info.serial)
        .cloned()
}

/// Existing instances keep using the committed definition while a hook inspects
/// its newly finalized prototype. The layout distinguishes the two objects.
pub(crate) fn instance_definition(
    runtime: &Runtime,
    class: Word,
    layout: Word,
) -> Option<Rc<ClassDefinition>> {
    let registry = runtime.classes.borrow();
    let id = runtime.heap.class_info(class)?.serial;
    let committed = registry.object_definitions.get(&id);
    if !registry.pending.is_empty() && committed.is_none_or(|d| d.instance_layout.get() != layout) {
        for update in registry.pending.iter().rev() {
            if let Some(candidate) = update.definition(runtime, class)
                && candidate.instance_layout.get() == layout
            {
                return Some(candidate);
            }
        }
    }
    committed.cloned()
}

pub(crate) fn class_declaration(runtime: &Runtime, class: Word) -> Option<Rc<ClassDeclaration>> {
    let serial = runtime.heap.class_info(class)?.serial;
    Some(Rc::clone(
        &runtime
            .classes
            .borrow()
            .object_declarations
            .get(&serial)?
            .declaration,
    ))
}

pub(crate) fn class_direct_superclasses(
    runtime: &Runtime,
    class: Word,
    at: usize,
) -> Result<Vec<Rc<RootSlot>>, Fault> {
    if let Some(parents) = registered_superclasses(runtime, class) {
        return Ok(parents);
    }
    let info = runtime.heap.class_info(class).expect("class object");
    let names = classes::system_parents(runtime, info.origin_name, at)?.unwrap_or_default();
    resolve_system_classes(runtime, &names, at)
}

pub(crate) fn class_direct_subclasses(
    runtime: &Runtime,
    class: Word,
    at: usize,
) -> Result<Vec<Rc<RootSlot>>, Fault> {
    let class = runtime.root_slot(class);
    let parent_id = class_serial(runtime, class.get());
    let mut children = {
        let registry = runtime.classes.borrow();
        registry
            .subclasses
            .get(&parent_id)
            .into_iter()
            .flatten()
            .map(|id| Rc::clone(&registry.object_declarations[id].class))
            .collect::<Vec<_>>()
    };
    let previous = runtime.heap.previous_class_subclasses(class.get());
    if previous != Word::NIL {
        children.extend(
            vm::proper_list(runtime, previous, at)?
                .into_iter()
                .map(|c| Rc::new(runtime.root_slot(c))),
        );
    }
    let names = classes::system_subclass_names(runtime, class.get(), at)?;
    children.extend(resolve_system_classes(runtime, &names, at)?);
    let registry = runtime.classes.borrow();
    if let Some(edits) = registry.subclass_edits.get(&parent_id) {
        children.retain(|c| !edits.contains_key(&class_serial(runtime, c.get())));
        children.extend(
            edits
                .values()
                .filter(|(_, present)| *present)
                .map(|(c, _)| Rc::clone(c)),
        );
    }
    // Nested hooks see the innermost proposed declaration for each child.
    let mut seen = HashSet::new();
    for update in registry.pending.iter().rev() {
        let child = &update.registered.class;
        if seen.insert(class_serial(runtime, child.get()))
            && let Some(present) = update.subclass_change(class.get())
        {
            children.retain(|c| c.get() != child.get());
            if present {
                children.push(Rc::clone(child));
            }
        }
    }
    Ok(children)
}

pub(crate) fn edit_subclass(
    runtime: &Runtime,
    parent: Word,
    child: Word,
    add: bool,
    at: usize,
) -> Result<(), Fault> {
    let parent = runtime.root_slot(parent);
    let child = runtime.root_slot(child);
    let child_id = runtime
        .heap
        .class_info(child.get())
        .ok_or_else(|| {
            Fault::type_error(runtime, 
                child.get(),
                "CLASS",
                EvalError::new(at, "expected subclass"),
            )
        })?
        .serial;
    let parent_id = runtime
        .heap
        .class_info(parent.get())
        .ok_or_else(|| {
            Fault::type_error(runtime, 
                parent.get(),
                "CLASS",
                EvalError::new(at, "expected superclass"),
            )
        })?
        .serial;
    // The direct-superclass declaration publishes these edges as one unit.
    // Its generic function hooks run while both directions are still staged.
    if let Some(update) = current_update(runtime, child.get())
        && update.registered.class.get() == child.get()
        && update.subclass_change(parent.get()) == Some(add)
    {
        return Ok(());
    }
    let previous = runtime.heap.previous_class_subclasses(parent.get());
    let inherited = previous != Word::NIL
        && vm::proper_list(runtime, previous, at)?.contains(&child.get())
        || class_direct_superclasses(runtime, child.get(), at)?
            .iter()
            .any(|p| p.get() == parent.get());
    let mut registry = runtime.classes.borrow_mut();
    if inherited == add {
        if let Some(edits) = registry.subclass_edits.get_mut(&parent_id) {
            edits.remove(&child_id);
            if edits.is_empty() {
                registry.subclass_edits.remove(&parent_id);
            }
        }
    } else {
        registry
            .subclass_edits
            .entry(parent_id)
            .or_default()
            .insert(child_id, (Rc::new(child), add));
    }
    Ok(())
}

pub(crate) fn registered_superclasses(runtime: &Runtime, class: Word) -> Option<Vec<Rc<RootSlot>>> {
    let serial = runtime.heap.class_info(class)?.serial;
    let registry = runtime.classes.borrow();
    Some(
        registry
            .object_declarations
            .get(&serial)?
            .direct_superclasses
            .clone(),
    )
}

pub(crate) fn ensure_empty_system_definition(
    runtime: &Runtime,
    class: Word,
    at: usize,
) -> Result<Word, Fault> {
    if class_definition(runtime, class).is_some() {
        return Ok(class);
    }
    let class = runtime.root_slot(class);
    let precedence = class_precedence(runtime, class.get(), at)?;
    let empty = ClassDefinition {
        kind: RuntimeClassKind::Standard,
        slots: Rc::from([]),
        slot_indices: HashMap::new(),
        representation: classes::representation(runtime, &precedence, at)?,
        prototype: OnceCell::new(),
        default_initargs: Vec::new(),
        instance_slots: 0,
        instance_layout: Rc::new(instances::instance_layout(runtime, &[], at)?),
        documentation: None,
        report: None,
        precedence,
    };
    let definition = Rc::new(empty);
    let serial = runtime
        .heap
        .class_info(class.get())
        .expect("standard object class")
        .serial;
    let mut registry = runtime.classes.borrow_mut();
    registry.object_definitions.insert(serial, definition);
    Ok(class.get())
}

pub(crate) fn remember_accessor(runtime: &Runtime, class: Word, method: Word) -> Option<()> {
    let id = runtime.heap.class_info(class)?.serial;
    let owner = runtime.heap.method_info(method)?.owner;
    if owner == Word::NIL {
        return None;
    }
    let registry = runtime.classes.borrow();
    registry
        .object_declarations
        .get(&id)?
        .accessors
        .borrow_mut()
        .push((runtime.root_slot(owner), runtime.root_slot(method)));
    Some(())
}

pub(crate) fn initialize_shared_slot(
    runtime: &Runtime,
    class: Word,
    slot_name: Word,
    cell: Word,
    value: Word,
) -> Option<()> {
    let class_id = runtime.heap.class_info(class)?.serial;
    let slot_id = runtime.heap.symbol_attributes(slot_name)?.id;
    let registry = runtime.classes.borrow();
    let staged = registry.pending.iter().rev().find_map(|update| {
        let computed = update
            .computed_shared
            .borrow()
            .get(&(class_id, slot_id))
            .cloned();
        computed
            .or_else(|| {
                (update.registered.class.get() == class)
                    .then(|| {
                        update
                            .registered
                            .shared_slots
                            .borrow()
                            .get(&slot_id)
                            .cloned()
                    })
                    .flatten()
            })
            .filter(|shared| shared.cell.get() == cell)
    });
    let shared = staged.or_else(|| {
        registry
            .object_declarations
            .get(&class_id)?
            .shared_slots
            .borrow()
            .get(&slot_id)
            .cloned()
    });
    if let Some(shared) = shared
        && shared.cell.get() == cell
        && !shared.initialized.replace(true)
    {
        runtime
            .heap
            .write_cell(cell, value)
            .expect("shared slot cell");
    }
    Some(())
}

pub(crate) fn named_definition(runtime: &Runtime, name: Word) -> Option<Rc<ClassDefinition>> {
    let id = runtime.heap.symbol_attributes(name)?.id;
    let registry = runtime.classes.borrow();
    let class = match registry.associations.get(&id) {
        Some(association) => association.as_ref()?.get(),
        None => registry.objects.get(&id)?.get(),
    };
    let class = runtime.heap.class_info(class)?;
    registry.object_definitions.get(&class.serial).cloned()
}

pub(crate) fn make_instances_obsolete(
    runtime: &Runtime,
    class: Word,
    at: usize,
) -> Result<(), Fault> {
    let serial = runtime.heap.class_info(class).expect("class object").serial;
    let definitions = {
        let registry = runtime.classes.borrow();
        if !registry.object_definitions.contains_key(&serial) {
            return Err(incomplete(at));
        }
        affected_classes(&registry.subclasses, serial)
            .into_iter()
            .filter_map(|serial| {
                Some((serial, Rc::clone(registry.object_definitions.get(&serial)?)))
            })
            .collect::<Vec<_>>()
    };
    let mut replacements = Vec::with_capacity(definitions.len());
    for (serial, definition) in definitions {
        let mut definition = (*definition).clone();
        definition.instance_layout =
            Rc::new(instances::instance_layout(runtime, &definition.slots, at)?);
        replacements.push((serial, Rc::new(definition)));
    }
    let mut registry = runtime.classes.borrow_mut();
    for (serial, definition) in replacements {
        registry.object_definitions.insert(serial, definition);
    }
    Ok(())
}

pub(crate) fn class_definition_kind(runtime: &Runtime, name: Word) -> Option<RuntimeClassKind> {
    let id = runtime.heap.symbol_attributes(name)?.id;
    let registry = runtime.classes.borrow();
    let class = registry.objects.get(&id)?;
    Some(
        registry
            .object_declarations
            .get(&class_serial(runtime, class.get()))?
            .declaration
            .kind,
    )
}

fn precedence_error(at: usize) -> Fault {
    Fault::plain(
        Class::ProgramError,
        EvalError::new(at, "inconsistent class precedence"),
    )
}

struct ClassNode {
    class: Rc<RootSlot>,
    parents: Vec<usize>,
    dependents: HashSet<usize>,
    predecessors: usize,
}

fn graph_class(
    runtime: &Runtime,
    proposed: Option<&Rc<RegisteredClass>>,
    id: usize,
) -> Option<Rc<RegisteredClass>> {
    if let Some(class) = proposed
        && class_serial(runtime, class.class.get()) == id
    {
        return Some(Rc::clone(class));
    }
    runtime
        .classes
        .borrow()
        .object_declarations
        .get(&id)
        .cloned()
}

fn compute_precedence(
    runtime: &Runtime,
    proposed: Option<&Rc<RegisteredClass>>,
    class: Rc<RootSlot>,
    at: usize,
) -> Result<Option<Vec<Rc<RootSlot>>>, Fault> {
    let mut nodes = HashMap::new();
    let mut pending = vec![class];
    let mut has_forward_references = false;
    while let Some(class) = pending.pop() {
        let info = runtime.heap.class_info(class.get()).expect("class object");
        let id = info.serial;
        if nodes.contains_key(&id) {
            continue;
        }
        let parents = if let Some(class) = graph_class(runtime, proposed, id) {
            class.direct_superclasses.clone()
        } else if let Some(parents) = classes::system_parents(runtime, info.origin_name, at)? {
            resolve_system_classes(runtime, &parents, at)?
        } else {
            has_forward_references = true;
            Vec::new()
        };
        let parent_ids = parents
            .iter()
            .map(|parent| class_serial(runtime, parent.get()))
            .collect();
        nodes.insert(
            id,
            ClassNode {
                class,
                parents: parent_ids,
                dependents: HashSet::new(),
                predecessors: 0,
            },
        );
        pending.extend(parents);
    }

    let edges: Vec<_> = nodes
        .iter()
        .flat_map(|(&id, node)| {
            std::iter::once(id)
                .chain(node.parents.iter().copied())
                .zip(node.parents.iter().copied())
        })
        .collect();
    for (before, after) in edges {
        if nodes.get_mut(&before).unwrap().dependents.insert(after) {
            nodes.get_mut(&after).unwrap().predecessors += 1;
        }
    }
    let mut available: HashSet<_> = nodes
        .iter()
        .filter_map(|(&id, node)| (node.predecessors == 0).then_some(id))
        .collect();
    let mut order: Vec<usize> = Vec::with_capacity(nodes.len());
    while !available.is_empty() {
        let next = if available.len() == 1 {
            available.iter().copied().next()
        } else {
            // CLHS 4.3.5.1 gives priority to the rightmost direct subclass.
            order.iter().rev().find_map(|id| {
                nodes[id]
                    .parents
                    .iter()
                    .copied()
                    .find(|parent| available.contains(parent))
            })
        }
        .ok_or_else(|| precedence_error(at))?;
        order.push(next);
        available.remove(&next);
        let dependents = std::mem::take(&mut nodes.get_mut(&next).unwrap().dependents);
        for dependent in dependents {
            let node = nodes.get_mut(&dependent).unwrap();
            node.predecessors -= 1;
            if node.predecessors == 0 {
                available.insert(dependent);
            }
        }
    }
    if order.len() != nodes.len() {
        return Err(precedence_error(at));
    }
    Ok((!has_forward_references)
        .then(|| order.iter().map(|id| Rc::clone(&nodes[id].class)).collect()))
}

fn class_serial(runtime: &Runtime, class: Word) -> usize {
    runtime.heap.class_info(class).expect("class object").serial
}

fn incomplete(at: usize) -> Fault {
    Fault::plain(
        Class::ProgramError,
        EvalError::new(at, "class has undefined superclasses"),
    )
}

pub(crate) fn class_precedence(
    runtime: &Runtime,
    class: Word,
    at: usize,
) -> Result<Rc<[Rc<RootSlot>]>, Fault> {
    if let Some(definition) = class_definition(runtime, class) {
        return Ok(Rc::clone(&definition.precedence));
    }
    // A finalization hook may already be dispatching on the new class's prototype.
    if let Some(update) = current_update(runtime, class)
        && update.finalized(runtime, class)
        && let Some(definition) = update.definition(runtime, class)
    {
        return Ok(Rc::clone(&definition.precedence));
    }
    let info = runtime.heap.class_info(class).ok_or_else(|| {
        Fault::type_error(runtime, 
            class,
            "CLASS",
            EvalError::new(at, "expected a class object"),
        )
    })?;
    if class_declaration(runtime, class).is_some() {
        return Err(incomplete(at));
    }
    if let Some(precedence) = runtime.classes.borrow().system_precedence.get(&info.serial) {
        return Ok(Rc::clone(precedence));
    }
    let immutable = classes::is_system_class(runtime, info.origin_name);
    let order = compute_precedence(runtime, None, Rc::new(runtime.root_slot(class)), at)?
        .ok_or_else(|| incomplete(at))?;
    let order: Rc<[_]> = order.into();
    if immutable {
        runtime
            .classes
            .borrow_mut()
            .system_precedence
            .insert(info.serial, Rc::clone(&order));
    }
    Ok(order)
}

fn resolve_system_classes(
    runtime: &Runtime,
    names: &[Rc<RootSlot>],
    at: usize,
) -> Result<Vec<Rc<RootSlot>>, Fault> {
    names
        .iter()
        .map(|name| {
            let object = classes::canonical_class(runtime, name.get(), at)?
                .expect("system class name has a class object");
            Ok(Rc::new(runtime.root_slot(object)))
        })
        .collect()
}

fn direct_superclass_allowed(
    runtime: &Runtime,
    proposed: Option<&Rc<RegisteredClass>>,
    kind: RuntimeClassKind,
    class: Word,
) -> bool {
    let info = runtime.heap.class_info(class).expect("superclass object");
    let name = info.origin_name;
    if let Some(superclass) = graph_class(runtime, proposed, info.serial) {
        superclass.declaration.kind == kind
    } else if classes::is_system_class(runtime, name) {
        match kind {
            RuntimeClassKind::Standard => {
                name == Word::T
                    || classes::is_storage_superclass(runtime, class)
                    || condition::named_word(runtime, name, crate::package::Package::COMMON_LISP)
                        .is_some_and(|name| matches!(name.as_str(), "STANDARD-OBJECT" | "STREAM"))
            }
            RuntimeClassKind::Condition => classes::is_condition_class(runtime, name),
        }
    } else {
        matches!(info.kind, ClassKind::ForwardReferenced)
    }
}

fn affected_classes(subclasses: &HashMap<usize, Vec<usize>>, changed: usize) -> Vec<usize> {
    let mut affected = vec![changed];
    let mut known = HashSet::from([changed]);
    let mut index = 0;
    while index < affected.len() {
        for id in subclasses
            .get(&affected[index])
            .into_iter()
            .flatten()
            .copied()
        {
            if known.insert(id) {
                affected.push(id);
            }
        }
        index += 1;
    }
    affected
}

fn effective_definition(
    runtime: &Runtime,
    proposed: Option<&Rc<RegisteredClass>>,
    registered: &RegisteredClass,
    precedence: &[Rc<RootSlot>],
    at: usize,
) -> Result<ClassDefinition, Fault> {
    let mut slots = Vec::<EffectiveSlot>::new();
    let mut indices = HashMap::new();
    let mut instance_slots = 0;
    let mut default_initargs = Vec::new();
    let mut default_names = HashSet::new();
    let mut report = None;
    for ancestor in precedence {
        let info = runtime
            .heap
            .class_info(ancestor.get())
            .expect("precedence class");
        let registered_ancestor = graph_class(runtime, proposed, info.serial);
        let class = registered_ancestor.as_ref();
        let builtins;
        let direct_slots = if let Some(class) = class {
            report = report.or_else(|| class.declaration.report.clone());
            for arg in &class.declaration.default_initargs {
                if default_names.insert(symbol_id(runtime, arg.name.get())) {
                    default_initargs.push(arg.clone());
                }
            }
            class.declaration.slots.as_ref()
        } else if let Some(class) = classes::builtin_condition(runtime, info.origin_name) {
            builtins = condition::builtin_direct_slots(runtime, class, at)?;
            builtins.as_ref()
        } else {
            continue;
        };
        for direct in direct_slots {
            let id = symbol_id(runtime, direct.name.get());
            let index = *indices.entry(id).or_insert_with(|| {
                let allocation = match &direct.allocation {
                    SlotKind::Custom(name) => SlotAllocation::Custom(Rc::clone(name)),
                    SlotKind::Instance => {
                        let index = instance_slots;
                        instance_slots += 1;
                        SlotAllocation::Instance(index)
                    }
                    SlotKind::Class => SlotAllocation::Class(Rc::clone(
                        &class
                            .expect("built-in condition slots are local")
                            .shared_slots
                            .borrow()[&id],
                    )),
                };
                let index = slots.len();
                slots.push(EffectiveSlot {
                    name: Rc::clone(&direct.name),
                    allocation,
                    initargs: Vec::new(),
                    initform: None,
                    initform_source: None,
                    metaobject: Rc::default(),
                    type_constraints: Vec::new(),
                    documentation: None,
                });
                index
            });
            let effective = &mut slots[index];
            if effective.initform.is_none() {
                effective.initform.clone_from(&direct.initform);
                effective
                    .initform_source
                    .clone_from(&direct.initform_source);
            }
            if effective.documentation.is_none() {
                effective.documentation.clone_from(&direct.documentation);
            }
            if let Some(specifier) = &direct.type_specifier {
                effective.type_constraints.push(Rc::clone(specifier));
            }
            for arg in &direct.initargs {
                if !effective
                    .initargs
                    .iter()
                    .any(|added| added.get() == arg.get())
                {
                    effective.initargs.push(Rc::clone(arg));
                }
            }
        }
    }
    Ok(ClassDefinition {
        kind: registered.declaration.kind,
        instance_layout: Rc::new(instances::instance_layout(runtime, &slots, at)?),
        slots: slots.into(),
        slot_indices: indices,
        representation: classes::representation(runtime, precedence, at)?,
        prototype: OnceCell::new(),
        default_initargs,
        instance_slots,
        documentation: registered.declaration.documentation.clone(),
        report,
        precedence: Rc::from(precedence),
    })
}

pub(crate) fn named_target(
    runtime: &Runtime,
    name: Word,
    at: usize,
) -> Result<Option<Word>, Fault> {
    if name == Word::NIL || !runtime.heap.is_symbol(name) {
        return Err(Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "expected a non-NIL class name"),
        ));
    }
    vm::check_definable_name(runtime, name, at)?;
    let name = runtime.root_slot(name);
    let found = classes::find_named_class(runtime, name.get(), at)?;
    if let Some(class) = found {
        let info = runtime.heap.class_info(class).expect("class binding");
        if info.name != name.get() {
            return Ok(None);
        }
        if classes::is_system_class(runtime, info.origin_name)
            || matches!(info.kind, ClassKind::Structure | ClassKind::BuiltIn)
        {
            return Err(Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "cannot redefine a system or structure class"),
            ));
        }
    }
    Ok(found)
}

pub(crate) fn bind_class(runtime: &Runtime, name: Word, class: Word) {
    let id = symbol_id(runtime, name);
    let mut registry = runtime.classes.borrow_mut();
    registry.objects.insert(id, runtime.root_slot(class));
    registry.associations.remove(&id);
    runtime.heap.namespace_changed();
}

/// Reserve a name until the constructor returns and its class is bound.
pub(crate) struct ClassNameClaim<'rt> {
    runtime: &'rt Runtime,
    id: usize,
    name: RootSlot,
}

impl<'rt> ClassNameClaim<'rt> {
    pub(crate) fn acquire(runtime: &'rt Runtime, name: Word, at: usize) -> Result<Self, Fault> {
        let name = runtime.heap.root_for_host(name);
        // An around method can suspend before it calls the null primary method.
        // Check that its name is still available before starting construction.
        if named_target(runtime, name.get(), at)?.is_some() {
            return Err(Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "class name was rebound during initialization"),
            ));
        }
        let id = symbol_id(runtime, name.get());
        if !runtime.classes.borrow_mut().claimed_names.insert(id) {
            return Err(class_name_claimed(at));
        }
        Ok(Self { runtime, id, name })
    }

    pub(crate) fn bind(&self, class: Word) {
        bind_class(self.runtime, self.name.get(), class);
    }
}

impl Drop for ClassNameClaim<'_> {
    fn drop(&mut self) {
        self.runtime
            .classes
            .borrow_mut()
            .claimed_names
            .remove(&self.id);
    }
}

pub(crate) fn check_name_unclaimed(runtime: &Runtime, id: usize, at: usize) -> Result<(), Fault> {
    if runtime.classes.borrow().claimed_names.contains(&id) {
        return Err(class_name_claimed(at));
    }
    Ok(())
}

fn class_name_claimed(at: usize) -> Fault {
    Fault::plain(
        Class::ProgramError,
        EvalError::new(at, "class name is already being defined"),
    )
}

pub(crate) fn declaration_active(runtime: &Runtime, class: Word) -> Option<bool> {
    let id = runtime.heap.class_info(class)?.serial;
    runtime
        .classes
        .borrow()
        .object_declarations
        .get(&id)
        .map(|d| d.active)
}

/// Unresolved parents stay unpublished until the class graph has been checked.
pub(crate) fn resolve_parents(
    runtime: &Runtime,
    parents: &[Root<'_>],
    at: usize,
) -> Result<Vec<Rc<RootSlot>>, Fault> {
    let mut result = Vec::with_capacity(parents.len());
    let mut seen_names = HashSet::new();
    let mut seen_objects = HashSet::new();
    for parent in parents {
        let word = parent.slot.get();
        let parent = if runtime.heap.class_info(word).is_some() {
            word
        } else {
            if !runtime.heap.is_symbol(word)
                || word == Word::NIL
                || !seen_names.insert(symbol_id(runtime, word))
            {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(at, "invalid or duplicate direct superclass"),
                ));
            }
            match classes::find_named_class(runtime, word, at)? {
                Some(class) => class,
                None => classes::allocate_forward_class(runtime, &parent.slot, at)?,
            }
        };
        if !seen_objects.insert(class_serial(runtime, parent)) {
            return Err(Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "duplicate direct superclass"),
            ));
        }
        result.push(Rc::new(runtime.root_slot(parent)));
    }
    Ok(result)
}

pub(crate) struct ClassUpdate {
    registered: Rc<RegisteredClass>,
    previous: Option<Rc<RegisteredClass>>,
    forward_classes: Vec<(usize, Rc<RootSlot>)>,
    affected: Vec<usize>,
    definitions: std::cell::RefCell<HashMap<usize, Option<Rc<ClassDefinition>>>>,
    // Reentrant definitions of unrelated classes may proceed. A changed ancestor
    // or descendant invalidates the prepared layouts before any are published.
    inputs: HashMap<usize, Option<Rc<RegisteredClass>>>,
    finalized: std::cell::RefCell<HashSet<usize>>,
    // Keep new cells private until all metaclass hooks and layout checks succeed.
    computed_shared: std::cell::RefCell<HashMap<(usize, usize), Rc<SharedSlot>>>,
}

pub(crate) fn register_class(
    runtime: &Runtime,
    class: Rc<RootSlot>,
    declaration: ClassDeclaration,
    direct_superclasses: Vec<Rc<RootSlot>>,
    at: usize,
    active: bool,
) -> Result<ClassChanges, Fault> {
    prepare_class(runtime, class, declaration, direct_superclasses, at, active)?
        .publish(runtime, at)
}

pub(crate) fn prepare_class(
    runtime: &Runtime,
    class: Rc<RootSlot>,
    declaration: ClassDeclaration,
    direct_superclasses: Vec<Rc<RootSlot>>,
    at: usize,
    active: bool,
) -> Result<ClassUpdate, Fault> {
    let invalid = |message| Fault::plain(Class::ProgramError, EvalError::new(at, message));
    let serial = class_serial(runtime, class.get());
    let previous = runtime
        .classes
        .borrow()
        .object_declarations
        .get(&serial)
        .cloned();
    if previous
        .as_ref()
        .is_some_and(|previous| previous.declaration.kind != declaration.kind)
    {
        return Err(invalid(
            "cannot redefine a standard class as a condition or vice versa",
        ));
    }
    let mut forward_classes = Vec::new();
    for parent in &direct_superclasses {
        let info = runtime
            .heap
            .class_info(parent.get())
            .expect("resolved parent");
        if matches!(info.kind, ClassKind::ForwardReferenced) {
            if info.name == declaration.name.get() {
                return Err(precedence_error(at));
            }
            let id = symbol_id(runtime, info.origin_name);
            match classes::find_named_class(runtime, info.origin_name, at)? {
                None => forward_classes.push((id, Rc::clone(parent))),
                Some(bound) if bound != parent.get() => {
                    return Err(invalid(
                        "forward superclass was redefined during class initialization",
                    ));
                }
                _ => {}
            }
        }
    }
    let mut shared_slots = HashMap::new();
    for slot in declaration.slots.iter() {
        if matches!(slot.allocation, SlotKind::Class) {
            let slot_id = symbol_id(runtime, slot.name.get());
            let cell = match previous
                .as_ref()
                .and_then(|c| c.shared_slots.borrow().get(&slot_id).cloned())
            {
                Some(shared) => shared,
                None => SharedSlot::new(runtime, &class, at)?,
            };
            shared_slots.insert(slot_id, cell);
        }
    }
    let retained_accessors = previous
        .as_ref()
        .filter(|p| Rc::ptr_eq(&p.declaration.slots, &declaration.slots));
    let accessors = retained_accessors
        .map(|p| Rc::clone(&p.accessors))
        .unwrap_or_default();
    let registered = Rc::new(RegisteredClass {
        class: Rc::clone(&class),
        direct_superclasses,
        declaration: Rc::new(declaration),
        active,
        shared_slots: shared_slots.into(),
        accessors,
    });
    let affected = affected_classes(&runtime.classes.borrow().subclasses, serial);
    let mut definitions = HashMap::new();
    let mut inputs = HashMap::new();
    for &id in &affected {
        let candidate = graph_class(runtime, Some(&registered), id).expect("affected class");
        if candidate.direct_superclasses.iter().any(|parent| {
            !direct_superclass_allowed(
                runtime,
                Some(&registered),
                candidate.declaration.kind,
                parent.get(),
            )
        }) {
            return Err(invalid("incompatible superclass for class metaclass"));
        }
        let precedence =
            compute_precedence(runtime, Some(&registered), Rc::clone(&candidate.class), at)?;
        let definition = if let Some(precedence) = precedence {
            for parent in &precedence {
                let id = class_serial(runtime, parent.get());
                inputs
                    .entry(id)
                    .or_insert_with(|| graph_class(runtime, None, id));
            }
            Some(Rc::new(effective_definition(
                runtime,
                Some(&registered),
                &candidate,
                &precedence,
                at,
            )?))
        } else {
            let mut pending = vec![id];
            while let Some(ancestor) = pending.pop() {
                if let std::collections::hash_map::Entry::Vacant(input) = inputs.entry(ancestor) {
                    input.insert(graph_class(runtime, None, ancestor));
                    if let Some(node) = graph_class(runtime, Some(&registered), ancestor) {
                        pending.extend(
                            node.direct_superclasses
                                .iter()
                                .map(|c| class_serial(runtime, c.get())),
                        );
                    }
                }
            }
            None
        };
        definitions.insert(id, definition);
    }
    Ok(ClassUpdate {
        registered,
        previous,
        forward_classes,
        affected,
        definitions: definitions.into(),
        inputs,
        finalized: Default::default(),
        computed_shared: Default::default(),
    })
}

impl ClassUpdate {
    fn subclass_change(&self, parent: Word) -> Option<bool> {
        let has_parent =
            |class: &RegisteredClass| class.direct_superclasses.iter().any(|p| p.get() == parent);
        let old = self.previous.as_ref().is_some_and(|c| has_parent(c));
        let new = has_parent(&self.registered);
        (old != new).then_some(new)
    }

    pub(crate) fn publish(&self, runtime: &Runtime, at: usize) -> Result<ClassChanges, Fault> {
        let registered = &self.registered;
        let class = &registered.class;
        let serial = class_serial(runtime, class.get());
        let active = registered.active;
        {
            let registry = runtime.classes.borrow();
            let changed = self.inputs.iter().any(|(id, before)| {
                match (before, registry.object_declarations.get(id)) {
                    (Some(a), Some(b)) => !Rc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                }
            });
            if changed || self.affected != affected_classes(&registry.subclasses, serial) {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(at, "class hierarchy changed during finalization"),
                ));
            }
        }
        for (id, parent) in &self.forward_classes {
            check_name_unclaimed(runtime, *id, at)?;
            let origin_name = runtime
                .heap
                .class_info(parent.get())
                .expect("forward class")
                .origin_name;
            debug_assert_eq!(*id, symbol_id(runtime, origin_name));
            if classes::find_named_class(runtime, origin_name, at)?
                .is_some_and(|bound| bound != parent.get())
            {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(at, "forward superclass name changed during finalization"),
                ));
            }
        }
        let definitions = self.definitions.borrow();
        for (&id, candidate) in definitions.iter() {
            let Some(definition) = candidate else {
                continue;
            };
            let object = definition.precedence[0].get();
            if class_definition(runtime, object)
                .is_some_and(|old| old.representation != definition.representation)
            {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(
                        at,
                        "cannot change the instance representation of a defined class",
                    ),
                ));
            }
            if let Some(meta) = runtime.heap.instance_class(object)
                && let Some(meta) = class_definition(runtime, meta)
                && (meta.representation == Representation::FuncallableStandardClass)
                    != matches!(
                        definition.representation,
                        Representation::FuncallableInstance | Representation::GenericFunction
                    )
            {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(
                        at,
                        "the metaclass and function superclass require different instance representations",
                    ),
                ));
            }
            debug_assert_eq!(class_serial(runtime, object), id);
        }
        if active
            && self
                .previous
                .as_ref()
                .is_none_or(|old| !Rc::ptr_eq(&old.accessors, &registered.accessors))
        {
            vm::class_definition::validate_accessors(runtime, &registered.declaration.slots, at)?;
        }
        let computed_shared = self.computed_shared.borrow();
        for (&(owner, name), cell) in computed_shared.iter() {
            if owner != serial
                && graph_class(runtime, None, owner)
                    .and_then(|c| c.shared_slots.borrow().get(&name).cloned())
                    .is_some_and(|other| !Rc::ptr_eq(&other, cell))
            {
                return Err(Fault::plain(
                    Class::ProgramError,
                    EvalError::new(at, "shared slot changed during finalization"),
                ));
            }
        }
        let mut removed_accessors = Vec::new();
        if let Some(previous) = self
            .previous
            .as_ref()
            .filter(|old| !Rc::ptr_eq(&old.accessors, &registered.accessors))
        {
            for (generic, method) in previous.accessors.borrow().iter() {
                let info = runtime
                    .heap
                    .method_info(method.get())
                    .expect("generated accessor");
                if info.owner == generic.get() {
                    removed_accessors
                        .push((runtime.root_slot(generic.get()), runtime.root_slot(method.get())));
                }
            }
        }
        {
            if matches!(
                runtime.heap.class_info(class.get()).unwrap().kind,
                ClassKind::ForwardReferenced
            ) {
                runtime
                    .heap
                    .resolve_forward_class(class.get(), ClassKind::Standard);
            }
            runtime
                .heap
                .set_class_name(class.get(), registered.declaration.name.get());
            let mut registry = runtime.classes.borrow_mut();
            for (id, parent) in &self.forward_classes {
                registry.objects.insert(*id, runtime.root_slot(parent.get()));
                registry.associations.remove(id);
            }
            // An explicit reverse-link edit only describes the previous edge.
            // Keep edits on relationships untouched by this redefinition.
            for parent in self
                .previous
                .iter()
                .flat_map(|c| &c.direct_superclasses)
                .chain(&registered.direct_superclasses)
            {
                if self.subclass_change(parent.get()).is_some() {
                    let id = class_serial(runtime, parent.get());
                    if let Some(edits) = registry.subclass_edits.get_mut(&id) {
                        edits.remove(&serial);
                        if edits.is_empty() {
                            registry.subclass_edits.remove(&id);
                        }
                    }
                }
            }
            if let Some(old) = &self.previous {
                for parent in &old.direct_superclasses {
                    let parent = class_serial(runtime, parent.get());
                    if let Some(children) = registry.subclasses.get_mut(&parent) {
                        children.retain(|&child| child != serial);
                        if children.is_empty() {
                            registry.subclasses.remove(&parent);
                        }
                    }
                }
            }
            for parent in &registered.direct_superclasses {
                registry
                    .subclasses
                    .entry(class_serial(runtime, parent.get()))
                    .or_default()
                    .push(serial);
            }
            registry
                .object_declarations
                .insert(serial, Rc::clone(registered));

            for (&(owner, name), cell) in computed_shared.iter() {
                registry.object_declarations[&owner]
                    .shared_slots
                    .borrow_mut()
                    .insert(name, Rc::clone(cell));
            }

            for (id, definition) in definitions.iter() {
                registry.prototypes.remove(id);
                match definition {
                    Some(definition) => {
                        registry
                            .object_definitions
                            .insert(*id, Rc::clone(definition));
                    }
                    None => {
                        registry.object_definitions.remove(id);
                    }
                }
            }
        }
        let mut initializations = Vec::new();
        if !active {
            return Ok(ClassChanges {
                shared: initializations,
                removed_accessors,
            });
        }
        let mut scheduled = HashSet::new();
        for id in &self.affected {
            let definition = &definitions[id];
            let class = graph_class(runtime, None, *id).expect("published class");
            if let Some(definition) = definition
                && class.active
            {
                definition.collect_shared_initializers(
                    runtime,
                    &mut scheduled,
                    &mut initializations,
                );
            }
        }
        Ok(ClassChanges {
            shared: initializations,
            removed_accessors,
        })
    }
}
mod finalization;
pub(crate) use finalization::{
    TaskClassUpdates, begin_update, computed_precedence, current_update, end_update, finish_update,
    inline_update_scope,
};
