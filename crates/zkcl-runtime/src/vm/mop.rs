use std::cell::OnceCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::classes::{
    self as registry, DefaultInitarg, DirectSlot, EffectiveSlot, SlotAllocation, SlotKind,
};
use crate::condition::{self, Class, Fault};
use crate::heap::{ClassKind, HashTest, RootSlot, SlotFields, Word};
use crate::package::Package;
use crate::symbol::Name;
use crate::{EvalError, NativeFunction, Root, Runtime, form_value, read};

use super::methods::{list_from_words, required_parameters};
use super::structures::{SlotDefinition as StructureSlot, StructureDefinition};
use super::{Primitive, classes, equality, generic, proper_list};

pub(super) fn intern_eql_specializer(
    runtime: &Runtime,
    object: Word,
    at: usize,
) -> Result<Word, Fault> {
    let heap = &runtime.heap;
    let object = runtime.root_slot(object);
    let table = match runtime.eql_specializers.get() {
        Some(table) => table,
        None => {
            let table = heap
                .hash_table(
                    HashTest::Eql,
                    // Every specializer strongly traces the exact object used as its
                    // key. Testing both fields for liveness therefore preserves
                    // interning without registering a redundant reverse edge.
                    Some(zkcl_bytecode::HashTableWeakness::KeyAndValue),
                    8,
                    zkcl_bytecode::RehashHint::integer(8),
                    zkcl_bytecode::RehashHint::integer(1),
                )
                .map_err(|err| EvalError::out_of_memory(at, err))?;
            runtime
                .eql_specializers
                .get_or_init(|| runtime.root_slot(table))
        }
    };
    let hash = equality::hash(heap, object.get(), HashTest::Eql);
    let same = |a, b| equality::eql(heap, a, b);
    if let Some(existing) = heap.hash_table_get(table.get(), object.get(), hash, same) {
        return Ok(existing);
    }
    let specializer = runtime.root_slot(
        heap.specializer(|| object.get())
            .map_err(|err| EvalError::out_of_memory(at, err))?,
    );
    heap.hash_table_set(table.get(), object.get(), specializer.get(), hash, same)
        .map_err(|err| EvalError::out_of_memory(at, err))?;
    Ok(specializer.get())
}

#[derive(Clone, Copy)]
enum Reader {
    ClassPrecedence,
    ComputePrecedence,
    ClassDirectSuperclasses,
    ClassDirectSubclasses,
    ClassFinalized,
    FinalizeInheritance,
    ClassDirectSlots,
    ClassEffectiveSlots,
    ClassDirectDefaultInitargs,
    ClassDefaultInitargs,
    SlotName,
    SlotAllocation,
    SlotLocation,
    SlotType,
    SlotInitargs,
    SlotInitform,
    SlotInitfunction,
    SlotReaders,
    SlotWriters,
    SlotDocumentation,
    ClassPrototype,
    GenericName,
    GenericLambdaList,
    GenericPrecedence,
    GenericMethods,
    GenericMethodClass,
    GenericDeclarations,
    GenericDocumentation,
    MethodLambdaList,
    MethodSpecializers,
    MethodOwner,
    MethodDocumentation,
    AccessorSlot,
}

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    classes::export_mop_classes(runtime)?;
    for name in [
        "CLASS",
        "STANDARD-CLASS",
        "BUILT-IN-CLASS",
        "STANDARD-OBJECT",
        "METHOD",
        "STANDARD-METHOD",
        "GENERIC-FUNCTION",
        "STANDARD-GENERIC-FUNCTION",
        "CLASS-NAME",
        "METHOD-QUALIFIERS",
        "COMPUTE-APPLICABLE-METHODS",
        "ADD-METHOD",
        "REMOVE-METHOD",
        "FIND-METHOD",
    ] {
        let symbol = runtime.intern(&Name::common_lisp(name), 0)?;
        runtime
            .packages
            .borrow_mut()
            .insert(Package::MOP, name.into(), symbol, true);
    }
    let push_native = |callback: Arc<NativeFunction>| {
        let mut natives = runtime.natives.borrow_mut();
        let function = Word::native(natives.len());
        natives.push(callback);
        function
    };
    let intern = export(runtime, "INTERN-EQL-SPECIALIZER")?;
    let callback = push_native(Arc::new(|runtime, args, at| {
        let [object] = args else {
            return Err(problem(at, "INTERN-EQL-SPECIALIZER expects one argument"));
        };
        Ok(vec![runtime.root(intern_eql_specializer(
            runtime,
            object.slot.get(),
            at,
        )?)])
    }));
    runtime
        .heap
        .set_symbol_cell(intern.slot.get(), true, callback);
    for (class, signature, readers) in [
        (
            "CLASS",
            "(class)",
            &[
                ("CLASS-PRECEDENCE-LIST", Reader::ClassPrecedence),
                ("COMPUTE-CLASS-PRECEDENCE-LIST", Reader::ComputePrecedence),
                ("COMPUTE-SLOTS", Reader::ClassEffectiveSlots),
                ("COMPUTE-DEFAULT-INITARGS", Reader::ClassDefaultInitargs),
                ("CLASS-DIRECT-SUPERCLASSES", Reader::ClassDirectSuperclasses),
                ("CLASS-DIRECT-SUBCLASSES", Reader::ClassDirectSubclasses),
                ("CLASS-FINALIZED-P", Reader::ClassFinalized),
                ("FINALIZE-INHERITANCE", Reader::FinalizeInheritance),
                ("CLASS-DIRECT-SLOTS", Reader::ClassDirectSlots),
                ("CLASS-SLOTS", Reader::ClassEffectiveSlots),
                ("CLASS-PROTOTYPE", Reader::ClassPrototype),
                ("CLASS-DEFAULT-INITARGS", Reader::ClassDefaultInitargs),
                (
                    "CLASS-DIRECT-DEFAULT-INITARGS",
                    Reader::ClassDirectDefaultInitargs,
                ),
            ][..],
        ),
        (
            "SLOT-DEFINITION",
            "(slot)",
            &[
                ("SLOT-DEFINITION-NAME", Reader::SlotName),
                ("SLOT-DEFINITION-ALLOCATION", Reader::SlotAllocation),
                ("SLOT-DEFINITION-TYPE", Reader::SlotType),
                ("SLOT-DEFINITION-INITARGS", Reader::SlotInitargs),
                ("SLOT-DEFINITION-INITFORM", Reader::SlotInitform),
                ("SLOT-DEFINITION-INITFUNCTION", Reader::SlotInitfunction),
                ("SLOT-DEFINITION-DOCUMENTATION", Reader::SlotDocumentation),
            ][..],
        ),
        (
            "EFFECTIVE-SLOT-DEFINITION",
            "(slot)",
            &[("SLOT-DEFINITION-LOCATION", Reader::SlotLocation)][..],
        ),
        (
            "DIRECT-SLOT-DEFINITION",
            "(slot)",
            &[
                ("SLOT-DEFINITION-READERS", Reader::SlotReaders),
                ("SLOT-DEFINITION-WRITERS", Reader::SlotWriters),
            ][..],
        ),
        (
            "STANDARD-GENERIC-FUNCTION",
            "(generic-function)",
            &[
                ("GENERIC-FUNCTION-NAME", Reader::GenericName),
                ("GENERIC-FUNCTION-LAMBDA-LIST", Reader::GenericLambdaList),
                (
                    "GENERIC-FUNCTION-ARGUMENT-PRECEDENCE-ORDER",
                    Reader::GenericPrecedence,
                ),
                ("GENERIC-FUNCTION-METHODS", Reader::GenericMethods),
                ("GENERIC-FUNCTION-METHOD-CLASS", Reader::GenericMethodClass),
                ("GENERIC-FUNCTION-DECLARATIONS", Reader::GenericDeclarations),
                (
                    "GENERIC-FUNCTION-DOCUMENTATION",
                    Reader::GenericDocumentation,
                ),
            ][..],
        ),
        (
            "STANDARD-METHOD",
            "(method)",
            &[
                ("METHOD-LAMBDA-LIST", Reader::MethodLambdaList),
                ("METHOD-SPECIALIZERS", Reader::MethodSpecializers),
                ("METHOD-GENERIC-FUNCTION", Reader::MethodOwner),
                ("METHOD-DOCUMENTATION", Reader::MethodDocumentation),
            ][..],
        ),
        (
            "STANDARD-ACCESSOR-METHOD",
            "(method)",
            &[("ACCESSOR-METHOD-SLOT-DEFINITION", Reader::AccessorSlot)][..],
        ),
    ] {
        let lambda_list = form_value::to_value(runtime, &read::read(runtime, signature)?)?;
        for &(name, reader) in readers {
            let symbol = export(runtime, name)?;
            let callback = push_native(Arc::new(move |runtime, args, at| {
                let result = reader.read(runtime, &args[2], at)?;
                Ok(vec![runtime.root(result)])
            }));
            generic::BuiltinGeneric::install(&symbol, lambda_list.clone())?
                .add(&[class], callback)?;
        }
    }
    let name = export(runtime, "COMPUTE-APPLICABLE-METHODS-USING-CLASSES")?;
    let generic =
        generic::BuiltinGeneric::with_signature_source(&name, "(generic-function classes)")?;
    let callback = push_native(Arc::new(|runtime, args, at| {
        let classes = args[3].slot.get();
        let classes = proper_list(runtime, classes, at)
            .map_err(|err| Fault::type_error(runtime, classes, "LIST", err))?
            .into_iter()
            .map(|class| runtime.root_slot(class))
            .collect::<Vec<_>>();
        match generic::applicable_methods_using_classes(runtime, &args[2], &classes, at)? {
            Some(methods) => Ok(vec![
                runtime.root(list_from_words(runtime, methods, at)?),
                runtime.root(Word::T),
            ]),
            None => Ok(vec![runtime.nil(), runtime.nil()]),
        }
    }));
    generic.add(&["STANDARD-GENERIC-FUNCTION", "T"], callback)?;

    install_slot_protocol(runtime)?;
    super::class_definition::initialize(runtime)?;
    super::function_mop::initialize(runtime)
}

fn install_slot_protocol(runtime: &Runtime) -> Result<(), EvalError> {
    let signature = form_value::to_value(
        runtime,
        &read::read(runtime, "(class object slot-definition)")?,
    )?;
    for (name, primitive, setter) in [
        ("SLOT-VALUE-USING-CLASS", "%SLOT-VALUE-USING-CLASS", false),
        ("SLOT-BOUNDP-USING-CLASS", "%SLOT-BOUNDP-USING-CLASS", false),
        (
            "SLOT-MAKUNBOUND-USING-CLASS",
            "%SLOT-MAKUNBOUND-USING-CLASS",
            false,
        ),
        (
            "(setf zk-mop:slot-value-using-class)",
            "%SET-SLOT-VALUE-USING-CLASS",
            true,
        ),
    ] {
        let generic = if setter {
            generic::BuiltinGeneric::setter(
                runtime,
                name,
                "(new-value class object slot-definition)",
            )?
        } else {
            generic::BuiltinGeneric::install(&export(runtime, name)?, signature.clone())?
        };
        let method = Primitive::named(primitive).expect("registered slot protocol primitive");
        for (class, object) in [
            ("STANDARD-CLASS", "T"),
            ("FUNCALLABLE-STANDARD-CLASS", "T"),
            ("CLASS", "CONDITION"),
            ("STRUCTURE-CLASS", "STRUCTURE-OBJECT"),
        ] {
            let specializers = ["T", class, object, "STANDARD-EFFECTIVE-SLOT-DEFINITION"];
            generic.add(
                if setter {
                    &specializers
                } else {
                    &specializers[1..]
                },
                method,
            )?;
        }
    }
    Ok(())
}

pub(super) fn export<'rt>(runtime: &'rt Runtime, name: &str) -> Result<Root<'rt>, EvalError> {
    let (symbol, _) = runtime.intern_package(Package::MOP, name, 0)?;
    runtime
        .packages
        .borrow_mut()
        .insert(Package::MOP, name.into(), symbol, true);
    Ok(runtime.root(symbol))
}

impl Reader {
    fn read(self, runtime: &Runtime, arg: &Root<'_>, at: usize) -> Result<Word, Fault> {
        let word = arg.slot.get();
        let copy_list = |list| list_from_words(runtime, proper_list(runtime, list, at)?, at);
        let class_list = |classes: &[Rc<RootSlot>]| {
            list_from_words(
                runtime,
                classes.iter().map(|class| class.get()).collect(),
                at,
            )
        };
        let pending = || registry::current_update(runtime, word);
        let definition = || {
            pending()
                .and_then(|u| u.definition(runtime, word))
                .or_else(|| registry::class_definition(runtime, word))
        };
        match self {
            Self::ComputePrecedence => {
                class_list(&registry::computed_precedence(runtime, word, at)?)
            }
            Self::ClassPrecedence => {
                if let Some(def) = definition() {
                    class_list(&def.precedence)
                } else {
                    class_list(&classes::class_precedence(runtime, word, at)?)
                }
            }
            Self::ClassDirectSuperclasses => {
                if let Some(update) = pending()
                    && let Some((_, parents)) = update.direct(word)
                {
                    return class_list(parents);
                }
                class_list(&registry::class_direct_superclasses(runtime, word, at)?)
            }
            Self::ClassDirectSubclasses => {
                class_list(&registry::class_direct_subclasses(runtime, word, at)?)
            }
            Self::ClassFinalized => Ok(if finalized(runtime, word) {
                Word::T
            } else {
                Word::NIL
            }),
            Self::FinalizeInheritance => {
                require_finalized(runtime, word, at)?;
                Ok(Word::NIL)
            }
            Self::ClassDirectSlots => {
                if let Some(update) = pending()
                    && let Some((decl, _)) = update.direct(word)
                {
                    return slot_list(runtime, &decl.slots, true, at);
                }
                if let Some(declaration) = registry::class_declaration(runtime, word) {
                    slot_list(runtime, &declaration.slots, true, at)
                } else {
                    system_slot_list(runtime, word, true, at)
                }
            }
            Self::ClassEffectiveSlots => {
                let Some(definition) = definition() else {
                    require_finalized(runtime, word, at)?;
                    return system_slot_list(runtime, word, false, at);
                };
                let mut slots = Vec::with_capacity(definition.slots.len());
                for slot in definition.slots.iter() {
                    slots.push(effective_slot(runtime, slot, at)?);
                }
                list_from_words(runtime, slots.iter().map(|slot| slot.get()).collect(), at)
            }
            Self::ClassPrototype => {
                require_finalized(runtime, word, at)?;
                if let Some(def) = definition() {
                    if let Some(prototype) = def.prototype.get() {
                        return Ok(prototype.get());
                    }
                    let value = super::instances::allocate_standard_instance(
                        runtime,
                        &arg.slot,
                        &def,
                        Word::NIL,
                        at,
                    )?;
                    let prototype = def.prototype.get_or_init(|| Rc::new(runtime.root_slot(value)));
                    return Ok(prototype.get());
                }
                let info = runtime.heap.class_info(word).expect("class object");
                let serial = info.serial;
                if let Some(cached) = runtime.classes.borrow().prototypes.get(&serial) {
                    return Ok(cached.get());
                }
                let prototype = if let Some(class) = builtin_condition(runtime, word) {
                    condition::builtin_prototype(runtime, class, at)?
                } else if let Some(prototype) =
                    classes::builtin_class_prototype(runtime, info.origin_name, at)?
                {
                    prototype
                } else {
                    super::instances::allocate_instance(runtime, &arg.slot, Word::NIL, at)?
                };
                runtime
                    .classes
                    .borrow_mut()
                    .prototypes
                    .insert(serial, runtime.root_slot(prototype));
                Ok(prototype)
            }
            Self::ClassDirectDefaultInitargs => {
                if let Some(update) = pending()
                    && let Some((decl, _)) = update.direct(word)
                {
                    return initargs(runtime, &decl.default_initargs, at);
                }
                registry::class_declaration(runtime, word).map_or(Ok(Word::NIL), |class| {
                    initargs(runtime, &class.default_initargs, at)
                })
            }
            Self::ClassDefaultInitargs => {
                if let Some(def) = definition() {
                    return initargs(runtime, &def.default_initargs, at);
                }
                require_finalized(runtime, word, at)?;
                Ok(Word::NIL)
            }
            Self::GenericName
            | Self::GenericLambdaList
            | Self::GenericPrecedence
            | Self::GenericMethods
            | Self::GenericMethodClass
            | Self::GenericDeclarations
            | Self::GenericDocumentation => {
                let info = runtime
                    .heap
                    .generic_info(word)
                    .expect("standard generic function");
                match self {
                    Self::GenericName => Ok(info.name),
                    Self::GenericLambdaList if info.lambda_list == Word::UNBOUND => {
                        Err(problem(at, "generic function has no lambda list"))
                    }
                    Self::GenericLambdaList => {
                        super::list::copy_tree(runtime, info.lambda_list, at)
                    }
                    Self::GenericPrecedence if info.precedence == Word::NIL => {
                        list_from_words(runtime, required_parameters(runtime, info.lambda_list), at)
                    }
                    Self::GenericPrecedence => copy_list(info.precedence),
                    Self::GenericMethods => copy_list(info.methods),
                    Self::GenericMethodClass => {
                        let [class, _, _] = runtime
                            .heap
                            .generic_options(word)
                            .expect("generic function");
                        if class == Word::NIL {
                            classes::builtin_class(runtime, "STANDARD-METHOD", at)
                        } else {
                            Ok(class)
                        }
                    }
                    Self::GenericDeclarations => super::list::copy_tree(
                        runtime,
                        runtime.heap.generic_options(word).unwrap()[2],
                        at,
                    ),
                    Self::GenericDocumentation => Ok(info.documentation),
                    _ => unreachable!(),
                }
            }
            Self::AccessorSlot => Ok(runtime
                .heap
                .method_info(word)
                .expect("accessor method")
                .accessor_slot),
            Self::MethodLambdaList
            | Self::MethodSpecializers
            | Self::MethodOwner
            | Self::MethodDocumentation => {
                let info = runtime.heap.method_info(word).expect("standard method");
                match self {
                    Self::MethodLambdaList => copy_list(info.lambda_list),
                    Self::MethodSpecializers => copy_list(info.specializers),
                    Self::MethodOwner => Ok(info.owner),
                    Self::MethodDocumentation => Ok(info.documentation),
                    _ => unreachable!(),
                }
            }
            _ => {
                let info = runtime
                    .heap
                    .slot_definition_info(word)
                    .expect("slot definition");
                match self {
                    Self::SlotName => Ok(info.name),
                    Self::SlotAllocation => Ok(info.allocation),
                    Self::SlotLocation => Ok(info.location),
                    Self::SlotType => Ok(info.type_specifier),
                    Self::SlotInitargs => copy_list(info.initargs),
                    Self::SlotInitform => Ok(info.initform),
                    Self::SlotInitfunction => Ok(info.initfunction),
                    Self::SlotReaders => copy_list(info.readers),
                    Self::SlotWriters => copy_list(info.writers),
                    Self::SlotDocumentation => Ok(info.documentation),
                    _ => unreachable!(),
                }
            }
        }
    }
}

fn optional(root: &Option<Rc<RootSlot>>) -> Word {
    root.as_ref().map_or(Word::NIL, |root| root.get())
}

fn builtin_condition(runtime: &Runtime, class: Word) -> Option<Class> {
    classes::builtin_condition(runtime, runtime.heap.class_info(class)?.origin_name)
}

fn system_slot_list(
    runtime: &Runtime,
    class: Word,
    direct: bool,
    at: usize,
) -> Result<Word, Fault> {
    let info = runtime.heap.class_info(class).expect("class object");
    if matches!(info.kind, ClassKind::Structure)
        && let Some(structure) = runtime.structure_definition(info.origin_name)
    {
        return structure_slot_list(runtime, &structure, direct, at);
    }
    let Some(kind) = classes::builtin_condition(runtime, info.origin_name) else {
        return Ok(Word::NIL);
    };
    if direct {
        let slots = condition::builtin_direct_slots(runtime, kind, at)?;
        return slot_list(runtime, &slots, true, at);
    }
    let owner = runtime.root_slot(class);
    let slots = condition::builtin_effective_slots(runtime, kind, at)?;
    let metas = slots
        .iter()
        .map(|slot| condition_effective_slot(runtime, &owner, slot, at))
        .collect::<Result<Vec<_>, _>>()?;
    list_from_words(runtime, metas.iter().map(|m| m.get()).collect(), at)
}

fn structure_slot_list(
    runtime: &Runtime,
    structure: &StructureDefinition,
    direct: bool,
    at: usize,
) -> Result<Word, Fault> {
    let mut metaobjects = Vec::new();
    for (index, slot) in structure.slots.iter().enumerate() {
        if direct && !slot.direct {
            continue;
        }
        metaobjects.push(structure_slot(runtime, structure, slot, index, direct, at)?);
    }
    list_from_words(
        runtime,
        metaobjects.into_iter().map(RootSlot::get).collect(),
        at,
    )
}

pub(super) fn structure_slot<'a>(
    runtime: &Runtime,
    structure: &StructureDefinition,
    slot: &'a StructureSlot,
    index: usize,
    direct: bool,
    at: usize,
) -> Result<&'a RootSlot, Fault> {
    let cache = if direct {
        &slot.direct_metaobject
    } else {
        &slot.effective_metaobject
    };
    if let Some(metaobject) = cache.get() {
        return Ok(metaobject);
    }
    let initarg = if structure.keyword_constructor.get() != Word::NIL {
        let name = runtime
            .heap
            .symbol_name(slot.name.get())
            .expect("slot name");
        Some(Rc::new(runtime.root_slot(
            runtime.intern(&Name::keyword(&name), at)?,
        )))
    } else {
        None
    };
    let reader = runtime.root_slot(if direct && slot.reader.get() != Word::NIL {
        list_from_words(runtime, vec![slot.reader.get()], at)?
    } else {
        Word::NIL
    });
    let type_specifier = Rc::new(runtime.root_slot(slot.type_specifier.get()));
    make_slot(
        runtime,
        cache,
        &slot.name,
        initarg.as_slice(),
        SlotKind::Instance,
        &[type_specifier],
        direct,
        at,
        |info| {
            info.initform = slot.initform.get();
            info.readers = reader.get();
            if !direct {
                info.location = Word::fixnum(index as isize).expect("structure slot index");
            }
        },
    )
}

pub(super) fn condition_effective_slot<'a>(
    runtime: &Runtime,
    owner: &RootSlot,
    slot: &'a EffectiveSlot,
    at: usize,
) -> Result<&'a RootSlot, Fault> {
    let fresh = slot.metaobject.get().is_none();
    let object = effective_slot(runtime, slot, at)?;
    if fresh {
        let SlotAllocation::Instance(index) = slot.allocation else {
            unreachable!("compact condition slot")
        };
        runtime.heap.set_slot_location(
            object.get(),
            owner.get(),
            Word::fixnum(index as isize).unwrap(),
        );
    }
    Ok(object)
}

fn slot_list(
    runtime: &Runtime,
    slots: &[DirectSlot],
    direct: bool,
    at: usize,
) -> Result<Word, Fault> {
    let mut metaobjects = Vec::with_capacity(slots.len());
    for slot in slots {
        metaobjects.push(slot_metaobject(runtime, slot, direct, at)?);
    }
    list_from_words(
        runtime,
        metaobjects.iter().map(|slot| slot.get()).collect(),
        at,
    )
}

pub(super) fn slot_metaobject<'a>(
    runtime: &Runtime,
    slot: &'a DirectSlot,
    direct: bool,
    at: usize,
) -> Result<&'a RootSlot, Fault> {
    make_slot(
        runtime,
        &slot.metaobject,
        &slot.name,
        &slot.initargs,
        slot.allocation.clone(),
        slot.type_specifier.as_slice(),
        direct,
        at,
        |info| {
            info.initform = optional(&slot.initform_source);
            info.initfunction = optional(&slot.initform);
            info.readers = optional(&slot.readers);
            info.writers = optional(&slot.writers);
            info.documentation = optional(&slot.documentation);
        },
    )
}

pub(super) fn effective_slot<'a>(
    runtime: &Runtime,
    slot: &'a EffectiveSlot,
    at: usize,
) -> Result<&'a RootSlot, Fault> {
    let allocation = match &slot.allocation {
        SlotAllocation::Custom(name) => SlotKind::Custom(Rc::clone(name)),
        SlotAllocation::Class(_) => SlotKind::Class,
        SlotAllocation::Instance(_) => SlotKind::Instance,
    };
    make_slot(
        runtime,
        &slot.metaobject,
        &slot.name,
        &slot.initargs,
        allocation,
        &slot.type_constraints,
        false,
        at,
        |info| {
            info.location = match slot.allocation {
                SlotAllocation::Instance(index) => {
                    Word::fixnum(index as isize).expect("allocated slot index")
                }
                SlotAllocation::Class(_) | SlotAllocation::Custom(_) => Word::NIL,
            };
            info.initform = optional(&slot.initform_source);
            info.initfunction = optional(&slot.initform);
            info.documentation = optional(&slot.documentation);
        },
    )
}

fn finalized(runtime: &Runtime, class: Word) -> bool {
    if let Some(update) = registry::current_update(runtime, class) {
        return update.finalized(runtime, class);
    }
    let info = runtime.heap.class_info(class).expect("class object");
    registry::class_definition(runtime, class).is_some()
        || classes::is_system_class(runtime, info.origin_name)
        || matches!(info.kind, ClassKind::Structure)
}

fn require_finalized(runtime: &Runtime, class: Word, at: usize) -> Result<(), Fault> {
    if finalized(runtime, class) {
        Ok(())
    } else {
        Err(problem(at, "class has undefined superclasses"))
    }
}

fn initargs(runtime: &Runtime, initargs: &[DefaultInitarg], at: usize) -> Result<Word, Fault> {
    let mut tuples = Vec::with_capacity(initargs.len());
    for arg in initargs {
        let tuple = list_from_words(
            runtime,
            vec![
                arg.name.get(),
                arg.initform_source.get(),
                arg.initform.get(),
            ],
            at,
        )?;
        tuples.push(runtime.root_slot(tuple));
    }
    list_from_words(runtime, tuples.iter().map(RootSlot::get).collect(), at)
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal MOP slot cache constructor receives slot attributes directly"
)]
fn make_slot<'a>(
    runtime: &Runtime,
    cache: &'a OnceCell<RootSlot>,
    name: &RootSlot,
    initargs: &[Rc<RootSlot>],
    allocation: SlotKind,
    types: &[Rc<RootSlot>],
    direct: bool,
    at: usize,
    fill: impl FnOnce(&mut SlotFields),
) -> Result<&'a RootSlot, Fault> {
    if let Some(slot) = cache.get() {
        return Ok(slot);
    }
    let allocation = runtime.root_slot(match allocation {
        SlotKind::Class => runtime.intern(&Name::keyword("CLASS"), at)?,
        SlotKind::Instance => runtime.intern(&Name::keyword("INSTANCE"), at)?,
        SlotKind::Custom(name) => name.get(),
    });
    let initargs = runtime.root_slot(list_from_words(
        runtime,
        initargs.iter().map(|arg| arg.get()).collect(),
        at,
    )?);
    let type_specifier = runtime.root_slot(match types {
        [] => Word::T,
        [single] => single.get(),
        many => {
            let and = runtime.intern(&Name::common_lisp("AND"), at)?;
            list_from_words(
                runtime,
                std::iter::once(and)
                    .chain(many.iter().map(|ty| ty.get()))
                    .collect(),
                at,
            )?
        }
    });
    let slot = runtime
        .heap
        .slot_definition(|| {
            let mut info = SlotFields {
                name: name.get(),
                initargs: initargs.get(),
                allocation: allocation.get(),
                type_specifier: type_specifier.get(),
                direct,
                location: Word::NIL,
                initfunction: Word::NIL,
                initform: Word::NIL,
                readers: Word::NIL,
                writers: Word::NIL,
                documentation: Word::NIL,
            };
            fill(&mut info);
            info
        })
        .map_err(|err| EvalError::out_of_memory(at, err))?;
    Ok(cache.get_or_init(|| runtime.root_slot(slot)))
}

fn problem(at: usize, message: &str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}
