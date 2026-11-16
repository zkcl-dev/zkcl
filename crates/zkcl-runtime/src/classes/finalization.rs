use std::mem::DropGuard;

use super::*;

/// Proposed class definitions move with the task while its Lisp hooks suspend.
#[derive(Default)]
pub(crate) struct TaskClassUpdates(Vec<Rc<ClassUpdate>>);

impl TaskClassUpdates {
    pub(crate) fn activate<'a>(&'a mut self, runtime: &'a Runtime) -> impl Drop + 'a {
        fn swap((updates, runtime): (&mut TaskClassUpdates, &Runtime)) {
            std::mem::swap(&mut updates.0, &mut runtime.classes.borrow_mut().pending);
        }

        swap((self, runtime));
        DropGuard::new((self, runtime), swap)
    }
}

/// Synchronous reentry sees the calling task's proposed definitions. If the
/// VM exits without running Lisp cleanups, discard only updates it added.
pub(crate) fn inline_update_scope(runtime: &Runtime) -> impl Drop + '_ {
    let depth = runtime.classes.borrow().pending.len();
    DropGuard::new(depth, |depth| {
        runtime.classes.borrow_mut().pending.truncate(depth);
    })
}

fn invalid(at: usize, message: &str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

pub(crate) fn begin_update(
    runtime: &Runtime,
    update: ClassUpdate,
    at: usize,
) -> Result<(Word, Word), Fault> {
    // Complete parents before descendants, including diamonds whose paths have unequal lengths.
    let mut order: Vec<_> = update
        .definitions
        .borrow()
        .values()
        .flatten()
        .map(|d| (d.precedence.len(), Rc::clone(&d.precedence[0])))
        .collect();
    order.sort_by_key(|(depth, class)| (*depth, class_serial(runtime, class.get())));
    let classes =
        vm::methods::list_from_words(runtime, order.iter().map(|(_, c)| c.get()).collect(), at)?;
    let mut registry = runtime.classes.borrow_mut();
    let token = Word::fixnum(registry.pending.len() as isize).expect("VM nesting depth");
    registry.pending.push(Rc::new(update));
    Ok((token, classes))
}

pub(crate) fn current_update(runtime: &Runtime, class: Word) -> Option<Rc<ClassUpdate>> {
    let id = runtime.heap.class_info(class)?.serial;
    runtime
        .classes
        .borrow()
        .pending
        .iter()
        .rev()
        .find(|u| u.definitions.borrow().contains_key(&id))
        .cloned()
}

pub(crate) fn finish_update(
    runtime: &Runtime,
    token: Word,
    at: usize,
) -> Result<ClassChanges, Fault> {
    let update = {
        let registry = runtime.classes.borrow();
        registry
            .pending
            .last()
            .filter(|_| token.as_fixnum() == Some(registry.pending.len() as isize - 1))
            .cloned()
            .ok_or_else(|| invalid(at, "no matching class finalization"))?
    };
    {
        let finalized = update.finalized.borrow();
        if update
            .definitions
            .borrow()
            .iter()
            .any(|(id, def)| def.is_some() && !finalized.contains(id))
        {
            return Err(invalid(
                at,
                "FINALIZE-INHERITANCE returned without finalizing the class",
            ));
        }
    }
    update.publish(runtime, at)
}

pub(crate) fn end_update(runtime: &Runtime, token: Word, at: usize) -> Result<(), Fault> {
    let mut registry = runtime.classes.borrow_mut();
    if registry.pending.is_empty() || token.as_fixnum() != Some(registry.pending.len() as isize - 1)
    {
        return Err(invalid(at, "no matching class finalization"));
    }
    registry.pending.pop();
    Ok(())
}

pub(crate) fn computed_precedence(
    runtime: &Runtime,
    class: Word,
    at: usize,
) -> Result<Vec<Rc<RootSlot>>, Fault> {
    let update = current_update(runtime, class);
    compute_precedence(
        runtime,
        update.as_ref().map(|p| &p.registered),
        Rc::new(runtime.root_slot(class)),
        at,
    )?
    .ok_or_else(|| incomplete(at))
}

impl ClassUpdate {
    pub(crate) fn finalized(&self, runtime: &Runtime, class: Word) -> bool {
        self.finalized
            .borrow()
            .contains(&class_serial(runtime, class))
    }

    pub(crate) fn definition(&self, runtime: &Runtime, class: Word) -> Option<Rc<ClassDefinition>> {
        self.definitions
            .borrow()
            .get(&class_serial(runtime, class))?
            .clone()
    }

    pub(crate) fn direct(&self, class: Word) -> Option<(&ClassDeclaration, &[Rc<RootSlot>])> {
        (self.registered.class.get() == class).then_some((
            &self.registered.declaration,
            &self.registered.direct_superclasses,
        ))
    }

    pub(crate) fn set_precedence(
        &self,
        runtime: &Runtime,
        class: Word,
        order: Vec<Word>,
        at: usize,
    ) -> Result<(), Fault> {
        let id = class_serial(runtime, class);
        let previous = self
            .definition(runtime, class)
            .ok_or_else(|| incomplete(at))?;
        if order
            .iter()
            .copied()
            .eq(previous.precedence.iter().map(|c| c.get()))
        {
            return Ok(());
        }
        let classes: HashSet<_> = previous.precedence.iter().map(|c| c.get()).collect();
        let supplied: HashSet<_> = order.iter().copied().collect();
        if order.len() != classes.len()
            || supplied != classes
            || order.first() != Some(&class)
            || order.last().copied() != previous.precedence.last().map(|c| c.get())
        {
            return Err(invalid(at, "invalid computed class precedence list"));
        }
        let order: Vec<_> = order
            .into_iter()
            .map(|w| Rc::new(runtime.root_slot(w)))
            .collect();
        let registered = graph_class(runtime, Some(&self.registered), id).expect("pending class");
        let effective =
            effective_definition(runtime, Some(&self.registered), &registered, &order, at)?;
        self.definitions
            .borrow_mut()
            .insert(id, Some(Rc::new(effective)));
        Ok(())
    }

    fn shared_for_effective_slot(
        &self,
        runtime: &Runtime,
        layout: &ClassDefinition,
        name: Word,
        at: usize,
    ) -> Result<Rc<SharedSlot>, Fault> {
        if let Some(SlotAllocation::Class(cell)) = layout.slot(runtime, name).map(|s| &s.allocation)
        {
            return Ok(Rc::clone(cell));
        }
        // A changed allocation follows the most specific direct definition. A
        // slot introduced by COMPUTE-SLOTS belongs to the class being finalized.
        let owner = layout
            .precedence
            .iter()
            .find(|ancestor| {
                graph_class(
                    runtime,
                    Some(&self.registered),
                    class_serial(runtime, ancestor.get()),
                )
                .is_some_and(|c| c.declaration.slots.iter().any(|s| s.name.get() == name))
            })
            .unwrap_or(&layout.precedence[0]);
        let key = (class_serial(runtime, owner.get()), symbol_id(runtime, name));
        let mut staged = self.computed_shared.borrow_mut();
        if let Some(cell) = staged.get(&key) {
            return Ok(Rc::clone(cell));
        }
        let existing = graph_class(runtime, None, key.0)
            .and_then(|c| c.shared_slots.borrow().get(&key.1).cloned());
        let cell = match existing {
            Some(cell) => cell,
            None => SharedSlot::new(runtime, owner, at)?,
        };
        staged.insert(key, Rc::clone(&cell));
        Ok(cell)
    }

    pub(crate) fn finish_layout(
        &self,
        runtime: &Runtime,
        class: Word,
        slots: &[Root<'_>],
        defaults: Vec<DefaultInitarg>,
        at: usize,
    ) -> Result<Vec<SharedInitialization>, Fault> {
        let id = class_serial(runtime, class);
        let old = self
            .definition(runtime, class)
            .ok_or_else(|| incomplete(at))?;
        let same_order = slots.len() == old.slots.len()
            && slots.iter().zip(old.slots.iter()).all(|(meta, slot)| {
                slot.metaobject
                    .get()
                    .is_some_and(|cached| cached.get() == meta.slot.get())
            });
        let mut new = (*old).clone();
        new.default_initargs = defaults;
        if !same_order {
            let mut ordered = Vec::with_capacity(slots.len());
            let mut indices = HashMap::new();
            let mut local = 0;
            let mut locations = vec![false; slots.len()];
            for meta in slots {
                let word = meta.slot.get();
                let info = runtime
                    .heap
                    .slot_definition_info(word)
                    .filter(|s| !s.direct && runtime.heap.is_variable(s.name))
                    .ok_or_else(|| {
                        invalid(at, "expected an initialized effective slot definition")
                    })?;
                if runtime
                    .heap
                    .slot_location_owner(word)
                    .is_some_and(|owner| owner != Word::NIL)
                {
                    return Err(invalid(
                        at,
                        "computed slot already belongs to a finalized layout",
                    ));
                }
                if indices
                    .insert(symbol_id(runtime, info.name), ordered.len())
                    .is_some()
                {
                    return Err(invalid(at, "duplicate effective slot"));
                }
                let allocation = match condition::named_word(
                    runtime,
                    info.allocation,
                    crate::package::Package::KEYWORD,
                )
                .as_deref()
                {
                    Some("CLASS") => SlotAllocation::Class(
                        self.shared_for_effective_slot(runtime, &old, info.name, at)?,
                    ),
                    Some("INSTANCE") => {
                        let index = info
                            .location
                            .as_fixnum()
                            .and_then(|n| usize::try_from(n).ok())
                            .filter(|&index| index < slots.len() && !locations[index])
                            .ok_or_else(|| {
                                invalid(at, "invalid or duplicate computed instance slot location")
                            })?;
                        locations[index] = true;
                        local += 1;
                        SlotAllocation::Instance(index)
                    }
                    _ => SlotAllocation::Custom(Rc::new(runtime.root_slot(info.allocation))),
                };
                // Allocating a new shared cell can move the slot metaobject.
                let word = meta.slot.get();
                let info = runtime
                    .heap
                    .slot_definition_info(word)
                    .expect("validated effective slot");
                let root = |w| Rc::new(runtime.root_slot(w));
                ordered.push(EffectiveSlot {
                    name: root(info.name),
                    allocation,
                    initargs: vm::proper_list(runtime, info.initargs, at)?
                        .into_iter()
                        .map(root)
                        .collect(),
                    initform: (info.initfunction != Word::NIL).then(|| root(info.initfunction)),
                    initform_source: (info.initfunction != Word::NIL).then(|| root(info.initform)),
                    metaobject: Rc::new(OnceCell::from(runtime.root_slot(word))),
                    type_constraints: if info.type_specifier == Word::T {
                        Vec::new()
                    } else {
                        vec![root(info.type_specifier)]
                    },
                    documentation: (info.documentation != Word::NIL)
                        .then(|| root(info.documentation)),
                });
            }
            if !locations[..local].iter().all(|&used| used) {
                return Err(invalid(at, "incomplete computed instance slot locations"));
            }
            new.instance_layout = Rc::new(instances::instance_layout(runtime, &ordered, at)?);
            new.instance_slots = local;
            new.slot_indices = indices;
            new.slots = ordered.into();
        }
        let owner = new.precedence[0].get();
        for slot in new.slots.iter() {
            let location = match slot.allocation {
                SlotAllocation::Instance(i) => Word::fixnum(i as isize).unwrap(),
                SlotAllocation::Class(_) | SlotAllocation::Custom(_) => Word::NIL,
            };
            runtime.heap.set_slot_location(
                slot.metaobject
                    .get()
                    .expect("computed slot metaobject")
                    .get(),
                owner,
                location,
            );
        }
        let mut initializers = Vec::new();
        if self.registered.active {
            new.collect_shared_initializers(runtime, &mut HashSet::new(), &mut initializers);
        }
        self.definitions.borrow_mut().insert(id, Some(Rc::new(new)));
        self.finalized.borrow_mut().insert(id);
        Ok(initializers)
    }
}
