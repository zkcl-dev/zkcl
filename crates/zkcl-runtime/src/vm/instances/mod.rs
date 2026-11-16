use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use crate::condition::{self, Class, Fault};
use crate::heap::{ClassKind, RootFrame, RootSlot, RootSlots, Word};
use crate::symbol::{Name, Package};
use crate::{EvalError, Runtime, compile};

use super::classes::{self, find_named_class};
use super::generic::{self, InitializationArgument};
use super::{
    ExecState, FunctionName, Invocation, Primitive, Resume, Signal, StepControl, Values, accept,
    dispatch_error, finish, invoke, proper_list,
};
use crate::classes::{ClassDefinition, EffectiveSlot, SlotAllocation, class_definition};

mod bootstrap;
pub(super) use bootstrap::{compile_builtin_lambda, initialize_slot_protocol};
pub(crate) use bootstrap::{
    initialize_documentation_protocol, initialize_function_keywords, initialize_load_forms,
};

#[derive(Clone, Copy)]
pub(crate) enum Op {
    DefaultInitargs,
    SharedInitialize,
    ChangeClassMigrate,
    AddedSlotNames,
    UpdateObsolete,
    Slot(SlotOp),
    SlotUsingClass(SlotOp),
    UnboundSlotInstance,
    ValidateReinitializeInitargs,
    ValidateChangeClassInitargs,
    ValidateRedefinedInitargs,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotOp {
    Value,
    SetValue,
    Boundp,
    ExistsP,
    Makunbound,
}

pub(super) const PRIMITIVES: &[(&str, Op)] = &[
    ("%DEFAULT-INITARGS", Op::DefaultInitargs),
    ("%SHARED-INITIALIZE", Op::SharedInitialize),
    ("%CHANGE-CLASS-MIGRATE", Op::ChangeClassMigrate),
    ("%ADDED-SLOT-NAMES", Op::AddedSlotNames),
    ("%UPDATE-OBSOLETE-INSTANCE", Op::UpdateObsolete),
    ("SLOT-VALUE", Op::Slot(SlotOp::Value)),
    ("(SETF SLOT-VALUE)", Op::Slot(SlotOp::SetValue)),
    ("SLOT-BOUNDP", Op::Slot(SlotOp::Boundp)),
    ("SLOT-EXISTS-P", Op::Slot(SlotOp::ExistsP)),
    ("SLOT-MAKUNBOUND", Op::Slot(SlotOp::Makunbound)),
    ("UNBOUND-SLOT-INSTANCE", Op::UnboundSlotInstance),
    (
        "%VALIDATE-REINITIALIZE-INITARGS",
        Op::ValidateReinitializeInitargs,
    ),
    (
        "%VALIDATE-CHANGE-CLASS-INITARGS",
        Op::ValidateChangeClassInitargs,
    ),
    (
        "%VALIDATE-REDEFINED-INITARGS",
        Op::ValidateRedefinedInitargs,
    ),
    ("%SLOT-VALUE-USING-CLASS", Op::SlotUsingClass(SlotOp::Value)),
    (
        "%SET-SLOT-VALUE-USING-CLASS",
        Op::SlotUsingClass(SlotOp::SetValue),
    ),
    (
        "%SLOT-BOUNDP-USING-CLASS",
        Op::SlotUsingClass(SlotOp::Boundp),
    ),
    (
        "%SLOT-MAKUNBOUND-USING-CLASS",
        Op::SlotUsingClass(SlotOp::Makunbound),
    ),
];

impl Op {
    pub(super) fn accepts(self, count: usize) -> bool {
        match self {
            Self::DefaultInitargs | Self::ValidateReinitializeInitargs => count >= 1,
            Self::ValidateChangeClassInitargs => count >= 3,
            Self::ValidateRedefinedInitargs => count >= 4,
            Self::SharedInitialize => count >= 2,
            Self::ChangeClassMigrate | Self::AddedSlotNames => count == 2,
            Self::Slot(op) => count == 2 + usize::from(op.setter()),
            Self::SlotUsingClass(op) => count == 5 + usize::from(op.setter()),
            Self::UnboundSlotInstance | Self::UpdateObsolete => count == 1,
        }
    }
}

impl SlotOp {
    fn setter(self) -> bool {
        matches!(self, Self::SetValue)
    }

    pub(super) fn binding(self, runtime: &Runtime) -> (Word, FunctionName) {
        let symbol = runtime
            .packages
            .borrow()
            .present(Package::MOP, self.protocol_name())
            .expect("installed slot protocol symbol")
            .0;
        let binding = if self.setter() {
            FunctionName::Setf(symbol)
        } else {
            FunctionName::Symbol(symbol)
        };
        (symbol, binding)
    }

    fn protocol_name(self) -> &'static str {
        match self {
            Self::Value | Self::SetValue => "SLOT-VALUE-USING-CLASS",
            Self::Boundp => "SLOT-BOUNDP-USING-CLASS",
            Self::Makunbound => "SLOT-MAKUNBOUND-USING-CLASS",
            Self::ExistsP => unreachable!("SLOT-EXISTS-P looks up the slot by name"),
        }
    }
}

pub(crate) fn setf_primitive(accessor: &str) -> Option<Word> {
    (accessor == "SLOT-VALUE")
        .then(|| Primitive::named("(SETF SLOT-VALUE)"))
        .flatten()
}

pub(super) fn allocate_instance(
    runtime: &Runtime,
    class: &RootSlot,
    name: Word,
    at: usize,
) -> Result<Word, Fault> {
    let Some(info) = runtime.heap.class_info(class.get()) else {
        return Err(Fault::type_error(runtime, 
            class.get(),
            "CLASS",
            EvalError::new(at, "expected a class object"),
        ));
    };
    if matches!(info.kind, ClassKind::Structure) {
        let definition = runtime
            .structure_definition(info.origin_name)
            .ok_or_else(|| error(at, "unknown structure type"))?;
        if matches!(
            definition.variant,
            Some(super::structures::VariantRole::Family(_))
        ) {
            return Err(error(at, "a variant family requires a constructor"));
        }
        let name = runtime.root_slot(info.origin_name);
        return runtime
            .heap
            .structure(definition.slots.len(), || name.get(), |_| Word::UNBOUND)
            .map_err(|error| EvalError::out_of_memory(at, error).into());
    }
    InitializationLayout::for_class(runtime, class.get(), at)?.allocate(runtime, class, name, at)
}

pub(super) fn allocate_standard_instance(
    runtime: &Runtime,
    class: &RootSlot,
    definition: &ClassDefinition,
    name: Word,
    at: usize,
) -> Result<Word, Fault> {
    use crate::classes::Representation;
    match definition.representation {
        Representation::StandardClass
        | Representation::FuncallableStandardClass
        | Representation::ForwardReferencedClass => {
            if !runtime.heap.is_symbol(name) {
                return Err(Fault::type_error(runtime, 
                    name,
                    "SYMBOL",
                    EvalError::new(at, "expected a class name"),
                ));
            }
            let object = runtime.root(super::classes::allocate_class_object(
                runtime,
                name,
                if definition.representation == Representation::ForwardReferencedClass {
                    ClassKind::ForwardReferenced
                } else {
                    ClassKind::Standard
                },
                at,
            )?);
            super::classes::attach_metaclass(runtime, &object.slot, class, at)?;
            Ok(object.slot.get())
        }
        Representation::DirectSlot | Representation::EffectiveSlot => {
            let allocation = runtime.root_slot(runtime.intern(&Name::keyword("INSTANCE"), at)?);
            let slot = runtime.root(
                runtime
                    .heap
                    .slot_definition(|| {
                        crate::heap::SlotFields::empty(
                            definition.representation == Representation::DirectSlot,
                            allocation.get(),
                        )
                    })
                    .map_err(|e| EvalError::out_of_memory(at, e))?,
            );
            let instance = runtime
                .heap
                .instance(class, &definition.instance_layout)
                .map_err(|e| EvalError::out_of_memory(at, e))?;
            runtime
                .heap
                .set_slot_meta_instance(slot.slot.get(), instance);
            Ok(slot.slot.get())
        }
        Representation::GenericFunction | Representation::Method | Representation::Specializer => {
            let payload = runtime.root_slot(
                runtime
                    .heap
                    .instance(class, &definition.instance_layout)
                    .map_err(|err| EvalError::out_of_memory(at, err))?,
            );
            let meta = match definition.representation {
                Representation::GenericFunction => {
                    let name = runtime.intern(&Name::common_lisp("STANDARD"), at)?;
                    let spec =
                        runtime.root_slot(super::methods::list_from_words(runtime, vec![name], at)?);
                    runtime
                        .heap
                        .generic_function(|| crate::heap::GenericFields::uninitialized(spec.get()))
                }
                Representation::Method => runtime
                    .heap
                    .method(crate::heap::MethodFields::uninitialized),
                Representation::Specializer => runtime.heap.specializer(|| Word::UNBOUND),
                _ => unreachable!(),
            }
            .map_err(|err| EvalError::out_of_memory(at, err))?;
            runtime
                .heap
                .set_additional_meta_instance(meta, payload.get());
            Ok(meta)
        }
        Representation::FuncallableInstance => runtime
            .heap
            .funcallable_instance(class, &definition.instance_layout),
        Representation::Instance => runtime.heap.instance(class, &definition.instance_layout),
    }
    .map_err(|error| EvalError::out_of_memory(at, error).into())
}

pub(crate) fn instance_layout(
    runtime: &Runtime,
    slots: &[EffectiveSlot],
    at: usize,
) -> Result<RootSlot, Fault> {
    let mut ordered: Vec<_> = slots
        .iter()
        .filter(|s| !matches!(s.allocation, SlotAllocation::Custom(_)))
        .collect();
    ordered.sort_by_key(|slot| match slot.allocation {
        SlotAllocation::Instance(index) => index,
        SlotAllocation::Class(_) | SlotAllocation::Custom(_) => usize::MAX,
    });
    let local = ordered
        .iter()
        .take_while(|slot| matches!(slot.allocation, SlotAllocation::Instance(_)))
        .count();
    runtime
        .heap
        .instance_layout(ordered.len(), local, |index| {
            let slot = ordered[index];
            let cell = match &slot.allocation {
                SlotAllocation::Instance(_) => None,
                SlotAllocation::Class(shared) => Some(shared.cell.get()),
                SlotAllocation::Custom(_) => {
                    unreachable!("custom allocations have no standard storage")
                }
            };
            (slot.name.get(), cell)
        })
        .map(|layout| runtime.root_slot(layout))
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

pub(super) fn start<'rt>(
    runtime: &'rt Runtime,
    frame: &RootSlots,
    op: Op,
    args: Range<usize>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let get = |index| frame.get(args.start + index);
    match op {
        Op::DefaultInitargs => {
            let class = class_designator(runtime, get(0), at)?;
            Initialization::for_default_initargs(
                runtime,
                class,
                frame,
                args.start + 1..args.end,
                resume,
                at,
            )
            .map(Invocation::Initialization)
        }
        Op::SharedInitialize => {
            let instance = get(0);
            Initialization::for_shared_initialize(
                runtime,
                instance,
                args.start + 1,
                frame,
                args.start + 2..args.end,
                resume,
                at,
            )
            .map(Invocation::Initialization)
        }
        Op::ValidateReinitializeInitargs
        | Op::ValidateChangeClassInitargs
        | Op::ValidateRedefinedInitargs => {
            validate_update_initargs(runtime, op, frame, args, at)?;
            values.one(Word::NIL);
            Ok(Invocation::Done(resume))
        }
        Op::ChangeClassMigrate => {
            let previous = change_class_migrate(runtime, get(0), get(1), at)?;
            values.one(previous);
            Ok(Invocation::Done(resume))
        }
        Op::AddedSlotNames => {
            let added = added_slot_names(runtime, get(0), get(1), at)?;
            values.one(added);
            Ok(Invocation::Done(resume))
        }
        Op::UnboundSlotInstance => {
            values.one(crate::condition::builtin_slot(
                runtime,
                get(0),
                Class::UnboundSlot,
                true,
                at,
            )?);
            Ok(Invocation::Done(resume))
        }
        Op::Slot(op) => {
            let base = usize::from(op.setter());
            let name = get(base + 1);
            if !runtime.heap.is_symbol(name) {
                return Err(Fault::type_error(runtime, 
                    name,
                    "SYMBOL",
                    EvalError::new(at, "expected a slot name symbol"),
                ));
            }
            let object = get(base);
            let value = op.setter().then(|| get(0));
            if let Some(result) = current_standard_slot(runtime, op, object, name, value, || true) {
                values.one(result);
                return Ok(Invocation::Done(resume));
            }
            Ok(Access::start(
                runtime,
                AccessTarget::Named(op, runtime.root_slot(name)),
                object,
                value,
                resume,
                at,
            ))
        }
        Op::SlotUsingClass(op) => {
            // Method functions receive the next-method function and flag first.
            let base = 2 + usize::from(op.setter());
            Ok(Access::start(
                runtime,
                AccessTarget::Effective {
                    op,
                    class: runtime.root_slot(get(base)),
                    slot: runtime.root_slot(get(base + 2)),
                },
                get(base + 1),
                op.setter().then(|| get(2)),
                resume,
                at,
            ))
        }
        Op::UpdateObsolete => Ok(Access::start(
            runtime,
            AccessTarget::Update,
            get(0),
            None,
            resume,
            at,
        )),
    }
}

/// A snapshot used only while transferring storage, before the next Lisp allocation.
struct PreviousSlots(HashMap<Word, (bool, Word)>);

impl PreviousSlots {
    fn read(runtime: &Runtime, object: Word, layout: Word) -> Self {
        let entries = runtime
            .heap
            .instance_layout_entries(layout)
            .expect("instance layout");
        Self(
            entries
                .into_iter()
                .enumerate()
                .map(|(index, (name, cell))| {
                    let value = match cell {
                        Some(cell) => runtime.heap.read_cell(cell),
                        None => runtime.heap.instance_slot(object, index),
                    }
                    .expect("existing slot storage");
                    (name, (cell.is_none(), value))
                })
                .collect(),
        )
    }

    /// A new local slot inherits even an unbound or formerly shared value.
    /// After the transfer, unmatched old local slots are available to the update method.
    fn transfer_locals(
        &mut self,
        runtime: &Runtime,
        destination: &ClassDefinition,
        mut added: impl FnMut(Word),
    ) -> RootFrame {
        let values = runtime.root_frame(destination.instance_slots);
        for slot in destination.slots.iter() {
            if let SlotAllocation::Instance(index) = slot.allocation {
                let name = slot.name.get();
                let value = match self.0.remove(&name) {
                    Some((_, value)) => value,
                    None => {
                        added(name);
                        Word::UNBOUND
                    }
                };
                values.set(index, value);
            }
        }
        values
    }

    fn discarded_locals(self) -> impl Iterator<Item = (Word, Word)> {
        self.0
            .into_iter()
            .filter_map(|(name, (local, value))| local.then_some((name, value)))
    }
}

fn change_class_migrate(
    runtime: &Runtime,
    instance_word: Word,
    new_class_word: Word,
    at: usize,
) -> Result<Word, Fault> {
    let instance_slot = runtime.root_slot(instance_word);
    let new_class_slot = runtime.root_slot(new_class_word);
    let Some(old_class) = runtime.heap.instance_class(instance_slot.get()) else {
        return Err(Fault::type_error(runtime, 
            instance_slot.get(),
            "STANDARD-OBJECT",
            EvalError::new(at, "CHANGE-CLASS expects a standard object instance"),
        ));
    };
    if let Some(info) = runtime.heap.class_info(instance_slot.get())
        && matches!(info.kind, ClassKind::ForwardReferenced)
        && super::classes::allocates_class_metaobjects(runtime, new_class_slot.get())
        && checked_definition(runtime, old_class, at)?.representation
            == crate::classes::Representation::ForwardReferencedClass
    {
        let previous = runtime.root(super::classes::allocate_class_object(
            runtime,
            info.name,
            ClassKind::ForwardReferenced,
            at,
        )?);
        let children = crate::classes::class_direct_subclasses(runtime, instance_slot.get(), at)?;
        let children = super::methods::list_from_words(
            runtime,
            children.iter().map(|c| c.get()).collect(),
            at,
        )?;
        runtime
            .heap
            .set_previous_class_subclasses(previous.slot.get(), children);
        // Direct-method and dependent spines support destructive unlinking.
        // Each metaobject therefore owns a separate snapshot of the list.
        for dependents in [false, true] {
            let head = runtime
                .heap
                .mop_relations(instance_slot.get(), dependents)
                .expect("class relations");
            let list =
                super::methods::list_from_words(runtime, proper_list(runtime, head, at)?, at)?;
            runtime
                .heap
                .set_mop_relations(previous.slot.get(), dependents, list);
        }
        runtime.heap.set_class_instance(
            previous.slot.get(),
            runtime
                .heap
                .class_instance(instance_slot.get())
                .expect("forward class storage"),
        );
        let layout = runtime
            .heap
            .instance_layout_of(previous.slot.get())
            .expect("forward metaclass layout");
        let target = checked_definition(runtime, new_class_slot.get(), at)?;
        let values = PreviousSlots::read(runtime, previous.slot.get(), layout)
            .transfer_locals(runtime, &target, |_| {});
        super::classes::attach_metaclass(runtime, &instance_slot, &new_class_slot, at)?;
        for i in 0..target.instance_slots {
            runtime
                .heap
                .set_instance_slot(instance_slot.get(), i, values.get(i));
        }
        return Ok(previous.slot.get());
    }
    let old_class_slot = runtime.root_slot(old_class);
    let old_layout = runtime
        .heap
        .instance_layout_of(instance_slot.get())
        .ok_or_else(|| error(at, "instance has no layout"))?;
    let old_def = crate::classes::instance_definition(runtime, old_class_slot.get(), old_layout)
        .ok_or_else(|| error(at, "class does not have a standard instance definition"))?;
    let new_def = checked_definition(runtime, new_class_slot.get(), at)?;
    use crate::classes::Representation;
    if matches!(
        new_def.representation,
        Representation::Method
            | Representation::StandardClass
            | Representation::FuncallableStandardClass
            | Representation::ForwardReferencedClass
            | Representation::DirectSlot
            | Representation::EffectiveSlot
    ) {
        return Err(error(
            at,
            "CHANGE-CLASS cannot convert an object into this metaobject class",
        ));
    }
    if new_def.representation != old_def.representation {
        return Err(error(
            at,
            "incompatible instance representations for CHANGE-CLASS",
        ));
    }
    let new_values = PreviousSlots::read(runtime, instance_slot.get(), old_layout)
        .transfer_locals(runtime, &new_def, |_| {});
    let generic = old_def.representation == Representation::GenericFunction;
    let previous = if generic {
        previous_generic_function(runtime, &instance_slot, &old_class_slot, &old_def, at)?
    } else {
        allocate_standard_instance(runtime, &old_class_slot, &old_def, Word::NIL, at)?
    };
    if let Some(target) = runtime.heap.funcallable_target(instance_slot.get()) {
        runtime.heap.set_funcallable_target(previous, target);
    }
    // Use the same layout as the original, including a prototype under finalization.
    for index in 0..old_def.instance_slots {
        let value = runtime
            .heap
            .instance_slot(instance_slot.get(), index)
            .expect("existing local slot");
        runtime.heap.set_instance_slot(previous, index, value);
    }
    let previous_slot = runtime.root_slot(previous);
    runtime
        .heap
        .replace_instance_class_and_storage(
            &instance_slot,
            &new_class_slot,
            &new_def.instance_layout,
            |idx| new_values.get(idx),
        )
        .map_err(|e| EvalError::out_of_memory(at, e))?;
    if generic {
        runtime.heap.invalidate_standard_dispatch(instance_slot.get());
    }
    Ok(previous_slot.get())
}

fn previous_generic_function(
    runtime: &Runtime,
    current: &RootSlot,
    class: &RootSlot,
    definition: &ClassDefinition,
    at: usize,
) -> Result<Word, Fault> {
    // The snapshot owns its list spines. Attached methods keep their current owner.
    let copy_list = |list| -> Result<RootSlot, Fault> {
        let entries = proper_list(runtime, list, at)?;
        Ok(runtime.root_slot(super::methods::list_from_words(runtime, entries, at)?))
    };
    let methods = copy_list(runtime.heap.generic_info(current.get()).unwrap().methods)?;
    let dependents = copy_list(runtime.heap.mop_relations(current.get(), true).unwrap())?;
    let payload = runtime.root_slot(
        runtime
            .heap
            .instance(class, &definition.instance_layout)
            .map_err(|e| EvalError::out_of_memory(at, e))?,
    );
    let previous = runtime
        .heap
        .generic_function_snapshot(current, &methods, &dependents)
        .map_err(|e| EvalError::out_of_memory(at, e))?;
    runtime.heap.set_additional_meta_instance(previous, payload.get());
    Ok(previous)
}

fn added_slot_names(
    runtime: &Runtime,
    previous: Word,
    current: Word,
    at: usize,
) -> Result<Word, Fault> {
    let prev_layout = runtime
        .heap
        .instance_layout_of(previous)
        .ok_or_else(|| error(at, "expected a standard object instance"))?;
    let curr_layout = runtime
        .heap
        .instance_layout_of(current)
        .ok_or_else(|| error(at, "expected a standard object instance"))?;
    let prev_names: HashSet<Word> = runtime
        .heap
        .instance_layout_entries(prev_layout)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let curr_entries = runtime
        .heap
        .instance_layout_entries(curr_layout)
        .unwrap_or_default();
    let added: Vec<Word> = curr_entries
        .into_iter()
        .filter_map(|(name, cell)| (cell.is_none() && !prev_names.contains(&name)).then_some(name))
        .collect();
    super::methods::list_from_words(runtime, added, at)
}

fn class_designator(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    if runtime.heap.class_info(value).is_some() {
        return Ok(value);
    }
    if !runtime.heap.is_symbol(value) {
        return Err(Fault::type_error(runtime, 
            value,
            "CLASS",
            EvalError::new(at, "expected a class or class name"),
        ));
    }
    find_named_class(runtime, value, at)?.ok_or_else(|| error(at, "no class with that name"))
}

fn checked_definition(
    runtime: &Runtime,
    class: Word,
    at: usize,
) -> Result<Rc<ClassDefinition>, Fault> {
    class_definition(runtime, class)
        .ok_or_else(|| error(at, "class does not have a standard instance definition"))
}

pub(super) enum Completion {
    Value,
    Signal { unhandled: bool },
    Warn,
}

enum InitMode {
    MakeCondition(Completion),
    DefaultInitargs,
    SharedInitialize { slot_names: Option<Vec<RootSlot>> },
}

enum InitializationLayout {
    Standard(Rc<ClassDefinition>),
    CompactCondition(Class, Rc<[EffectiveSlot]>),
}

impl InitializationLayout {
    fn for_class(runtime: &Runtime, class: Word, at: usize) -> Result<Self, Fault> {
        if let Some(definition) = class_definition(runtime, class) {
            return Ok(Self::Standard(definition));
        }
        if let Some(kind) = runtime
            .heap
            .class_info(class)
            .and_then(|info| classes::builtin_condition(runtime, info.origin_name))
        {
            return Ok(Self::CompactCondition(
                kind,
                condition::builtin_effective_slots(runtime, kind, at)?,
            ));
        }
        Err(error(
            at,
            "class does not have a standard instance definition",
        ))
    }

    fn slots(&self) -> &[EffectiveSlot] {
        match self {
            Self::Standard(def) => &def.slots,
            Self::CompactCondition(_, slots) => slots,
        }
    }

    fn defaults(&self) -> &[crate::classes::DefaultInitarg] {
        match self {
            Self::Standard(def) => &def.default_initargs,
            Self::CompactCondition(..) => &[],
        }
    }

    fn allocate(
        &self,
        runtime: &Runtime,
        class: &RootSlot,
        name: Word,
        at: usize,
    ) -> Result<Word, Fault> {
        match self {
            Self::Standard(def) => allocate_standard_instance(runtime, class, def, name, at),
            Self::CompactCondition(kind, _) => {
                condition::allocate_builtin_condition(runtime, *kind, at)
            }
        }
    }
}

pub(super) struct Initialization {
    class: RootSlot,
    layout: InitializationLayout,
    initargs: RootFrame,
    pairs: usize,
    next_default: usize,
    instance: Option<RootSlot>,
    next_slot: usize,
    awaiting: Option<InitializationResult>,
    mode: InitMode,
    associated_restart_scope: Option<usize>,
    resume: Resume,
    at: usize,
}

enum InitializationResult {
    Default(usize),
    SlotBoundp(usize),
    SlotInitform(usize),
}

impl Initialization {
    pub(super) fn new(
        runtime: &Runtime,
        class: Word,
        frame: &RootSlots,
        args: Range<usize>,
        completion: Completion,
        resume: Resume,
        at: usize,
    ) -> Result<Box<Self>, Fault> {
        Self::with_mode(
            runtime,
            class,
            None,
            frame,
            args,
            InitMode::MakeCondition(completion),
            resume,
            at,
        )
    }

    fn for_default_initargs(
        runtime: &Runtime,
        class: Word,
        frame: &RootSlots,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Box<Self>, Fault> {
        Self::with_mode(
            runtime,
            class,
            None,
            frame,
            args,
            InitMode::DefaultInitargs,
            resume,
            at,
        )
    }

    fn for_shared_initialize(
        runtime: &Runtime,
        instance: Word,
        slot_names_reg: usize,
        frame: &RootSlots,
        args: Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Box<Self>, Fault> {
        let instance = runtime.root_slot(instance);
        let class = classes::class_of(runtime, instance.get(), at)?;
        let slot_names_word = frame.get(slot_names_reg);
        let slot_names = if slot_names_word == Word::T {
            None
        } else {
            let list = proper_list(runtime, slot_names_word, at)
                .map_err(|err| Fault::type_error(runtime, slot_names_word, "LIST", err))?;
            Some(list.into_iter().map(|name| runtime.root_slot(name)).collect())
        };
        Self::with_mode(
            runtime,
            class,
            Some(instance),
            frame,
            args,
            InitMode::SharedInitialize { slot_names },
            resume,
            at,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal CLOS initialization dispatch takes mode and frame slice directly"
    )]
    fn with_mode(
        runtime: &Runtime,
        class: Word,
        instance: Option<RootSlot>,
        frame: &RootSlots,
        args: Range<usize>,
        mode: InitMode,
        resume: Resume,
        at: usize,
    ) -> Result<Box<Self>, Fault> {
        let class = runtime.root_slot(class);
        let layout = InitializationLayout::for_class(runtime, class.get(), at)?;
        check_initarg_pairs(runtime, frame, args.clone(), at)?;
        let extra_defaults = if matches!(mode, InitMode::SharedInitialize { .. }) {
            0
        } else {
            layout.defaults().len() * 2
        };
        let initargs = runtime.root_frame(args.len() + extra_defaults);
        let pairs = args.len() / 2;
        for (index, argument) in args.enumerate() {
            initargs.set(index, frame.get(argument));
        }
        let next_default = if matches!(mode, InitMode::SharedInitialize { .. }) {
            layout.defaults().len()
        } else {
            0
        };
        Ok(Box::new(Self {
            class,
            layout,
            initargs,
            pairs,
            next_default,
            instance,
            next_slot: 0,
            awaiting: None,
            mode,
            associated_restart_scope: None,
            resume,
            at,
        }))
    }

    pub(super) fn associate(&mut self, associated_restart_scope: usize) {
        self.associated_restart_scope = Some(associated_restart_scope);
    }

    fn validate_initargs(&self, runtime: &Runtime) -> Result<(), Fault> {
        use InitializationArgument::{NewInstance, Value};

        let all_slots = runtime.root_slot(Word::T);
        let instance = NewInstance(&self.class);
        validate_initarg_names(
            runtime,
            self.layout.slots(),
            &self.initargs,
            0..self.pairs * 2,
            &[
                ("ALLOCATE-INSTANCE", &[Value(&self.class)]),
                ("INITIALIZE-INSTANCE", &[instance]),
                ("SHARED-INITIALIZE", &[instance, Value(&all_slots)]),
            ],
            self.at,
        )
    }

    fn step(&mut self, runtime: &Runtime, returned: Word) -> Result<Outcome, Fault> {
        if let Some(awaiting) = self.awaiting.take() {
            match awaiting {
                InitializationResult::Default(index) => {
                    let name = self.layout.defaults()[index].name.get();
                    self.initargs.set(2 * self.pairs, name);
                    self.initargs.set(2 * self.pairs + 1, returned);
                    self.pairs += 1;
                }
                InitializationResult::SlotBoundp(index) => {
                    if returned == Word::NIL {
                        return Ok(self.call_slot_initform(index));
                    }
                }
                InitializationResult::SlotInitform(index) => {
                    if let Some(outcome) =
                        self.initialize_slot(runtime, &self.layout.slots()[index], returned)
                    {
                        return Ok(outcome);
                    }
                }
            }
        }
        while let Some(default) = self.layout.defaults().get(self.next_default) {
            let index = self.next_default;
            self.next_default += 1;
            if !(0..self.pairs).any(|pair| self.initargs.get(2 * pair) == default.name.get()) {
                self.awaiting = Some(InitializationResult::Default(index));
                return Ok(Outcome::call(&default.initform));
            }
        }
        if matches!(self.mode, InitMode::DefaultInitargs) {
            self.validate_initargs(runtime)?;
            let tail = runtime.root_frame(1);
            for index in (0..self.pairs * 2).rev() {
                let cell = runtime
                    .heap
                    .cons(|| (self.initargs.get(index), tail.get(0)))
                    .map_err(|error| EvalError::out_of_memory(self.at, error))?;
                tail.set(0, cell);
            }
            return Ok(Outcome::Value(tail.get(0)));
        }
        if self.instance.is_none() {
            self.validate_initargs(runtime)?;
            let instance = self
                .layout
                .allocate(runtime, &self.class, Word::NIL, self.at)?;
            self.instance = Some(runtime.root_slot(instance));
        }
        let instance = self.instance.as_ref().expect("allocated instance").get();
        while let Some(slot) = self.layout.slots().get(self.next_slot) {
            let index = self.next_slot;
            self.next_slot += 1;
            let initarg = (0..self.pairs).find(|&pair| {
                let key = self.initargs.get(2 * pair);
                slot.initargs.iter().any(|name| name.get() == key)
            });
            if let Some(pair) = initarg {
                if let Some(outcome) =
                    self.initialize_slot(runtime, slot, self.initargs.get(2 * pair + 1))
                {
                    return Ok(outcome);
                }
            } else if slot.initform.is_some() {
                let eligible = match &self.mode {
                    InitMode::SharedInitialize {
                        slot_names: Some(names),
                    } => names.iter().any(|name| name.get() == slot.name.get()),
                    _ => true,
                };
                if eligible {
                    if matches!(self.mode, InitMode::SharedInitialize { .. }) {
                        self.awaiting = Some(InitializationResult::SlotBoundp(index));
                        return Ok(self.call_slot_primitive(runtime, "SLOT-BOUNDP", slot, None));
                    }
                    if read_slot(runtime, instance, &slot.allocation) == Word::UNBOUND {
                        return Ok(self.call_slot_initform(index));
                    }
                }
            }
        }
        match &self.mode {
            InitMode::SharedInitialize { .. } | InitMode::MakeCondition(Completion::Value) => {
                Ok(Outcome::Value(instance))
            }
            InitMode::MakeCondition(Completion::Signal { unhandled }) => {
                let mut signal =
                    Signal::from_lisp(runtime, instance, *unhandled, self.at, self.resume);
                signal.associated_restart_scope = self.associated_restart_scope;
                Ok(Outcome::Signal(signal))
            }
            InitMode::MakeCondition(Completion::Warn) => {
                Ok(Outcome::Warn(instance, self.associated_restart_scope))
            }
            InitMode::DefaultInitargs => unreachable!("DefaultInitargs completes before slot loop"),
        }
    }

    fn call_slot_initform(&mut self, index: usize) -> Outcome {
        self.awaiting = Some(InitializationResult::SlotInitform(index));
        let initform = self.layout.slots()[index]
            .initform
            .as_ref()
            .expect("scheduled slot initform");
        Outcome::call(initform)
    }

    fn initialize_slot(
        &self,
        runtime: &Runtime,
        slot: &EffectiveSlot,
        value: Word,
    ) -> Option<Outcome> {
        if matches!(self.mode, InitMode::SharedInitialize { .. }) {
            return Some(self.call_slot_primitive(runtime, "(SETF SLOT-VALUE)", slot, Some(value)));
        }
        let instance = self.instance.as_ref().expect("allocated condition");
        write_slot(runtime, instance.get(), &slot.allocation, value);
        None
    }

    fn call_slot_primitive(
        &self,
        runtime: &Runtime,
        name: &str,
        slot: &EffectiveSlot,
        value: Option<Word>,
    ) -> Outcome {
        let function = runtime.root_slot(Primitive::named(name).expect("registered slot primitive"));
        let base = usize::from(value.is_some());
        let count = base + 2;
        let args = runtime.root_frame(count);
        args.set(
            base,
            self.instance.as_ref().expect("allocated instance").get(),
        );
        args.set(base + 1, slot.name.get());
        if let Some(value) = value {
            args.set(0, value);
        }
        Outcome::Call { function, args }
    }
}

fn validate_update_initargs(
    runtime: &Runtime,
    op: Op,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<(), Fault> {
    use InitializationArgument::Value;

    let fixed = match op {
        Op::ValidateReinitializeInitargs => 1,
        Op::ValidateChangeClassInitargs => 3,
        Op::ValidateRedefinedInitargs => 4,
        _ => unreachable!("initialization validation operation"),
    };
    let initargs = args.start + fixed..args.end;
    if initargs.is_empty() {
        return Ok(());
    }
    check_initarg_pairs(runtime, frame, initargs.clone(), at)?;
    let arguments: Vec<_> = (args.start..args.start + fixed)
        .map(|index| runtime.root_slot(frame.get(index)))
        .collect();
    let no_slots = runtime.root_slot(Word::NIL);
    let (method, arguments, instance, slot_names) = match arguments.as_slice() {
        [instance] => ("REINITIALIZE-INSTANCE", &arguments[..], instance, &no_slots),
        [_, current, added] => (
            "UPDATE-INSTANCE-FOR-DIFFERENT-CLASS",
            &arguments[..2],
            current,
            added,
        ),
        [instance, added, _, _] => (
            "UPDATE-INSTANCE-FOR-REDEFINED-CLASS",
            &arguments[..],
            instance,
            added,
        ),
        _ => unreachable!("initialization method arguments"),
    };
    let class = classes::class_of(runtime, instance.get(), at)?;
    let layout = InitializationLayout::for_class(runtime, class, at)?;
    let method_arguments: Vec<_> = arguments.iter().map(Value).collect();
    validate_initarg_names(
        runtime,
        layout.slots(),
        frame,
        initargs,
        &[
            (method, &method_arguments),
            ("SHARED-INITIALIZE", &[Value(instance), Value(slot_names)]),
        ],
        at,
    )
}

fn check_initarg_pairs(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<(), Fault> {
    if !args.len().is_multiple_of(2) {
        return Err(initarg_error(
            at,
            "initialization arguments must be name/value pairs",
        ));
    }
    if args
        .step_by(2)
        .any(|index| !runtime.heap.is_symbol(frame.get(index)))
    {
        return Err(initarg_error(
            at,
            "expected an initialization argument name",
        ));
    }
    Ok(())
}

fn validate_initarg_names(
    runtime: &Runtime,
    slots: &[EffectiveSlot],
    frame: &RootSlots,
    args: Range<usize>,
    calls: &[(&str, &[InitializationArgument<'_>])],
    at: usize,
) -> Result<(), Fault> {
    if args.is_empty() {
        return Ok(());
    }
    let allow_other_keys = runtime.root_slot(runtime.intern(&Name::keyword("ALLOW-OTHER-KEYS"), at)?);
    if args
        .clone()
        .step_by(2)
        .find(|&index| frame.get(index) == allow_other_keys.get())
        .is_some_and(|index| frame.get(index + 1) != Word::NIL)
    {
        return Ok(());
    }
    let mut method_names = Vec::new();
    for (name, arguments) in calls {
        let symbol = runtime.intern(&Name::common_lisp(name), at)?;
        let Some(generic) = runtime.heap.symbol_cell(symbol, true) else {
            continue;
        };
        if runtime.heap.generic_info(generic).is_none() {
            continue;
        }
        let generic = runtime.root(generic);
        let methods = generic::applicable_initialization_methods(runtime, &generic, arguments, at)?;
        for method in methods {
            let info = runtime.heap.method_info(method).expect("applicable method");
            let signature = compile::call_signature(runtime, &runtime.root(info.lambda_list), at)?;
            if signature.allow_other_keys {
                return Ok(());
            }
            method_names.extend(signature.keywords.into_iter().flatten());
        }
    }
    let method_keys = method_names
        .iter()
        .map(|name| runtime.intern(name, at).map(|symbol| runtime.root_slot(symbol)))
        .collect::<Result<Vec<_>, _>>()?;
    for index in args.step_by(2) {
        let name = frame.get(index);
        let known = name == allow_other_keys.get()
            || method_keys.iter().any(|key| key.get() == name)
            || slots
                .iter()
                .any(|slot| slot.initargs.iter().any(|arg| arg.get() == name));
        if !known {
            let spelling = runtime.heap.symbol_name(name).expect("validated initarg");
            return Err(initarg_error(
                at,
                format!("unrecognized initialization argument {spelling}"),
            ));
        }
    }
    Ok(())
}

fn initarg_error(at: usize, message: impl Into<String>) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

fn read_slot(runtime: &Runtime, object: Word, allocation: &SlotAllocation) -> Word {
    match allocation {
        SlotAllocation::Instance(index) => runtime.heap.instance_slot(object, *index),
        SlotAllocation::Class(shared) => runtime.heap.read_cell(shared.cell.get()),
        SlotAllocation::Custom(_) => unreachable!("custom slot access dispatched to a Lisp method"),
    }
    .expect("linked slot allocation")
}

fn write_slot(runtime: &Runtime, object: Word, allocation: &SlotAllocation, value: Word) {
    match allocation {
        SlotAllocation::Instance(index) => runtime.heap.set_instance_slot(object, *index, value),
        SlotAllocation::Class(shared) => runtime.heap.write_cell(shared.cell.get(), value),
        SlotAllocation::Custom(_) => unreachable!("custom slot access dispatched to a Lisp method"),
    }
    .expect("linked slot allocation");
}

pub(crate) fn bound_slot(runtime: &Runtime, object: Word, name: Word) -> Option<Word> {
    let layout = runtime.heap.instance_layout_of(object)?;
    let slots = runtime.heap.instance_layout_entries(layout)?;
    let (index, (_, cell)) = slots
        .into_iter()
        .enumerate()
        .find(|(_, (candidate, _))| *candidate == name)?;
    let value = match cell {
        Some(cell) => runtime.heap.read_cell(cell),
        None => runtime.heap.instance_slot(object, index),
    }?;
    (value != Word::UNBOUND).then_some(value)
}

fn error(at: usize, message: impl Into<String>) -> Fault {
    Fault::plain(Class::SimpleError, EvalError::new(at, message))
}

/// Execute a native call to a slot storage primitive on a current standard
/// instance. A miss leaves the call untouched for the VM, including Lisp
/// slot hooks and migration. `admit` charges the call before a storage write.
pub(crate) fn try_standard_slot_primitive(
    runtime: &Runtime,
    target: Word,
    arity: usize,
    argument_at: impl Fn(usize) -> Word,
    admit: impl FnOnce() -> bool,
) -> Option<Word> {
    let Primitive::Instance(operation @ Op::Slot(op)) = Primitive::decode(target)? else {
        return None;
    };
    if !operation.accepts(arity) {
        return None;
    }
    let base = usize::from(op.setter());
    let name = argument_at(base + 1);
    if !runtime.heap.is_symbol(name) {
        return None;
    }
    current_standard_slot(
        runtime,
        op,
        argument_at(base),
        name,
        op.setter().then(|| argument_at(0)),
        admit,
    )
}

// A warm access to current standard storage needs no suspended VM operation.
fn current_standard_slot(
    runtime: &Runtime,
    op: SlotOp,
    object: Word,
    name: Word,
    value: Option<Word>,
    admit: impl FnOnce() -> bool,
) -> Option<Word> {
    let protocol = runtime.invocation_protocol.get()?;
    let class = runtime.heap.instance_class(object)?;
    let layout = runtime.heap.instance_layout_of(object)?;
    let definition = crate::classes::instance_definition(runtime, class, layout)?;
    if layout != definition.instance_layout.get() {
        return None;
    }
    let slot = definition.slot(runtime, name);
    if op == SlotOp::ExistsP {
        return admit().then_some(truth(slot.is_some()));
    }
    let slot = slot?;
    if matches!(slot.allocation, SlotAllocation::Custom(_)) {
        return None;
    }
    let effective = slot.metaobject.get()?.get();
    if !protocol.standard_slot_storage(runtime, op, class, effective) {
        return None;
    }
    SlotLocation::Instance(&slot.allocation).access(runtime, op, object, value, admit)
}

pub(super) struct Access {
    target: AccessTarget,
    object: RootSlot,
    value: Option<RootSlot>,
    completing: Option<SlotOp>,
    resume: Resume,
    at: usize,
}

enum AccessTarget {
    Update,
    Named(SlotOp, RootSlot),
    Effective {
        op: SlotOp,
        class: RootSlot,
        slot: RootSlot,
    },
}

enum SlotLocation<'a> {
    Condition(usize),
    Structure(usize),
    Instance(&'a SlotAllocation),
}

impl SlotLocation<'_> {
    // NIL as a Lisp value is a successful read. None requests SLOT-UNBOUND.
    fn access(
        &self,
        runtime: &Runtime,
        op: SlotOp,
        object: Word,
        value: Option<Word>,
        admit: impl FnOnce() -> bool,
    ) -> Option<Word> {
        match op {
            SlotOp::Value => {
                let result = self.read(runtime, object);
                (result != Word::UNBOUND && admit()).then_some(result)
            }
            SlotOp::SetValue => {
                if !admit() {
                    return None;
                }
                let value = value.expect("slot setter value");
                self.write(runtime, object, value);
                Some(value)
            }
            SlotOp::Boundp => admit().then(|| truth(self.read(runtime, object) != Word::UNBOUND)),
            SlotOp::Makunbound => {
                if !admit() {
                    return None;
                }
                self.write(runtime, object, Word::UNBOUND);
                Some(object)
            }
            SlotOp::ExistsP => unreachable!("SLOT-EXISTS-P needs no storage access"),
        }
    }

    fn read(&self, runtime: &Runtime, object: Word) -> Word {
        match self {
            Self::Condition(index) => runtime
                .heap
                .condition_slot(object, *index)
                .expect("linked condition slot"),
            Self::Structure(index) => runtime
                .heap
                .structure_slot(object, *index)
                .expect("linked structure slot"),
            Self::Instance(allocation) => read_slot(runtime, object, allocation),
        }
    }

    fn write(&self, runtime: &Runtime, object: Word, value: Word) {
        match self {
            Self::Condition(index) => runtime
                .heap
                .set_condition_slot(object, *index, value)
                .expect("linked condition slot"),
            Self::Structure(index) => runtime
                .heap
                .set_structure_slot(object, *index, value)
                .expect("linked structure slot"),
            Self::Instance(allocation) => write_slot(runtime, object, allocation, value),
        }
    }
}

impl Access {
    fn start<'rt>(
        runtime: &'rt Runtime,
        target: AccessTarget,
        object: Word,
        value: Option<Word>,
        resume: Resume,
        at: usize,
    ) -> Invocation<'rt> {
        Invocation::AccessInstance(Box::new(Self {
            target,
            object: runtime.root_slot(object),
            value: value.map(|word| runtime.root_slot(word)),
            completing: None,
            resume,
            at,
        }))
    }

    fn step(&mut self, runtime: &Runtime, returned: Word) -> Result<Outcome, Fault> {
        let heap = &runtime.heap;
        if let Some(op) = self.completing {
            return Ok(Outcome::Value(match op {
                SlotOp::Value | SlotOp::Boundp => returned,
                SlotOp::Makunbound => self.object.get(),
                SlotOp::SetValue => self.value.as_ref().expect("slot setter value").get(),
                SlotOp::ExistsP => unreachable!("SLOT-EXISTS-P has no callback"),
            }));
        }
        let object = self.object.get();
        let Some(class) = heap.instance_class(object) else {
            if let Some(class) = heap.condition_class(object) {
                return self.condition_access(runtime, class);
            }
            return self.structure_access(runtime);
        };
        let layout = heap
            .instance_layout_of(object)
            .expect("standard instance layout");
        let definition =
            crate::classes::instance_definition(runtime, class, layout).ok_or_else(|| {
                error(
                    self.at,
                    "class does not have a standard instance definition",
                )
            })?;
        if layout != definition.instance_layout.get() {
            return self.migrate(runtime, &definition, layout);
        }
        match &self.target {
            AccessTarget::Update => Ok(Outcome::Value(object)),
            AccessTarget::Named(op, name) => {
                let (op, name) = (*op, name.get());
                let slot = definition.slot(runtime, name);
                if matches!(op, SlotOp::ExistsP) {
                    return Ok(Outcome::Value(truth(slot.is_some())));
                }
                let Some(slot) = slot else {
                    return self.hook(runtime, op, name, true);
                };
                let class = runtime.root_slot(class);
                let slot = super::mop::effective_slot(runtime, slot, self.at)?;
                self.protocol(runtime, op, class.get(), slot.get())
            }
            AccessTarget::Effective {
                op,
                class: expected,
                slot,
            } => {
                if expected.get() != class {
                    return Err(self.wrong_class());
                }
                let effective = heap
                    .slot_definition_info(slot.get())
                    .and_then(|info| definition.slot(runtime, info.name))
                    .filter(|effective| {
                        effective
                            .metaobject
                            .get()
                            .is_some_and(|cached| cached.get() == slot.get())
                    })
                    .ok_or_else(|| self.wrong_slot())?;
                if matches!(effective.allocation, SlotAllocation::Custom(_)) {
                    return Err(error(
                        self.at,
                        "custom slot allocation requires a slot protocol method",
                    ));
                }
                self.access_location(
                    runtime,
                    *op,
                    effective.name.get(),
                    SlotLocation::Instance(&effective.allocation),
                )
            }
        }
    }

    fn condition_access(
        &mut self,
        runtime: &Runtime,
        condition_class: Class,
    ) -> Result<Outcome, Fault> {
        let slots = condition::builtin_effective_slots(runtime, condition_class, self.at)?;
        match &self.target {
            AccessTarget::Update => Ok(Outcome::Value(self.object.get())),
            AccessTarget::Named(op, name) => {
                let op = *op;
                let slot = slots.iter().find(|slot| slot.name.get() == name.get());
                if matches!(op, SlotOp::ExistsP) {
                    return Ok(Outcome::Value(truth(slot.is_some())));
                }
                let Some(slot) = slot else {
                    return self.hook(runtime, op, name.get(), true);
                };
                let class = runtime.root_slot(super::classes::class_of(
                    runtime,
                    self.object.get(),
                    self.at,
                )?);
                let slot = super::mop::condition_effective_slot(runtime, &class, slot, self.at)?;
                self.protocol(runtime, op, class.get(), slot.get())
            }
            AccessTarget::Effective { op, class, slot } => {
                let actual = super::classes::class_of(runtime, self.object.get(), self.at)?;
                if actual != class.get() {
                    return Err(self.wrong_class());
                }
                let (index, definition) = slots
                    .iter()
                    .enumerate()
                    .find(|(_, definition)| {
                        definition
                            .metaobject
                            .get()
                            .is_some_and(|metaobject| metaobject.get() == slot.get())
                    })
                    .ok_or_else(|| self.wrong_slot())?;
                self.access_location(
                    runtime,
                    *op,
                    definition.name.get(),
                    SlotLocation::Condition(index),
                )
            }
        }
    }

    fn wrong_class(&self) -> Fault {
        error(self.at, "slot protocol class must be the object's class")
    }

    fn wrong_slot(&self) -> Fault {
        error(
            self.at,
            "slot definition is not an effective slot of the class",
        )
    }

    fn structure_access(&mut self, runtime: &Runtime) -> Result<Outcome, Fault> {
        let object = self.object.get();
        let expected_instance = || {
            Fault::type_error(runtime, 
                object,
                "STANDARD-OBJECT",
                EvalError::new(self.at, "expected a standard instance"),
            )
        };
        let Some((layout, _)) = runtime.heap.structure_layout(object) else {
            return if matches!(self.target, AccessTarget::Named(SlotOp::ExistsP, _)) {
                Ok(Outcome::Value(Word::NIL))
            } else {
                Err(expected_instance())
            };
        };
        let definition = runtime
            .structure_definition(layout)
            .expect("linked structure definition");
        match &self.target {
            AccessTarget::Named(op, name) => {
                let (op, name) = (*op, name.get());
                let slot = definition
                    .slots
                    .iter()
                    .enumerate()
                    .find(|(_, slot)| slot.name.get() == name);
                if matches!(op, SlotOp::ExistsP) {
                    return Ok(Outcome::Value(truth(slot.is_some())));
                }
                let Some((index, slot)) = slot else {
                    return self.hook(runtime, op, name, true);
                };
                let class = runtime.root_slot(super::classes::class_of(runtime, object, self.at)?);
                let slot =
                    super::mop::structure_slot(runtime, &definition, slot, index, false, self.at)?;
                self.protocol(runtime, op, class.get(), slot.get())
            }
            AccessTarget::Effective { op, class, slot } => {
                let actual = super::classes::class_of(runtime, object, self.at)?;
                if class.get() != actual {
                    return Err(self.wrong_class());
                }
                let index = runtime
                    .heap
                    .slot_definition_info(slot.get())
                    .and_then(|info| info.location.as_fixnum())
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| self.wrong_slot())?;
                let storage = definition
                    .slots
                    .get(index)
                    .filter(|storage| {
                        storage
                            .effective_metaobject
                            .get()
                            .is_some_and(|cached| cached.get() == slot.get())
                    })
                    .ok_or_else(|| self.wrong_slot())?;
                self.access_location(
                    runtime,
                    *op,
                    storage.name.get(),
                    SlotLocation::Structure(index),
                )
            }
            AccessTarget::Update => Err(expected_instance()),
        }
    }

    fn access_location(
        &mut self,
        runtime: &Runtime,
        op: SlotOp,
        name: Word,
        location: SlotLocation<'_>,
    ) -> Result<Outcome, Fault> {
        match location.access(
            runtime,
            op,
            self.object.get(),
            self.value.as_ref().map(RootSlot::get),
            || true,
        ) {
            Some(value) => Ok(Outcome::Value(value)),
            None => self.hook(runtime, op, name, false),
        }
    }

    fn protocol(
        &mut self,
        runtime: &Runtime,
        op: SlotOp,
        class: Word,
        slot: Word,
    ) -> Result<Outcome, Fault> {
        let (symbol, name) = op.binding(runtime);
        let function = name.get(runtime).ok_or_else(|| {
            Fault::cell(runtime, 
                Class::UndefinedFunction,
                symbol,
                EvalError::new(self.at, "slot protocol function is undefined"),
            )
        })?;
        let function = runtime.root_slot(function);
        let base = usize::from(op.setter());
        let args = runtime.root_frame(base + 3);
        args.set(base, class);
        args.set(base + 1, self.object.get());
        if let Some(value) = &self.value {
            args.set(0, value.get());
        }
        args.set(base + 2, slot);
        self.completing = Some(op);
        Ok(Outcome::Call { function, args })
    }

    fn migrate(
        &self,
        runtime: &Runtime,
        definition: &ClassDefinition,
        layout: Word,
    ) -> Result<Outcome, Fault> {
        let heap = &runtime.heap;
        let object = self.object.get();
        let mut previous = PreviousSlots::read(runtime, object, layout);
        let mut added = Vec::new();
        let values = previous.transfer_locals(runtime, definition, |name| added.push(name));
        let mut discarded = Vec::new();
        let mut properties = Vec::new();
        for (name, value) in previous.discarded_locals() {
            discarded.push(runtime.root_slot(name));
            if value != Word::UNBOUND {
                properties.push(runtime.root_slot(name));
                properties.push(runtime.root_slot(value));
            }
        }
        let args = runtime.root_frame(4);
        let list = super::methods::list_from_words;
        args.set(1, list(runtime, added, self.at)?);
        args.set(
            2,
            list(
                runtime,
                discarded.iter().map(RootSlot::get).collect(),
                self.at,
            )?,
        );
        args.set(
            3,
            list(
                runtime,
                properties.iter().map(RootSlot::get).collect(),
                self.at,
            )?,
        );
        let hook = runtime.intern(
            &Name::common_lisp("UPDATE-INSTANCE-FOR-REDEFINED-CLASS"),
            self.at,
        )?;
        let function = runtime.root_slot(super::designator(runtime, hook, self.at)?);
        // Publish the new layout before calling Lisp so recursive slot access
        // in the update method sees the migrated instance.
        heap.replace_instance_storage(&self.object, &definition.instance_layout, |index| {
            values.get(index)
        })
        .map_err(|error| EvalError::out_of_memory(self.at, error))?;
        args.set(0, self.object.get());
        Ok(Outcome::Call { function, args })
    }

    fn hook(
        &mut self,
        runtime: &Runtime,
        op: SlotOp,
        name: Word,
        missing: bool,
    ) -> Result<Outcome, Fault> {
        let name = runtime.root_slot(name);
        let hook = if missing {
            "SLOT-MISSING"
        } else {
            "SLOT-UNBOUND"
        };
        let hook = runtime.intern(&Name::common_lisp(hook), self.at)?;
        let function = runtime.root_slot(super::designator(runtime, hook, self.at)?);
        let operation = if missing {
            let name = match op {
                SlotOp::Value => "SLOT-VALUE",
                SlotOp::SetValue => "SETF",
                SlotOp::Boundp => "SLOT-BOUNDP",
                SlotOp::Makunbound => "SLOT-MAKUNBOUND",
                SlotOp::ExistsP => unreachable!("SLOT-EXISTS-P has no hook"),
            };
            Some(runtime.root_slot(
                runtime.intern(&Name::common_lisp(name), self.at)?,
            ))
        } else {
            None
        };
        let count = 3 + usize::from(operation.is_some()) + usize::from(self.value.is_some());
        let args = runtime.root_frame(count);
        args.set(
            0,
            super::classes::class_of(runtime, self.object.get(), self.at)?,
        );
        args.set(1, self.object.get());
        args.set(2, name.get());
        if let Some(operation) = operation {
            args.set(3, operation.get());
        }
        if let Some(value) = &self.value {
            args.set(4, value.get());
        }
        self.completing = Some(op);
        Ok(Outcome::Call { function, args })
    }
}

fn truth(value: bool) -> Word {
    if value { Word::T } else { Word::NIL }
}

enum Outcome {
    Value(Word),
    Signal(Signal),
    Warn(Word, Option<usize>),
    Call { function: RootSlot, args: RootFrame },
}

impl Outcome {
    fn call(function: &RootSlot) -> Self {
        Self::Call {
            function: function.clone(),
            args: RootFrame::empty(),
        }
    }
}

pub(super) enum Operation {
    Initialize(Box<Initialization>),
    Access(Box<Access>),
}

pub(super) fn dispatch<'rt>(
    runtime: &'rt Runtime,
    exec: &mut ExecState<'rt>,
    mut operation: Operation,
    returned: bool,
) -> Result<StepControl<'rt>, EvalError> {
    let returned = if returned {
        exec.values.get(0)
    } else {
        Word::NIL
    };
    let (outcome, at, resume) = match &mut operation {
        Operation::Initialize(initialization) => (
            initialization.step(runtime, returned),
            initialization.at,
            initialization.resume,
        ),
        Operation::Access(access) => (access.step(runtime, returned), access.at, access.resume),
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return dispatch_error(runtime, exec, error),
    };
    match outcome {
        Outcome::Value(word) => {
            exec.values.one(word);
            Ok(StepControl::Pending(finish(
                resume,
                &mut exec.frames,
                &mut exec.values,
                &mut exec.control,
            )))
        }
        Outcome::Signal(signal) => accept(runtime, exec, Invocation::Signal(signal)),
        Outcome::Warn(condition, enclosing) => {
            match super::warn_invocation(runtime, condition, resume, at, enclosing) {
                Ok(invocation) => accept(runtime, exec, invocation),
                Err(fault) => dispatch_error(runtime, exec, fault),
            }
        }
        Outcome::Call { function, args } => {
            let invocation = invoke(
                runtime,
                function.get(),
                &args,
                0..args.len(),
                &mut exec.values,
                Resume::Instance,
                at,
            );
            match invocation {
                Ok(invocation) => {
                    exec.control
                        .pause_instance(exec.frames.len().saturating_sub(1), operation);
                    accept(runtime, exec, invocation)
                }
                Err(error) => dispatch_error(runtime, exec, error),
            }
        }
    }
}
