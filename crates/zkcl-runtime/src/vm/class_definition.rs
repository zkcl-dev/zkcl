//! Native storage operations used by the class initialization protocol.
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::{
    Continuation, ContinuationStep, Resume, Values, classes as objects, methods, mop, proper_list,
};
use crate::classes::{
    self, ClassDeclaration, ClassNameClaim, DefaultInitarg, DirectSlot, RuntimeClassKind, SlotKind,
};
use crate::condition::{Class, Fault};
use crate::heap::{ClassKind, RootSlot, SlotFields, Word};
use crate::package::Package;
use crate::{EvalError, NativeFunction, Root, Runtime};

fn error(at: usize, message: &str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}

fn rooted(runtime: &Runtime, word: Word) -> Rc<RootSlot> {
    Rc::new(runtime.root_slot(word))
}

pub(super) struct DefineNamedClass<'rt> {
    claim: ClassNameClaim<'rt>,
    constructor: RootSlot,
    resume: Resume,
    at: usize,
}

impl<'rt> DefineNamedClass<'rt> {
    pub(super) fn new(
        runtime: &'rt Runtime,
        name: Word,
        constructor: Word,
        resume: Resume,
        at: usize,
    ) -> Result<Self, Fault> {
        let constructor = runtime.root_slot(super::designator(runtime, constructor, at)?);
        Ok(Self {
            claim: ClassNameClaim::acquire(runtime, name, at)?,
            constructor,
            resume,
            at,
        })
    }
}

impl Continuation for DefineNamedClass<'_> {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if !returned {
            return Ok(ContinuationStep::Call {
                target: self.constructor.get(),
                args: runtime.root_frame(0),
                at: self.at,
            });
        }
        let class = values.get(0);
        if runtime.heap.class_info(class).is_none() {
            return Err(error(self.at, "invalid class binding"));
        }
        self.claim.bind(class);
        Ok(ContinuationStep::Done(class))
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

fn list<'rt>(runtime: &'rt Runtime, value: Word, at: usize) -> Result<Vec<Root<'rt>>, Fault> {
    Ok(proper_list(runtime, value, at)
        .map_err(|e| Fault::type_error(runtime, value, "LIST", e))?
        .into_iter()
        .map(|w| runtime.root(w))
        .collect())
}

fn symbol(runtime: &Runtime, value: Word, at: usize) -> Result<Rc<RootSlot>, Fault> {
    if !runtime.heap.is_symbol(value) {
        return Err(Fault::type_error(runtime, 
            value,
            "SYMBOL",
            EvalError::new(at, "expected a symbol"),
        ));
    }
    Ok(rooted(runtime, value))
}

fn keyword(runtime: &Runtime, value: Word) -> Option<String> {
    (runtime.heap.symbol_home(value) == Some(Package::KEYWORD))
        .then(|| runtime.heap.symbol_name(value))
        .flatten()
}

fn slot_initfunction(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    if value == Word::NIL || super::is_function(runtime, value) {
        Ok(value)
    } else {
        Err(error(at, "expected an initialization function or NIL"))
    }
}

// The source plist and its existing slot object stay rooted in the caller.
// This conversion only inspects Lisp values; it does not allocate on the Lisp heap.
fn slot_fields(
    runtime: &Runtime,
    mut info: SlotFields,
    fields: &[Root<'_>],
    at: usize,
) -> Result<SlotFields, Fault> {
    let (pairs, rest) = fields.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(error(at, "expected a canonical slot property list"));
    }
    let mut seen = HashSet::new();
    let mut form = false;
    let mut function = false;
    for [key, value] in pairs {
        let id = runtime
            .heap
            .symbol_attributes(key.slot.get())
            .ok_or_else(|| error(at, "invalid slot option name"))?
            .id;
        if !seen.insert(id) {
            continue;
        }
        let word = value.slot.get();
        match keyword(runtime, key.slot.get()).as_deref() {
            Some("NAME") => info.name = word,
            Some("TYPE") => info.type_specifier = word,
            Some("INITFORM") => {
                info.initform = word;
                form = true;
            }
            Some("INITFUNCTION") => {
                info.initfunction = slot_initfunction(runtime, word, at)?;
                function = true;
            }
            Some("DOCUMENTATION") => {
                documentation(runtime, word, at)?;
                info.documentation = word;
            }
            Some("ALLOCATION") => {
                if !runtime.heap.is_symbol(word) || word == Word::NIL {
                    return Err(error(at, "expected a non-NIL slot allocation name"));
                }
                info.allocation = word;
            }
            Some("INITARGS") => {
                for arg in proper_list(runtime, word, at)? {
                    if !runtime.heap.is_symbol(arg) {
                        return Err(error(at, "expected a slot initialization argument name"));
                    }
                }
                info.initargs = word;
            }
            Some(kind @ ("READERS" | "WRITERS")) => {
                for value in proper_list(runtime, word, at)? {
                    super::FunctionName::parse(runtime, value, at)?;
                    if kind == "READERS" && !runtime.heap.is_symbol(value) {
                        return Err(error(at, "expected a reader name"));
                    }
                }
                if kind == "READERS" {
                    info.readers = word;
                } else {
                    info.writers = word;
                }
            }
            // MAKE-INSTANCE checks extension initargs against the actual slot class.
            _ => {}
        }
    }
    if !runtime.heap.is_variable(info.name) {
        return Err(error(at, "slot definition requires a variable name"));
    }
    if form != function {
        return Err(error(
            at,
            "slot :initform and :initfunction must be supplied together",
        ));
    }
    Ok(info)
}

fn direct_slot(
    runtime: &Runtime,
    info: SlotFields,
    meta: Option<Word>,
    at: usize,
) -> Result<DirectSlot, Fault> {
    if !info.direct || !runtime.heap.is_variable(info.name) {
        return Err(error(at, "expected an initialized direct slot definition"));
    }
    let root_slot = |word| rooted(runtime, word);
    let optional = |v| (v != Word::NIL).then(|| root_slot(v));
    Ok(DirectSlot {
        name: root_slot(info.name),
        allocation: match keyword(runtime, info.allocation).as_deref() {
            Some("CLASS") => SlotKind::Class,
            Some("INSTANCE") => SlotKind::Instance,
            _ => SlotKind::Custom(root_slot(info.allocation)),
        },
        initargs: proper_list(runtime, info.initargs, at)?
            .into_iter()
            .map(root_slot)
            .collect(),
        initform: optional(info.initfunction),
        initform_source: optional(info.initform),
        readers: optional(info.readers),
        writers: optional(info.writers),
        documentation: optional(info.documentation),
        type_specifier: (info.type_specifier != Word::T).then(|| root_slot(info.type_specifier)),
        metaobject: meta.map_or_else(OnceCell::new, |w| OnceCell::from(runtime.root_slot(w))),
    })
}

fn parse_slots(runtime: &Runtime, value: Word, at: usize) -> Result<Rc<[DirectSlot]>, Fault> {
    let specs = list(runtime, value, at)?;
    let instance = runtime.root(runtime.intern(&crate::symbol::Name::keyword("INSTANCE"), at)?);
    let mut names = HashSet::new();
    specs
        .iter()
        .map(|spec| {
            let (info, meta) =
                if let Some(info) = runtime.heap.slot_definition_info(spec.slot.get()) {
                    (info, Some(spec.slot.get()))
                } else {
                    let fields = list(runtime, spec.slot.get(), at)?;
                    (
                        slot_fields(
                            runtime,
                            SlotFields::empty(true, instance.slot.get()),
                            &fields,
                            at,
                        )?,
                        None,
                    )
                };
            let slot = direct_slot(runtime, info, meta, at)?;
            if !names.insert(runtime.heap.symbol_attributes(slot.name.get()).unwrap().id) {
                return Err(error(at, "duplicate direct slot name"));
            }
            Ok(slot)
        })
        .collect()
}

fn initialize_slot<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [slot, fields @ ..] = args else {
        return Err(error(at, "expected a slot definition"));
    };
    let word = slot.slot.get();
    let info = runtime
        .heap
        .slot_definition_info(word)
        .ok_or_else(|| error(at, "expected a standard slot definition"))?;
    if fields.is_empty() && info.name != Word::NIL {
        return Ok(vec![slot.clone()]); // A metaclass layout update only initializes its added slots.
    }
    if info.name != Word::NIL {
        return Err(error(at, "cannot reinitialize a slot definition"));
    }
    runtime
        .heap
        .update_slot_definition(word, slot_fields(runtime, info, fields, at)?);
    Ok(vec![slot.clone()])
}

// SBCL-compatible writers support metaclasses which amend a freshly computed
// effective slot. Once attached, its allocation and its physical layout agree.
fn set_effective_slot_field<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [slot, key, value] = args else {
        return Err(error(at, "expected an effective slot, field and value"));
    };
    let word = slot.slot.get();
    let mut info = runtime
        .heap
        .slot_definition_info(word)
        .filter(|info| !info.direct && info.name != Word::NIL)
        .ok_or_else(|| error(at, "expected an initialized effective slot"))?;
    if runtime.heap.slot_location_owner(word) != Some(Word::NIL) {
        return Err(error(at, "cannot change a finalized effective slot"));
    }
    match keyword(runtime, key.slot.get()).as_deref() {
        Some("INITFORM") => info.initform = value.slot.get(),
        Some("INITFUNCTION") => {
            info.initfunction = slot_initfunction(runtime, value.slot.get(), at)?
        }
        _ => info = slot_fields(runtime, info, &args[1..], at)?,
    }
    runtime.heap.update_slot_definition(word, info);
    Ok(vec![value.clone()])
}

fn parse_defaults(runtime: &Runtime, value: Word, at: usize) -> Result<Vec<DefaultInitarg>, Fault> {
    let mut names = HashSet::new();
    list(runtime, value, at)?
        .iter()
        .map(|entry| {
            let fields = list(runtime, entry.slot.get(), at)?;
            let [name, source, function] = fields.as_slice() else {
                return Err(error(
                    at,
                    "expected a canonical default initialization argument",
                ));
            };
            let key = symbol(runtime, name.slot.get(), at)?;
            if !names.insert(runtime.heap.symbol_attributes(key.get()).unwrap().id)
                || !super::is_function(runtime, function.slot.get())
            {
                return Err(error(at, "invalid default initialization argument"));
            }
            Ok(DefaultInitarg {
                name: key,
                initform: rooted(runtime, function.slot.get()),
                initform_source: rooted(runtime, source.slot.get()),
            })
        })
        .collect()
}

fn blank(runtime: &Runtime, kind: RuntimeClassKind, name: Word) -> ClassDeclaration {
    ClassDeclaration {
        kind,
        name: rooted(runtime, name),
        slots: Rc::from([]),
        default_initargs: Vec::new(),
        documentation: None,
        report: None,
    }
}

/// A compound type can be inhabited without having a named class. Such a
/// class declares no new slots or accessors and inherits the condition layout.
pub(super) fn make_condition_intersection(
    runtime: &Runtime,
    parents: Vec<Rc<RootSlot>>,
    at: usize,
) -> Result<Word, Fault> {
    let (_, name) = runtime.fresh_symbol("CONDITION-INTERSECTION", at)?;
    let declaration = blank(runtime, RuntimeClassKind::Condition, name);
    let class = rooted(
        runtime,
        objects::allocate_class_object(runtime, name, ClassKind::Standard, at)?,
    );
    let changes =
        classes::register_class(runtime, Rc::clone(&class), declaration, parents, at, true)?;
    debug_assert!(changes.shared.is_empty() && changes.removed_accessors.is_empty());
    runtime.heap.set_class_name(class.get(), Word::NIL);
    Ok(class.get())
}

fn documentation(runtime: &Runtime, value: Word, at: usize) -> Result<Option<Rc<RootSlot>>, Fault> {
    if value == Word::NIL {
        Ok(None)
    } else if runtime.heap.is_string(value) {
        Ok(Some(rooted(runtime, value)))
    } else {
        Err(Fault::type_error(runtime, 
            value,
            "STRING",
            EvalError::new(at, "expected class documentation"),
        ))
    }
}

fn class_changes<'rt>(
    runtime: &'rt Runtime,
    changes: classes::ClassChanges,
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let lists = || -> Result<Vec<Root<'rt>>, Fault> {
        let mut slots = Vec::new();
        for slot in &changes.shared {
            slots.push(runtime.root(methods::list_from_words(
                runtime,
                vec![
                    slot.class.get(),
                    slot.slot_name.get(),
                    slot.cell.get(),
                    slot.initform.get(),
                ],
                at,
            )?));
        }
        let mut removed = Vec::new();
        for (gf, m) in &changes.removed_accessors {
            removed.push(runtime.root(methods::list_from_words(
                runtime,
                vec![gf.get(), m.get()],
                at,
            )?));
        }
        [slots, removed]
            .iter()
            .map(|entries| {
                Ok(runtime.root(methods::list_from_words(
                    runtime,
                    entries.iter().map(|r| r.slot.get()).collect(),
                    at,
                )?))
            })
            .collect()
    };
    let result = lists();
    if result.is_err() {
        for (gf, m) in changes.removed_accessors {
            methods::remove(runtime, gf.get(), m.get(), at)?;
        }
    }
    result
}

fn metaclass(runtime: &Runtime, designator: Word, at: usize) -> Result<Word, Fault> {
    let class = if runtime.heap.class_info(designator).is_some() {
        designator
    } else {
        objects::find_named_class(runtime, designator, at)?
            .ok_or_else(|| error(at, "undefined metaclass"))?
    };
    if !objects::allocates_class_metaobjects(runtime, class) {
        return Err(error(
            at,
            "expected a metaclass whose instances are class metaobjects",
        ));
    }
    Ok(class)
}

/// DEFINE-CONDITION and the ANSI compile-time class declaration use the same
/// canonical slot records as ENSURE-CLASS. A provisional definition does not
/// execute slot initforms or require the load-time methods of the metaclass.
pub(crate) fn register_declaration<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [
        condition,
        active,
        name,
        parents,
        slots,
        defaults,
        docs,
        report,
        meta,
    ] = args
    else {
        return Err(error(at, "invalid class declaration"));
    };
    let kind = if condition.is_nil() {
        RuntimeClassKind::Standard
    } else {
        RuntimeClassKind::Condition
    };
    let existing = classes::named_target(runtime, name.slot.get(), at)?.map(|c| runtime.root(c));
    let id = runtime
        .heap
        .symbol_attributes(name.slot.get())
        .expect("validated class name")
        .id;
    classes::check_name_unclaimed(runtime, id, at)?;
    if active.is_nil()
        && existing
            .as_ref()
            .is_some_and(|c| classes::declaration_active(runtime, c.slot.get()) == Some(true))
    {
        return Ok(vec![runtime.nil()]);
    }
    let class = match existing {
        Some(class) => class,
        None => runtime.root(objects::allocate_class_object(
            runtime,
            name.slot.get(),
            ClassKind::Standard,
            at,
        )?),
    };
    let mut declaration = blank(runtime, kind, name.slot.get());
    declaration.slots = parse_slots(runtime, slots.slot.get(), at)?;
    declaration.default_initargs = parse_defaults(runtime, defaults.slot.get(), at)?;
    declaration.documentation = documentation(runtime, docs.slot.get(), at)?;
    declaration.report = (!report.is_nil()).then(|| rooted(runtime, report.slot.get()));
    let meta = if condition.is_nil() {
        Some(runtime.root(if meta.is_nil() {
            objects::builtin_class(runtime, "STANDARD-CLASS", at)?
        } else {
            metaclass(runtime, meta.slot.get(), at)?
        }))
    } else {
        None
    };
    let mut parents =
        classes::resolve_parents(runtime, &list(runtime, parents.slot.get(), at)?, at)?;
    if parents.is_empty() {
        let default = match &meta {
            None => "CONDITION",
            Some(meta)
                if classes::class_definition(runtime, meta.slot.get()).is_some_and(|d| {
                    d.representation == classes::Representation::FuncallableStandardClass
                }) =>
            {
                "FUNCALLABLE-STANDARD-OBJECT"
            }
            _ => "STANDARD-OBJECT",
        };
        parents.push(rooted(runtime, objects::builtin_class(runtime, default, at)?));
    }
    let previous_storage = runtime.root(
        runtime
            .heap
            .class_instance(class.slot.get())
            .unwrap_or(Word::NIL),
    );
    let attach = previous_storage.is_nil()
        || matches!(
            runtime
                .heap
                .class_info(class.slot.get())
                .expect("class being declared")
                .kind,
            ClassKind::ForwardReferenced
        );
    if attach {
        if let Some(meta) = meta {
            objects::attach_metaclass(runtime, &class.slot, &meta.slot, at)?;
        } else {
            runtime.heap.set_class_instance(class.slot.get(), Word::NIL);
        }
    }
    let pending = classes::register_class(
        runtime,
        rooted(runtime, class.slot.get()),
        declaration,
        parents,
        at,
        !active.is_nil(),
    )
    .inspect_err(|_| {
        if attach {
            runtime
                .heap
                .set_class_instance(class.slot.get(), previous_storage.slot.get());
        }
    })?;
    classes::bind_class(runtime, name.slot.get(), class.slot.get());
    class_changes(runtime, pending, at)
}

fn configure<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [class, parents, options] = args else {
        return Err(error(at, "invalid class initialization"));
    };
    let info = runtime
        .heap
        .class_info(class.slot.get())
        .ok_or_else(|| error(at, "expected class metaobject"))?;
    if objects::is_system_class(runtime, info.origin_name) && info.origin_name != Word::NIL {
        return Err(error(at, "cannot reinitialize a system class"));
    }
    let mut declaration = classes::class_declaration(runtime, class.slot.get()).map_or_else(
        || blank(runtime, RuntimeClassKind::Standard, info.name),
        |d| (*d).clone(),
    );
    let options = list(runtime, options.slot.get(), at)?;
    let (pairs, rest) = options.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(error(at, "expected paired class initialization arguments"));
    }
    let mut seen = HashSet::new();
    for [key, value] in pairs {
        let Some(id) = runtime.heap.symbol_attributes(key.slot.get()).map(|s| s.id) else {
            return Err(error(at, "invalid initialization argument name"));
        };
        if !seen.insert(id) {
            continue;
        }
        match keyword(runtime, key.slot.get()).as_deref() {
            Some("DIRECT-SLOTS") => declaration.slots = parse_slots(runtime, value.slot.get(), at)?,
            Some("DIRECT-DEFAULT-INITARGS") => {
                declaration.default_initargs = parse_defaults(runtime, value.slot.get(), at)?
            }
            Some("DOCUMENTATION") => {
                declaration.documentation = documentation(runtime, value.slot.get(), at)?
            }
            Some("NAME") => declaration.name = symbol(runtime, value.slot.get(), at)?,
            _ => {}
        }
    }
    let parents = classes::resolve_parents(runtime, &list(runtime, parents.slot.get(), at)?, at)?;
    let update = classes::prepare_class(
        runtime,
        rooted(runtime, class.slot.get()),
        declaration,
        parents,
        at,
        true,
    )?;
    let (token, affected) = classes::begin_update(runtime, update, at)?;
    Ok(vec![runtime.root(token), runtime.root(affected)])
}

/// Check all generated signatures before replacing any installed accessor. A
/// previously unbound function may be named by several slots in one DEFCLASS.
pub(crate) fn validate_accessors(
    runtime: &Runtime,
    slots: &[DirectSlot],
    at: usize,
) -> Result<(), Fault> {
    let mut arities = HashMap::new();
    for slot in slots {
        for (names, arity) in [(&slot.readers, 1), (&slot.writers, 2)] {
            let Some(names) = names else { continue };
            for word in proper_list(runtime, names.get(), at)? {
                let name = super::FunctionName::parse(runtime, word, at)?;
                let (setter, symbol) = match name {
                    super::FunctionName::Symbol(s) => (false, s),
                    super::FunctionName::Setf(s) => (true, s),
                };
                let id = runtime
                    .heap
                    .symbol_attributes(symbol)
                    .expect("accessor symbol")
                    .id;
                if arities
                    .insert((setter, id), arity)
                    .is_some_and(|prior| prior != arity)
                {
                    return Err(error(
                        at,
                        "incongruent reader and writer with the same name",
                    ));
                }
                if let Some(bound) = name.get(runtime) {
                    let info = runtime
                        .heap
                        .generic_info(bound)
                        .ok_or_else(|| error(at, "accessor name denotes an ordinary function"))?;
                    let sig = info.call_signature;
                    if sig.required != arity || sig.positional != arity || sig.accepts_extra {
                        return Err(error(
                            at,
                            "accessor and generic function lambda lists are not congruent",
                        ));
                    }
                } else {
                    name.check_definable(runtime, at)?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    // Public names must be external when the reader loads mop.lisp.
    for name in [
        "DIRECT-SLOT-DEFINITION-CLASS",
        "EFFECTIVE-SLOT-DEFINITION-CLASS",
        "COMPUTE-EFFECTIVE-SLOT-DEFINITION",
        "ENSURE-CLASS",
        "ENSURE-CLASS-USING-CLASS",
        "VALIDATE-SUPERCLASS",
        "STANDARD-INSTANCE-ACCESS",
        "FUNCALLABLE-STANDARD-INSTANCE-ACCESS",
        "SET-FUNCALLABLE-INSTANCE-FUNCTION",
    ] {
        mop::export(runtime, name)?;
    }
    let name = "%DEFINE-NAMED-CLASS";
    let symbol = runtime.intern_package(Package::MOP, name, 0)?.0;
    let function = super::Primitive::named("ZK-MOP::%DEFINE-NAMED-CLASS")
        .expect("registered class definition primitive");
    runtime.heap.set_symbol_cell(symbol, true, function);
    type Callback = for<'rt> fn(&'rt Runtime, &[Root<'rt>], usize) -> Result<Vec<Root<'rt>>, Fault>;
    for (name, callback) in [
        (
            "%CLASS-TARGET",
            (|rt: &Runtime, args: &[Root<'_>], at| {
                let [name] = args else {
                    return Err(error(at, "expected a class name"));
                };
                Ok(vec![rt.root(
                    classes::named_target(rt, name.slot.get(), at)?.unwrap_or(Word::NIL),
                )])
            }) as Callback,
        ),
        ("%METACLASS", |rt, args, at| {
            let [meta] = args else {
                return Err(error(at, "expected a metaclass"));
            };
            Ok(vec![rt.root(metaclass(rt, meta.slot.get(), at)?)])
        }),
        ("%RESOLVE-PARENTS", |rt, args, at| {
            let [parents] = args else {
                return Err(error(at, "expected a superclass list"));
            };
            let parents = classes::resolve_parents(rt, &list(rt, parents.slot.get(), at)?, at)?;
            Ok(vec![rt.root(methods::list_from_words(
                rt,
                parents.iter().map(|p| p.get()).collect(),
                at,
            )?)])
        }),
        ("%CLASS-ACTIVE-P", |rt, args, at| {
            let [class] = args else {
                return Err(error(at, "expected a class"));
            };
            if rt.heap.class_info(class.slot.get()).is_none() {
                return Err(error(at, "expected a class"));
            }
            Ok(vec![rt.root(
                if classes::declaration_active(rt, class.slot.get()) == Some(true) {
                    Word::T
                } else {
                    Word::NIL
                },
            )])
        }),
        ("%COMPILER-CLASS-P", |rt, args, at| {
            let [class] = args else {
                return Err(error(at, "expected a class"));
            };
            let provisional = classes::declaration_active(rt, class.slot.get()) == Some(false);
            Ok(vec![rt.root(if provisional { Word::T } else { Word::NIL })])
        }),
        ("%ATTACH-PROVISIONAL-METACLASS", |rt, args, at| {
            let [class, meta] = args else {
                return Err(error(at, "expected a provisional class and its metaclass"));
            };
            if rt.heap.class_info(class.slot.get()).is_none()
                || classes::declaration_active(rt, class.slot.get()) == Some(true)
            {
                return Err(error(at, "expected a provisional class"));
            }
            let previous = rt.root(
                rt.heap
                    .class_instance(class.slot.get())
                    .unwrap_or(Word::NIL),
            );
            objects::attach_metaclass(rt, &class.slot, &meta.slot, at)?;
            Ok(vec![previous])
        }),
        ("%RESTORE-UNPUBLISHED-STORAGE", |rt, args, at| {
            let [class, storage] = args else {
                return Err(error(
                    at,
                    "expected a provisional class and its original storage",
                ));
            };
            if rt.heap.class_info(class.slot.get()).is_none()
                || !(storage.is_nil() || rt.heap.instance_class(storage.slot.get()).is_some())
            {
                return Err(error(at, "invalid provisional class storage"));
            }
            if classes::declaration_active(rt, class.slot.get()) != Some(true) {
                rt.heap
                    .set_class_instance(class.slot.get(), storage.slot.get());
            }
            Ok(Vec::new())
        }),
        ("%CLASS-STORAGE", |rt, args, at| {
            let [class] = args else {
                return Err(error(at, "expected a class"));
            };
            Ok(vec![
                rt.root(
                    rt.heap
                        .class_instance(class.slot.get())
                        .unwrap_or(Word::NIL),
                ),
            ])
        }),
        ("SET-FUNCALLABLE-INSTANCE-FUNCTION", |rt, args, at| {
            let [instance, function] = args else {
                return Err(error(at, "expected a funcallable instance and a function"));
            };
            if !super::is_function(rt, function.slot.get()) {
                return Err(Fault::type_error(
                    rt,
                    function.slot.get(),
                    "FUNCTION",
                    EvalError::new(at, "expected a function"),
                ));
            }
            if rt.heap.generic_info(instance.slot.get()).is_some() {
                rt.heap
                    .set_generic_function_target(instance.slot.get(), function.slot.get());
            } else {
                rt.heap
                    .set_funcallable_target(instance.slot.get(), function.slot.get())
                    .ok_or_else(|| error(at, "expected a funcallable instance"))?;
            }
            Ok(vec![function.clone()])
        }),
        ("%LOCAL-SLOT", |rt, args, at| {
            let [object, index, funcallable, rest @ ..] = args else {
                return Err(error(at, "expected an instance slot location"));
            };
            let instance = object.slot.get();
            if (rt.heap.funcallable_target(instance).is_some()
                || rt.heap.generic_info(instance).is_some())
                == funcallable.is_nil()
            {
                return Err(error(at, "wrong instance representation for slot access"));
            }
            let index = index
                .as_fixnum()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| error(at, "expected a nonnegative slot location"))?;
            match rest {
                [] => {
                    let word = rt
                        .heap
                        .instance_slot(instance, index)
                        .ok_or_else(|| error(at, "invalid local slot location"))?;
                    if word == Word::UNBOUND {
                        return Err(error(at, "unbound local slot"));
                    }
                    Ok(vec![rt.root(word)])
                }
                [value] => {
                    rt.heap
                        .set_instance_slot(instance, index, value.slot.get())
                        .ok_or_else(|| error(at, "invalid local slot location"))?;
                    Ok(vec![value.clone()])
                }
                _ => Err(error(at, "expected one new slot value")),
            }
        }),
        ("%PENDING-CLASS-P", |rt, args, at| {
            let [class] = args else {
                return Err(error(at, "expected a class"));
            };
            Ok(vec![rt.root(
                if classes::current_update(rt, class.slot.get()).is_some() {
                    Word::T
                } else {
                    Word::NIL
                },
            )])
        }),
        ("%USE-PRECEDENCE", |rt, args, at| {
            let [class, order] = args else {
                return Err(error(at, "expected a class and its precedence list"));
            };
            let update = classes::current_update(rt, class.slot.get())
                .ok_or_else(|| error(at, "no pending class"))?;
            update.set_precedence(
                rt,
                class.slot.get(),
                proper_list(rt, order.slot.get(), at)?,
                at,
            )?;
            Ok(Vec::new())
        }),
        ("%ASSIGN-SLOT-LOCATIONS", |rt, args, at| {
            let [slots] = args else {
                return Err(error(at, "expected effective slot definitions"));
            };
            let instance = rt.intern(&crate::symbol::Name::keyword("INSTANCE"), at)?;
            let mut local = 0;
            for slot in proper_list(rt, slots.slot.get(), at)? {
                let info = rt
                    .heap
                    .slot_definition_info(slot)
                    .filter(|info| !info.direct && rt.heap.is_variable(info.name))
                    .ok_or_else(|| {
                        error(at, "expected an initialized effective slot definition")
                    })?;
                let location = if info.allocation == instance {
                    let index = Word::fixnum(local).expect("instance slot index");
                    local += 1;
                    index
                } else {
                    Word::NIL
                };
                if rt.heap.slot_location_owner(slot) == Some(Word::NIL) {
                    rt.heap.set_slot_location(slot, Word::NIL, location);
                } else if info.location != location {
                    return Err(error(at, "cannot move a finalized effective slot"));
                }
            }
            Ok(vec![slots.clone()])
        }),
        ("%USE-LAYOUT", |rt, args, at| {
            let [class, slots, defaults] = args else {
                return Err(error(at, "expected class, slots and defaults"));
            };
            let update = classes::current_update(rt, class.slot.get())
                .ok_or_else(|| error(at, "no pending class"))?;
            let defaults = parse_defaults(rt, defaults.slot.get(), at)?;
            let shared = update.finish_layout(
                rt,
                class.slot.get(),
                &list(rt, slots.slot.get(), at)?,
                defaults,
                at,
            )?;
            class_changes(
                rt,
                classes::ClassChanges {
                    shared,
                    removed_accessors: Vec::new(),
                },
                at,
            )
        }),
        ("%PUBLISH-CLASS", |rt, args, at| {
            let [token] = args else {
                return Err(error(at, "expected class finalization token"));
            };
            class_changes(rt, classes::finish_update(rt, token.slot.get(), at)?, at)
        }),
        ("%END-CLASS-UPDATE", |rt, args, at| {
            let [token] = args else {
                return Err(error(at, "expected class finalization token"));
            };
            classes::end_update(rt, token.slot.get(), at)?;
            Ok(Vec::new())
        }),
        ("%CONFIGURE-CLASS", configure),
        ("%INITIALIZE-SLOT", initialize_slot),
        ("%SET-EFFECTIVE-SLOT-FIELD", set_effective_slot_field),
    ] {
        let symbol = runtime.intern_package(Package::MOP, name, 0)?.0;
        let mut natives = runtime.natives.borrow_mut();
        let function = Word::native(natives.len());
        natives.push(std::sync::Arc::new(callback) as std::sync::Arc<NativeFunction>);
        runtime.heap.set_symbol_cell(symbol, true, function);
    }
    let initializer_symbol = runtime
        .intern_package(Package::MOP, "%INITIALIZE-CLASS-SLOT", 0)?
        .0;
    runtime.heap.set_symbol_cell(
        initializer_symbol,
        true,
        crate::macros::compiled_native(crate::macros::INITIALIZE_SHARED_SLOT)
            .expect("class slot initializer"),
    );
    let update = runtime
        .intern_package(Package::MOP, "%UPDATE-INSTANCE", 0)?
        .0;
    runtime.heap.set_symbol_cell(
        update,
        true,
        super::Primitive::named("%UPDATE-OBSOLETE-INSTANCE")
            .expect("registered obsolete instance update"),
    );
    runtime.load_builtin_source(include_str!("../../lisp/runtime/clos/mop.lisp"))
}
