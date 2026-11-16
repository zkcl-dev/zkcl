use num_rational::BigRational;

use crate::condition::{Class, Fault};
use crate::heap::{
    ArrayElement, ClassFields, ClassKind, HashTest, RandomState, ReadtableCase, RootSlot,
    RootSlots, StreamKind, Word,
};
use crate::number::{Number, NumberKind, Real};
use crate::pathname::Pathname;
use crate::symbol::{Name, Package};
use crate::{EvalError, Runtime};
use std::ops::Range;
use std::rc::Rc;

use crate::classes::{self, Representation};
pub(crate) use crate::classes::{
    DirectSlot, RuntimeClassKind, SlotKind, class_declaration, class_definition, class_precedence,
};

use super::{Primitive, is_function};

macro_rules! system_classes {
    ($($variant:ident ($name:literal, $metaclass:ident) => [$($parent:expr),*];)*) => {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum SystemClass {
            $($variant,)*
            Condition(Class),
        }

        impl SystemClass {
            fn all() -> impl Iterator<Item = Self> {
                [$(Self::$variant,)*].into_iter().chain(Class::all().map(Self::Condition))
            }

            fn named(name: &str) -> Option<Self> {
                Some(match name {
                    $($name => Self::$variant,)*
                    _ => Self::Condition(Class::named(name)?),
                })
            }

            fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                    Self::Condition(class) => class.name(),
                }
            }

            fn metaclass(self) -> ClassKind {
                match self {
                    $(Self::$variant => ClassKind::$metaclass,)*
                    Self::Condition(_) => ClassKind::Standard,
                }
            }

            fn standard_package(self) -> Option<Package> {
                match self {
                    Self::SimdPack | Self::SimdMask => None,
                    Self::Treelist | Self::MutableTreelist => Some(Package::TREELIST),
                    Self::FuncallableStandardClass | Self::FuncallableStandardObject | Self::Metaobject | Self::Specializer | Self::EqlSpecializer | Self::ForwardReferencedClass |
                    Self::SlotDefinition | Self::DirectSlotDefinition | Self::EffectiveSlotDefinition |
                    Self::StandardSlotDefinition | Self::StandardDirectSlotDefinition |
                    Self::StandardEffectiveSlotDefinition | Self::StandardAccessorMethod | Self::StandardReaderMethod | Self::StandardWriterMethod => Some(Package::MOP),
                    _ => Some(Package::COMMON_LISP),
                }
            }

            fn symbol(self, runtime: &Runtime, at: usize) -> Result<Word, EvalError> {
                if let Some(package) = self.standard_package() {
                    Ok(runtime.intern_package(package, self.name(), at)?.0)
                } else {
                    Ok(self.simd_symbol(runtime).expect("SIMD types initialized"))
                }
            }

            fn simd_symbol(self, runtime: &Runtime) -> Option<Word> {
                let symbols = runtime.simd_type_symbols.get()?;
                match self {
                    Self::SimdPack => Some(symbols.pack()),
                    Self::SimdMask => Some(symbols.mask()),
                    _ => None,
                }
            }

            fn is_a(self, expected: Self) -> bool {
                self == expected || self.parents().any(|parent| parent.is_a(expected))
            }

            fn parents(self) -> impl Iterator<Item = Self> {
                let (system, conditions): (&[Self], &[Class]) = match self {
                    $(Self::$variant => (&[$($parent),*], &[]),)*
                    Self::Condition(Class::Condition) => (&[Self::T], &[]),
                    Self::Condition(class) => (&[], class.direct_superclasses()),
                };
                system.iter().copied().chain(conditions.iter().copied().map(Self::Condition))
            }
        }
    };
}

// The CLHS Figure 4-8 classes, MOP classes, and zkcl's floating-point classes.
// SHORT-FLOAT and LONG-FLOAT are type aliases for the two represented formats.
// Condition inheritance is defined in Class.
system_classes! {
    T ("T", BuiltIn) => [];
    Array ("ARRAY", BuiltIn) => [Self::T];
    BitVector ("BIT-VECTOR", BuiltIn) => [Self::Vector];
    BroadcastStream ("BROADCAST-STREAM", BuiltIn) => [Self::Stream];
    BuiltInClass ("BUILT-IN-CLASS", Standard) => [Self::Class];
    Character ("CHARACTER", BuiltIn) => [Self::T];
    Class ("CLASS", Standard) => [Self::Specializer];
    Complex ("COMPLEX", BuiltIn) => [Self::Number];
    ConcatenatedStream ("CONCATENATED-STREAM", BuiltIn) => [Self::Stream];
    Cons ("CONS", BuiltIn) => [Self::List];
    DoubleFloat ("DOUBLE-FLOAT", BuiltIn) => [Self::Float];
    EchoStream ("ECHO-STREAM", BuiltIn) => [Self::Stream];
    EffectiveSlotDefinition ("EFFECTIVE-SLOT-DEFINITION", Standard) => [Self::SlotDefinition];
    EqlSpecializer ("EQL-SPECIALIZER", Standard) => [Self::Specializer];
    FileStream ("FILE-STREAM", BuiltIn) => [Self::Stream];
    Float ("FLOAT", BuiltIn) => [Self::Real];
    ForwardReferencedClass ("FORWARD-REFERENCED-CLASS", Standard) => [Self::Class];
    Function ("FUNCTION", BuiltIn) => [Self::T];
    FuncallableStandardClass ("FUNCALLABLE-STANDARD-CLASS", Standard) => [Self::Class];
    FuncallableStandardObject ("FUNCALLABLE-STANDARD-OBJECT", Standard) => [Self::Function, Self::StandardObject];
    GenericFunction ("GENERIC-FUNCTION", Standard) => [Self::Metaobject, Self::FuncallableStandardObject];
    HashTable ("HASH-TABLE", BuiltIn) => [Self::T];
    Integer ("INTEGER", BuiltIn) => [Self::Rational];
    List ("LIST", BuiltIn) => [Self::Sequence];
    LogicalPathname ("LOGICAL-PATHNAME", BuiltIn) => [Self::Pathname];
    Method ("METHOD", Standard) => [Self::Metaobject];
    Metaobject ("METAOBJECT", Standard) => [Self::StandardObject];
    MethodCombination ("METHOD-COMBINATION", Standard) => [Self::Metaobject];
    Null ("NULL", BuiltIn) => [Self::Symbol, Self::List];
    Number ("NUMBER", BuiltIn) => [Self::T];
    Package ("PACKAGE", BuiltIn) => [Self::T];
    Pathname ("PATHNAME", BuiltIn) => [Self::T];
    RandomState ("RANDOM-STATE", BuiltIn) => [Self::T];
    Ratio ("RATIO", BuiltIn) => [Self::Rational];
    Rational ("RATIONAL", BuiltIn) => [Self::Real];
    Readtable ("READTABLE", BuiltIn) => [Self::T];
    Real ("REAL", BuiltIn) => [Self::Number];
    Restart ("RESTART", BuiltIn) => [Self::T];
    Sequence ("SEQUENCE", BuiltIn) => [Self::T];
    SimdPack ("PACK", BuiltIn) => [Self::T];
    SimdMask ("MASK", BuiltIn) => [Self::T];
    SingleFloat ("SINGLE-FLOAT", BuiltIn) => [Self::Float];
    SlotDefinition ("SLOT-DEFINITION", Standard) => [Self::Metaobject];
    DirectSlotDefinition ("DIRECT-SLOT-DEFINITION", Standard) => [Self::SlotDefinition];
    Specializer ("SPECIALIZER", Standard) => [Self::Metaobject];
    StandardClass ("STANDARD-CLASS", Standard) => [Self::Class];
    StandardDirectSlotDefinition ("STANDARD-DIRECT-SLOT-DEFINITION", Standard) => [Self::StandardSlotDefinition, Self::DirectSlotDefinition];
    StandardEffectiveSlotDefinition ("STANDARD-EFFECTIVE-SLOT-DEFINITION", Standard) => [Self::StandardSlotDefinition, Self::EffectiveSlotDefinition];
    StandardGenericFunction ("STANDARD-GENERIC-FUNCTION", Standard) => [Self::GenericFunction];
    StandardAccessorMethod ("STANDARD-ACCESSOR-METHOD", Standard) => [Self::StandardMethod];
    StandardReaderMethod ("STANDARD-READER-METHOD", Standard) => [Self::StandardAccessorMethod];
    StandardWriterMethod ("STANDARD-WRITER-METHOD", Standard) => [Self::StandardAccessorMethod];
    StandardMethod ("STANDARD-METHOD", Standard) => [Self::Method];
    StandardObject ("STANDARD-OBJECT", Standard) => [Self::T];
    StandardSlotDefinition ("STANDARD-SLOT-DEFINITION", Standard) => [Self::SlotDefinition];
    Stream ("STREAM", BuiltIn) => [Self::T];
    String ("STRING", BuiltIn) => [Self::Vector];
    StringStream ("STRING-STREAM", BuiltIn) => [Self::Stream];
    StructureClass ("STRUCTURE-CLASS", Standard) => [Self::Class];
    StructureObject ("STRUCTURE-OBJECT", Structure) => [Self::T];
    Symbol ("SYMBOL", BuiltIn) => [Self::T];
    SynonymStream ("SYNONYM-STREAM", BuiltIn) => [Self::Stream];
    TwoWayStream ("TWO-WAY-STREAM", BuiltIn) => [Self::Stream];
    Treelist ("TREELIST", BuiltIn) => [Self::Sequence];
    MutableTreelist ("MUTABLE-TREELIST", BuiltIn) => [Self::Sequence];
    Vector ("VECTOR", BuiltIn) => [Self::Array, Self::Sequence];
}

/// A direct class, before its canonical Lisp object is fetched or allocated.
#[derive(Clone, Copy)]
enum DirectClass {
    System(SystemClass),
    Structure(Word),
    Standard(Word),
}

fn direct_class(runtime: &Runtime, object: Word) -> DirectClass {
    use SystemClass::*;

    let heap = &runtime.heap;
    if let Some(class) = heap.instance_class(object) {
        return DirectClass::Standard(class);
    }
    if let Some((name, _)) = heap.structure_layout(object) {
        return DirectClass::Structure(name);
    }
    let class = if let Some(class) = heap.class_info(object) {
        match class.kind {
            ClassKind::BuiltIn => BuiltInClass,
            ClassKind::Standard
                if matches!(
                    system_class(runtime, class.origin_name),
                    Some(GenericFunction | StandardGenericFunction | FuncallableStandardObject)
                ) =>
            {
                FuncallableStandardClass
            }
            // A condition class is a class whose metaclass is a subclass of
            // CLASS but not of STANDARD-CLASS (CLHS 4.3.7.1); the runtime has
            // no separate metaclass name, so the class of a condition class
            // object is CLASS.
            ClassKind::Standard
                if is_condition_class(runtime, class.origin_name)
                    || class_declaration(runtime, object)
                        .is_some_and(|decl| decl.kind == RuntimeClassKind::Condition) =>
            {
                SystemClass::Class
            }
            ClassKind::Standard => StandardClass,
            ClassKind::Structure => StructureClass,
            ClassKind::ForwardReferenced => ForwardReferencedClass,
        }
    } else if let Some(kind) = heap.number_kind(object) {
        match kind {
            NumberKind::Fixnum | NumberKind::Bignum => Integer,
            NumberKind::Ratio => Ratio,
            NumberKind::Single => SingleFloat,
            NumberKind::Double => DoubleFloat,
            NumberKind::Complex => Complex,
        }
    } else if heap.simd_pack_shape(object).is_some() {
        SimdPack
    } else if heap.simd_mask(object).is_some() {
        SimdMask
    } else if heap.is_treelist(object) {
        Treelist
    } else if heap.is_mutable_treelist(object) {
        MutableTreelist
    } else if object == Word::NIL {
        Null
    } else if object.as_character().is_some() {
        Character
    } else if heap.is_symbol(object) {
        Symbol
    } else if heap.cons_fields(object).is_some() {
        Cons
    } else if heap.is_string(object) {
        String
    } else if let Some(array) = heap.array_info(object) {
        match (array.rank, array.element) {
            (1, ArrayElement::Bit) => BitVector,
            (1, ArrayElement::Character) => String,
            (1, _) => Vector,
            _ => Array,
        }
    } else if let Some(class) = heap.condition_class(object) {
        Condition(class)
    } else if let Some(fields) = heap.pathname_fields(object) {
        if heap.is_string(fields[0]) {
            LogicalPathname
        } else {
            Pathname
        }
    } else if object.package_index().is_some() {
        Package
    } else if heap.restart_name(object).is_some() {
        Restart
    } else if heap.hash_table_info(object).is_some() {
        HashTable
    } else if heap.is_random_state(object) {
        RandomState
    } else if heap.readtable_case(object).is_some() {
        Readtable
    } else if let Some(stream) = heap.stream_info(object) {
        match stream.kind {
            StreamKind::StringInput | StreamKind::StringOutput | StreamKind::StringOutputTo => {
                StringStream
            }
            StreamKind::FileInput
            | StreamKind::FileOutput
            | StreamKind::FileIo
            | StreamKind::FileProbe => FileStream,
            StreamKind::Broadcast => BroadcastStream,
            StreamKind::Concatenated => ConcatenatedStream,
            StreamKind::Echo => EchoStream,
            StreamKind::Synonym => SynonymStream,
            StreamKind::TwoWay => TwoWayStream,
            StreamKind::StandardInput
            | StreamKind::StandardOutput
            | StreamKind::ErrorOutput
            | StreamKind::Terminal => Stream,
        }
    } else if heap.generic_info(object).is_some() {
        StandardGenericFunction
    } else if let Some(method) = heap.method_info(object) {
        if method.accessor_slot == Word::NIL {
            StandardMethod
        } else if heap
            .cons_fields(method.specializers)
            .expect("accessor specializers")
            .1
            == Word::NIL
        {
            StandardReaderMethod
        } else {
            StandardWriterMethod
        }
    } else if heap.eql_specializer_object(object).is_some() {
        EqlSpecializer
    } else if let Some(slot) = heap.slot_definition_info(object) {
        if slot.direct {
            StandardDirectSlotDefinition
        } else {
            StandardEffectiveSlotDefinition
        }
    } else if is_function(runtime, object) {
        Function
    } else {
        T
    };
    DirectClass::System(class)
}

fn system_class(runtime: &Runtime, name: Word) -> Option<SystemClass> {
    if let Some(symbols) = runtime.simd_type_symbols.get() {
        if name == symbols.pack() {
            return Some(SystemClass::SimdPack);
        }
        if name == symbols.mask() {
            return Some(SystemClass::SimdMask);
        }
    }
    let package = runtime.heap.symbol_home(name)?;
    if !matches!(
        package,
        Package::COMMON_LISP | Package::MOP | Package::TREELIST
    ) {
        return None;
    }
    let class = SystemClass::named(&runtime.heap.symbol_name(name)?)?;
    (class.standard_package() == Some(package)).then_some(class)
}

pub(super) fn export_mop_classes(runtime: &Runtime) -> Result<(), EvalError> {
    for class in [
        SystemClass::FuncallableStandardClass,
        SystemClass::FuncallableStandardObject,
        SystemClass::Metaobject,
        SystemClass::Specializer,
        SystemClass::EqlSpecializer,
        SystemClass::ForwardReferencedClass,
        SystemClass::SlotDefinition,
        SystemClass::DirectSlotDefinition,
        SystemClass::EffectiveSlotDefinition,
        SystemClass::StandardSlotDefinition,
        SystemClass::StandardDirectSlotDefinition,
        SystemClass::StandardEffectiveSlotDefinition,
        SystemClass::StandardAccessorMethod,
        SystemClass::StandardReaderMethod,
        SystemClass::StandardWriterMethod,
    ] {
        let word = class.symbol(runtime, 0)?;
        runtime
            .packages
            .borrow_mut()
            .insert(Package::MOP, class.name().into(), word, true);
    }
    Ok(())
}

pub(crate) fn builtin_class(runtime: &Runtime, spelling: &str, at: usize) -> Result<Word, Fault> {
    let name = SystemClass::named(spelling)
        .expect("built-in class name")
        .symbol(runtime, at)?;
    Ok(canonical_class(runtime, name, at)?.expect("built-in class object"))
}

pub(crate) fn is_system_class(runtime: &Runtime, name: Word) -> bool {
    system_class(runtime, name).is_some()
}

pub(crate) fn is_condition_class(runtime: &Runtime, name: Word) -> bool {
    system_class(runtime, name)
        .is_some_and(|class| class.is_a(SystemClass::Condition(Class::Condition)))
}

pub(crate) fn builtin_condition(runtime: &Runtime, name: Word) -> Option<Class> {
    match system_class(runtime, name)? {
        SystemClass::Condition(class) => Some(class),
        _ => None,
    }
}

/// A representative value built without borrowing mutable application state.
pub(super) fn builtin_class_prototype(
    runtime: &Runtime,
    name: Word,
    at: usize,
) -> Result<Option<Word>, Fault> {
    let Some(class) = system_class(runtime, name) else {
        return Ok(None);
    };
    let heap = &runtime.heap;
    let value = match class {
        SystemClass::T | SystemClass::Null | SystemClass::List => return Ok(Some(Word::NIL)),
        SystemClass::Symbol => return Ok(Some(runtime.fresh_symbol("SYMBOL", at)?.1)),
        SystemClass::Character => return Ok(Some(Word::character(' '))),
        SystemClass::Function => {
            return Ok(Some(Primitive::named("IDENTITY").expect("core primitive")));
        }
        SystemClass::Pathname => {
            return Ok(Some(
                Pathname::default().to_lisp_value(runtime, at)?.slot.get(),
            ));
        }
        SystemClass::Cons => heap.cons(|| (Word::NIL, Word::NIL)),
        SystemClass::Array => {
            heap.array(&[0, 0], ArrayElement::General, false, None, |_| Word::NIL)
        }
        SystemClass::Vector | SystemClass::Sequence => {
            heap.array(&[0], ArrayElement::General, false, None, |_| Word::NIL)
        }
        SystemClass::BitVector => heap.array(&[0], ArrayElement::Bit, false, None, |_| {
            Word::fixnum(0).expect("zero bit")
        }),
        SystemClass::String => heap.string(""),
        SystemClass::HashTable => heap.hash_table(
            HashTest::Eql,
            None,
            0,
            zkcl_bytecode::RehashHint::integer(8),
            zkcl_bytecode::RehashHint::integer(1),
        ),
        SystemClass::Readtable => heap.readtable(ReadtableCase::Upcase),
        SystemClass::RandomState => heap.allocate_random_state(RandomState::INITIAL),
        SystemClass::Number | SystemClass::Real | SystemClass::Rational | SystemClass::Integer => {
            return Ok(Some(Word::fixnum(0).expect("zero fixnum")));
        }
        SystemClass::Ratio => heap.allocate_number(&Number::Real(Real::Ratio(BigRational::new(
            1.into(),
            2.into(),
        )))),
        SystemClass::Complex => heap.allocate_number(&Number::Complex(
            Real::Integer(0.into()),
            Real::Integer(1.into()),
        )),
        SystemClass::Float | SystemClass::SingleFloat => {
            heap.allocate_number(&Number::Real(Real::Single(0.0)))
        }
        SystemClass::DoubleFloat => heap.allocate_number(&Number::Real(Real::Double(0.0))),
        SystemClass::Treelist => heap.treelist_empty(),
        SystemClass::MutableTreelist => heap.mutable_treelist_empty(),
        _ => return Ok(None),
    };
    value
        .map(Some)
        .map_err(|error| EvalError::out_of_memory(at, error).into())
}

pub(crate) fn system_parents(
    runtime: &Runtime,
    name: Word,
    at: usize,
) -> Result<Option<Vec<Rc<RootSlot>>>, Fault> {
    if let Some(class) = system_class(runtime, name) {
        let parents = class
            .parents()
            .map(|parent| {
                let name = parent.symbol(runtime, at)?;
                Ok(Rc::new(runtime.root_slot(name)))
            })
            .collect::<Result<_, Fault>>()?;
        return Ok(Some(parents));
    }
    if let Some(structure) = runtime.structure_definition(name) {
        let parent = if let Some(parent) = &structure.parent {
            parent.get()
        } else {
            runtime.intern(&Name::common_lisp("STRUCTURE-OBJECT"), at)?
        };
        return Ok(Some(vec![Rc::new(runtime.root_slot(parent))]));
    }
    Ok(None)
}

/// Built-in and DEFSTRUCT class objects are created on demand. Enumerate
/// their definitions so reflection also finds classes never looked up before.
pub(crate) fn system_subclass_names(
    runtime: &Runtime,
    class: Word,
    at: usize,
) -> Result<Vec<Rc<RootSlot>>, Fault> {
    let info = runtime.heap.class_info(class).expect("class object");
    let system = system_class(runtime, info.origin_name);
    let mut names = if matches!(info.kind, ClassKind::Structure) {
        let included = (system != Some(SystemClass::StructureObject)).then_some(info.origin_name);
        runtime.structure_subclass_names(included)
    } else {
        Vec::new()
    };
    if let Some(parent) = system {
        for child in SystemClass::all().filter(|child| child.parents().any(|p| p == parent)) {
            names.push(Rc::new(runtime.root_slot(child.symbol(runtime, at)?)));
        }
    }
    Ok(names)
}

/// Decides SUBTYPEP for type specifiers that name classes.
///
/// The built-in class graph, structure definitions, and finalized standard
/// class precedence lists are closed, so a missing path between two known
/// classes is a definite negative.  Unknown names stay undecided.
pub(crate) fn class_subtypep(runtime: &Runtime, sub: Word, sup: Word) -> Option<bool> {
    if runtime.heap.class_info(sub).is_some()
        || runtime.heap.class_info(sup).is_some()
        || associated_class(runtime, sub).is_some()
        || associated_class(runtime, sup).is_some()
    {
        return class_designators_subtypep(runtime, sub, sup);
    }
    if sub == sup || sup == Word::T || sub == Word::NIL {
        return Some(true);
    }
    if !runtime.heap.is_symbol(sub) || !runtime.heap.is_symbol(sup) {
        return None;
    }
    let sub_system = system_class(runtime, sub);
    let sup_system = system_class(runtime, sup);
    if let (Some(sub_class), Some(sup_class)) = (sub_system, sup_system) {
        return Some(sub_class.is_a(sup_class));
    }

    let sub_structure = runtime.structure_definition(sub).is_some();
    let sup_structure = runtime.structure_definition(sup).is_some();
    if sub_structure {
        if let Some(sup_class) = sup_system {
            return Some(sup_class == SystemClass::StructureObject);
        }
        if sup_structure {
            return Some(runtime.structure_subtype(sub, sup));
        }
        // A structure class cannot inherit from a standard class.
        return Some(false);
    }
    if sup_structure {
        return Some(false);
    }

    if classes::declared(runtime, sub) {
        return class_designators_subtypep(runtime, sub, sup);
    }
    if sub_system.is_some() && classes::declared(runtime, sup) {
        return Some(false);
    }
    None
}

fn class_designators_subtypep(runtime: &Runtime, sub: Word, sup: Word) -> Option<bool> {
    let resolve = |designator| {
        if runtime.heap.class_info(designator).is_some() {
            Some(designator)
        } else {
            find_named_class(runtime, designator, 0).ok()?
        }
    };
    let sup = runtime.root_slot(sup);
    let sub = runtime.root_slot(resolve(sub)?);
    let sup = resolve(sup.get())?;
    class_objects_subtypep(runtime, sub.get(), sup, 0)
}

fn associated_class(runtime: &Runtime, name: Word) -> Option<Option<Word>> {
    let id = runtime.heap.symbol_attributes(name)?.id;
    let registry = runtime.classes.borrow();
    registry
        .associations
        .get(&id)
        .map(|association| association.as_ref().map(|class| class.get()))
}

fn class_objects_subtypep(runtime: &Runtime, sub: Word, sup: Word, depth: usize) -> Option<bool> {
    if depth > 64 {
        return None;
    }
    let sub_info = runtime.heap.class_info(sub)?;
    let sup_info = runtime.heap.class_info(sup)?;
    if sub_info.serial == sup_info.serial {
        return Some(true);
    }

    let sub_system = system_class(runtime, sub_info.origin_name);
    let sup_system = system_class(runtime, sup_info.origin_name);
    if let (Some(sub_class), Some(sup_class)) = (sub_system, sup_system) {
        return Some(sub_class.is_a(sup_class));
    }

    if matches!(sub_info.kind, ClassKind::Structure) {
        if sup_system == Some(SystemClass::StructureObject) {
            return Some(true);
        }
        return if matches!(sup_info.kind, ClassKind::Structure) {
            Some(runtime.structure_subtype(sub_info.origin_name, sup_info.origin_name))
        } else {
            Some(false)
        };
    }
    if matches!(sup_info.kind, ClassKind::Structure) {
        return Some(false);
    }

    if let Some(definition) = class_definition(runtime, sub) {
        if definition.precedence.iter().any(|class| {
            runtime
                .heap
                .class_info(class.get())
                .is_some_and(|info| info.serial == sup_info.serial)
        }) {
            return Some(true);
        }
        return Some(false);
    }

    let parents = classes::registered_superclasses(runtime, sub)?;
    let mut has_unknown_subtype = false;
    for parent in parents {
        match class_objects_subtypep(runtime, parent.get(), sup, depth + 1) {
            Some(true) => return Some(true),
            Some(false) => {}
            None => has_unknown_subtype = true,
        }
    }
    (!has_unknown_subtype).then_some(false)
}

pub(crate) fn find_named_class(
    runtime: &Runtime,
    name: Word,
    at: usize,
) -> Result<Option<Word>, Fault> {
    if !runtime.heap.is_symbol(name) {
        return Err(Fault::type_error(runtime, 
            name,
            "SYMBOL",
            EvalError::new(at, "expected a class name symbol"),
        ));
    }
    let id = runtime
        .heap
        .symbol_attributes(name)
        .expect("validated class name")
        .id;
    if let Some(association) = runtime
        .classes
        .borrow()
        .associations
        .get(&id)
        .map(|entry| entry.as_ref().map(|slot| slot.get()))
    {
        return Ok(association);
    }
    canonical_class(runtime, name, at)
}

/// The class a name denotes independently of any FIND-CLASS association.
pub(crate) fn canonical_class(
    runtime: &Runtime,
    name: Word,
    at: usize,
) -> Result<Option<Word>, Fault> {
    if !runtime.heap.is_symbol(name) {
        return Err(Fault::type_error(runtime, 
            name,
            "SYMBOL",
            EvalError::new(at, "expected a class name symbol"),
        ));
    }
    let system = system_class(runtime, name);
    let metaclass = if let Some(system) = system {
        system.metaclass()
    } else if classes::has_declaration(runtime, name) {
        ClassKind::Standard
    } else if runtime.structure_definition(name).is_some() {
        ClassKind::Structure
    } else {
        let id = runtime
            .heap
            .symbol_attributes(name)
            .expect("class name symbol")
            .id;
        return Ok(runtime.classes.borrow().objects.get(&id).and_then(|class| {
            let info = runtime.heap.class_info(class.get()).expect("class object");
            matches!(info.kind, ClassKind::ForwardReferenced).then(|| class.get())
        }));
    };
    let class = ensure_class_object(runtime, name, metaclass, at)?;
    if matches!(metaclass, ClassKind::Structure)
        && matches!(
            runtime.heap.class_info(class).unwrap().kind,
            ClassKind::ForwardReferenced
        )
    {
        runtime.heap.set_class_instance(class, Word::NIL);
        runtime
            .heap
            .resolve_forward_class(class, ClassKind::Structure);
    }
    if system.is_some_and(|s| {
        s == SystemClass::StandardObject
            || s == SystemClass::FuncallableStandardObject
            || s.is_a(SystemClass::Metaobject)
    }) {
        return classes::ensure_empty_system_definition(runtime, class, at).map(Some);
    }
    Ok(Some(class))
}

pub(crate) fn representation(
    runtime: &Runtime,
    precedence: &[Rc<RootSlot>],
    at: usize,
) -> Result<Representation, Fault> {
    let mut representation = Representation::Instance;
    let mut class_object = false;
    let mut slot_object = false;
    let mut callable = false;
    let mut specializer = false;
    for class in precedence {
        let name = runtime
            .heap
            .class_info(class.get())
            .expect("precedence class")
            .origin_name;
        let required = match system_class(runtime, name) {
            Some(SystemClass::DirectSlotDefinition) => Representation::DirectSlot,
            Some(SystemClass::EffectiveSlotDefinition) => Representation::EffectiveSlot,
            Some(SystemClass::ForwardReferencedClass) => Representation::ForwardReferencedClass,
            Some(SystemClass::StandardClass) => Representation::StandardClass,
            Some(SystemClass::FuncallableStandardClass) => Representation::FuncallableStandardClass,
            Some(SystemClass::GenericFunction) => Representation::GenericFunction,
            Some(SystemClass::Method) => Representation::Method,
            Some(SystemClass::Specializer) => {
                specializer = true;
                continue;
            }
            Some(SystemClass::Class) => {
                class_object = true;
                continue;
            }
            Some(SystemClass::SlotDefinition) => {
                slot_object = true;
                continue;
            }
            Some(SystemClass::FuncallableStandardObject) => {
                callable = true;
                continue;
            }
            _ => continue,
        };
        if representation != Representation::Instance && representation != required {
            return Err(incompatible_representation(at));
        }
        representation = required;
    }
    for (present, default, compatible) in [
        (
            class_object,
            Representation::StandardClass,
            matches!(
                representation,
                Representation::StandardClass
                    | Representation::FuncallableStandardClass
                    | Representation::ForwardReferencedClass
            ),
        ),
        (
            slot_object,
            Representation::DirectSlot,
            matches!(
                representation,
                Representation::DirectSlot | Representation::EffectiveSlot
            ),
        ),
        (
            callable,
            Representation::FuncallableInstance,
            representation == Representation::GenericFunction,
        ),
    ] {
        if present && !compatible {
            if representation != Representation::Instance {
                return Err(incompatible_representation(at));
            }
            representation = default;
        }
    }
    if specializer && !class_object {
        if representation != Representation::Instance {
            return Err(incompatible_representation(at));
        }
        representation = Representation::Specializer;
    }
    Ok(representation)
}

fn incompatible_representation(at: usize) -> Fault {
    Fault::plain(
        Class::ProgramError,
        EvalError::new(
            at,
            "superclasses require incompatible object representations",
        ),
    )
}

pub(crate) fn is_storage_superclass(runtime: &Runtime, class: Word) -> bool {
    let name = runtime
        .heap
        .class_info(class)
        .expect("superclass object")
        .origin_name;
    system_class(runtime, name).is_some_and(|s| {
        s == SystemClass::FuncallableStandardObject || s.is_a(SystemClass::Metaobject)
    })
}

pub(crate) fn allocates_class_metaobjects(runtime: &Runtime, class: Word) -> bool {
    class_definition(runtime, class).is_some_and(|d| {
        matches!(
            d.representation,
            Representation::StandardClass
                | Representation::FuncallableStandardClass
                | Representation::ForwardReferencedClass
        )
    })
}

pub(crate) fn allocate_forward_class(
    runtime: &Runtime,
    name: &RootSlot,
    at: usize,
) -> Result<Word, Fault> {
    let metaclass = runtime.root_slot(builtin_class(runtime, "FORWARD-REFERENCED-CLASS", at)?);
    super::instances::allocate_instance(runtime, &metaclass, name.get(), at)
}

pub(crate) fn attach_metaclass(
    runtime: &Runtime,
    class: &RootSlot,
    metaclass: &RootSlot,
    at: usize,
) -> Result<(), Fault> {
    let definition = class_definition(runtime, metaclass.get()).ok_or_else(|| {
        Fault::plain(
            Class::ProgramError,
            EvalError::new(at, "metaclass has undefined superclasses"),
        )
    })?;
    let instance = runtime
        .heap
        .instance(metaclass, &definition.instance_layout)
        .map_err(|err| EvalError::out_of_memory(at, err))?;
    runtime.heap.set_class_instance(class.get(), instance);
    Ok(())
}

fn next_class_serial(runtime: &Runtime) -> usize {
    let mut registry = runtime.classes.borrow_mut();
    let serial = registry.next_serial;
    registry.next_serial += 1;
    serial
}

pub(crate) fn ensure_class_object(
    runtime: &Runtime,
    name: Word,
    metaclass: ClassKind,
    at: usize,
) -> Result<Word, Fault> {
    let id = runtime
        .heap
        .symbol_attributes(name)
        .expect("class name symbol")
        .id;
    if let Some(found) = runtime.classes.borrow().objects.get(&id) {
        return Ok(found.get());
    }
    let object = allocate_class_object(runtime, name, metaclass, at)?;
    runtime
        .classes
        .borrow_mut()
        .objects
        .insert(id, runtime.root_slot(object));
    Ok(object)
}

/// Allocate an unpublished class object so registration can validate its graph first.
pub(crate) fn allocate_class_object(
    runtime: &Runtime,
    name: Word,
    kind: ClassKind,
    at: usize,
) -> Result<Word, Fault> {
    let name = runtime.root_slot(name);
    let serial = next_class_serial(runtime);
    Ok(runtime
        .heap
        .class(kind, serial, || name.get())
        .map_err(|error| EvalError::out_of_memory(at, error))?)
}

fn includes(runtime: &Runtime, object: Word, expected: SystemClass) -> bool {
    let actual = match direct_class(runtime, object) {
        DirectClass::System(class) => class,
        DirectClass::Structure(_) => SystemClass::StructureObject,
        DirectClass::Standard(class) => {
            return class_definition(runtime, class).is_some_and(|definition| {
                definition.precedence.iter().any(|class| {
                    let name = runtime
                        .heap
                        .class_info(class.get())
                        .expect("precedence class")
                        .origin_name;
                    system_class(runtime, name) == Some(expected)
                })
            });
        }
    };
    actual.is_a(expected)
}

pub(super) fn named_includes(runtime: &Runtime, object: Word, name: &str) -> Option<bool> {
    let class = SystemClass::named(name)?;
    class.standard_package()?;
    Some(includes(runtime, object, class))
}

pub(super) fn typep(runtime: &Runtime, object: Word, class: ClassFields) -> bool {
    if let Some(system) = system_class(runtime, class.origin_name) {
        includes(runtime, object, system)
    } else if let Some(actual) = runtime.heap.instance_class(object) {
        // A class object specifier denotes that class object, not every
        // class that happens to share its name.
        runtime
            .heap
            .class_info(actual)
            .is_some_and(|info| info.serial == class.serial)
            || class_definition(runtime, actual).is_some_and(|definition| {
                definition.precedence.iter().any(|candidate| {
                    runtime
                        .heap
                        .class_info(candidate.get())
                        .is_some_and(|info| info.serial == class.serial)
                })
            })
    } else {
        runtime.structure_is_a(object, class.origin_name)
    }
}

pub(super) fn named_user_includes(
    runtime: &Runtime,
    object: Word,
    class_name: Word,
) -> Option<bool> {
    if let Some(class) = system_class(runtime, class_name) {
        return Some(includes(runtime, object, class));
    }
    let id = runtime.heap.symbol_attributes(class_name)?.id;
    let class = {
        let registry = runtime.classes.borrow();
        match registry.associations.get(&id) {
            Some(association) => association.as_ref()?.get(),
            None => registry.objects.get(&id)?.get(),
        }
    };
    let info = runtime.heap.class_info(class)?;
    if matches!(info.kind, ClassKind::ForwardReferenced) {
        return None;
    }
    Some(typep(runtime, object, info))
}

/// Literal classes for which CLHS 3.2.4.4 requires MAKE-LOAD-FORM.
pub(crate) fn requires_load_form(runtime: &Runtime, object: Word) -> bool {
    match direct_class(runtime, object) {
        DirectClass::Standard(_) | DirectClass::Structure(_) => true,
        DirectClass::System(class) => {
            class.is_a(SystemClass::StandardObject)
                || class.is_a(SystemClass::StructureObject)
                || matches!(
                    class,
                    SystemClass::Condition(_)
                        | SystemClass::Treelist
                        | SystemClass::MutableTreelist
                )
        }
    }
}

/// Identity of the class precedence used for a standard dispatch cache entry.
/// A committed instance layout changes on redefinition. Built-in classes have
/// immutable precedence and their canonical names are disjoint from layouts.
pub(super) fn dispatch_class_identity(runtime: &Runtime, object: Word) -> Option<Word> {
    match direct_class(runtime, object) {
        DirectClass::Standard(class) => {
            Some(class_definition(runtime, class)?.instance_layout.get())
        }
        DirectClass::System(class) if matches!(class.metaclass(), ClassKind::BuiltIn) => {
            if let Some(package) = class.standard_package() {
                Some(runtime.packages.borrow().present(package, class.name())?.0)
            } else {
                class.simd_symbol(runtime)
            }
        }
        _ => None,
    }
}

pub(crate) fn class_of(runtime: &Runtime, object: Word, at: usize) -> Result<Word, Fault> {
    let name = match direct_class(runtime, object) {
        DirectClass::System(class) => class.symbol(runtime, at)?,
        DirectClass::Structure(name) => name,
        DirectClass::Standard(class) => return Ok(class),
    };
    Ok(canonical_class(runtime, name, at)?.expect("instances have a defined class"))
}

pub(super) fn has_direct_class(runtime: &Runtime, object: Word, expected: Word) -> bool {
    let info = runtime.heap.class_info(expected).expect("class object");
    match direct_class(runtime, object) {
        DirectClass::Standard(class) => class == expected,
        DirectClass::System(class) => system_class(runtime, info.origin_name) == Some(class),
        DirectClass::Structure(name) => {
            matches!(info.kind, ClassKind::Structure) && info.origin_name == name
        }
    }
}

pub(super) fn set_class_name(
    runtime: &Runtime,
    class: Word,
    name: Word,
    at: usize,
) -> Result<Word, Fault> {
    if runtime.heap.class_info(class).is_none() {
        return Err(Fault::type_error(runtime, 
            class,
            "CLASS",
            EvalError::new(at, "expected a class object"),
        ));
    }
    if !runtime.heap.is_symbol(name) {
        return Err(Fault::type_error(runtime, 
            name,
            "SYMBOL",
            EvalError::new(at, "expected a class name symbol"),
        ));
    }
    runtime.heap.set_class_name(class, name);
    Ok(name)
}

pub(super) fn find_class(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    if args.len() == 3 {
        crate::macros::validate_environment(runtime, frame.get(args.start + 2), at)?;
    }
    let name = frame.get(args.start);
    if let Some(class) = find_named_class(runtime, name, at)? {
        return Ok(class);
    }
    if args.len() > 1 && frame.get(args.start + 1) == Word::NIL {
        return Ok(Word::NIL);
    }
    let spelling = runtime
        .heap
        .symbol_name(name)
        .expect("validated class name");
    Err(Fault::plain(
        Class::SimpleError,
        EvalError::new(at, format!("no class named {spelling}")),
    ))
}

pub(crate) fn class_name(runtime: &Runtime, class: Word, at: usize) -> Result<Word, Fault> {
    runtime
        .heap
        .class_info(class)
        .map(|info| info.name)
        .ok_or_else(|| {
            Fault::type_error(runtime, 
                class,
                "CLASS",
                EvalError::new(at, "expected a class object"),
            )
        })
}
