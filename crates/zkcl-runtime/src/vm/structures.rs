use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use super::proper_list;
use crate::condition::{Class, Fault};
use crate::heap::{RootSlot, Word};
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime};

mod image;

#[derive(Default)]
pub(crate) struct Registry {
    definitions: HashMap<usize, Rc<StructureDefinition>>,
    typed: HashMap<usize, Rc<TypedStructureDefinition>>,
    readers: HashMap<usize, (usize, usize)>,
}

pub(crate) struct TypedStructureDefinition {
    pub(crate) is_list: bool,
    pub(crate) total_len: usize,
    pub(crate) prefix_names: Box<[(usize, RootSlot)]>,
    pub(crate) slots: Box<[TypedSlotDefinition]>,
}

pub(crate) struct TypedSlotDefinition {
    pub(crate) name: RootSlot,
    pub(crate) initform: RootSlot,
    pub(crate) type_specifier: RootSlot,
    pub(crate) reader: RootSlot,
    pub(crate) read_only: bool,
    pub(crate) offset: usize,
}

#[derive(PartialEq)]
pub(crate) enum VariantRole {
    Family(Vec<usize>),
    Constructor,
}

pub(crate) struct StructureDefinition {
    pub(crate) variant: Option<VariantRole>,
    pub(crate) name: RootSlot,
    pub(crate) parent: Option<RootSlot>,
    pub(crate) slots: Box<[SlotDefinition]>,
    pub(crate) keyword_constructor: RootSlot,
    pub(crate) documentation: RootSlot,
}

pub(crate) struct SlotDefinition {
    pub(crate) name: RootSlot,
    pub(crate) initform: RootSlot,
    pub(crate) type_specifier: RootSlot,
    pub(crate) reader: RootSlot,
    pub(crate) read_only: bool,
    pub(super) direct: bool,
    pub(super) direct_metaobject: OnceCell<RootSlot>,
    pub(super) effective_metaobject: OnceCell<RootSlot>,
}

impl Runtime {
    /// With no :INCLUDE option the direct superclass is STRUCTURE-OBJECT.
    pub(super) fn structure_subclass_names(&self, included: Option<Word>) -> Vec<Rc<RootSlot>> {
        self.structures
            .borrow()
            .definitions
            .values()
            .filter(|definition| definition.parent.as_ref().map(RootSlot::get) == included)
            .map(|definition| Rc::new(self.root_slot(definition.name.get())))
            .collect()
    }

    pub(crate) fn structure_definition(&self, name: Word) -> Option<Rc<StructureDefinition>> {
        let id = self.heap.symbol_attributes(name)?.id;
        self.structures.borrow().definitions.get(&id).cloned()
    }

    pub(crate) fn typed_structure_definition(
        &self,
        name: Word,
    ) -> Option<Rc<TypedStructureDefinition>> {
        let id = self.heap.symbol_attributes(name)?.id;
        self.structures.borrow().typed.get(&id).cloned()
    }

    /// Resolve a compiled pattern projection by the reader's symbol identity.
    /// Layout offsets can differ in a FASL consumer; no producer offset escapes.
    pub(crate) fn variant_field(&self, value: Word, reader: Word) -> Option<Word> {
        let (layout, _) = self.heap.structure_layout(value)?;
        let reader_id = self.heap.symbol_attributes(reader)?.id;
        let layout_id = self.heap.symbol_attributes(layout)?.id;
        let registry = self.structures.borrow();
        let &(owner, index) = registry.readers.get(&reader_id)?;
        if owner != layout_id {
            return None;
        }
        self.heap.structure_slot(value, index)
    }

    pub(crate) fn structure_place(&self, reader: Word) -> Option<(Word, usize)> {
        let id = self.heap.symbol_attributes(reader)?.id;
        let registry = self.structures.borrow();
        let &(owner, index) = registry.readers.get(&id)?;
        let definition = &registry.definitions[&owner];
        (!definition.slots[index].read_only).then(|| (definition.name.get(), index))
    }

    pub(crate) fn structure_subtype(&self, mut name: Word, expected: Word) -> bool {
        let registry = self.structures.borrow();
        while name != expected {
            let Some(id) = self.heap.symbol_attributes(name).map(|data| data.id) else {
                return false;
            };
            let Some(parent) = registry
                .definitions
                .get(&id)
                .and_then(|definition| definition.parent.as_ref())
            else {
                return false;
            };
            name = parent.get();
        }
        true
    }

    pub(crate) fn structure_is_a(&self, value: Word, expected: Word) -> bool {
        self.heap
            .structure_layout(value)
            .is_some_and(|(name, _)| self.structure_subtype(name, expected))
    }
}

pub(crate) fn register<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let (args, variant) = if let [fields @ .., marker, cases] = args
        && fields.len() == 5
        && marker.is_nil()
    {
        let cases = proper_list(runtime, cases.slot.get(), at)?;
        let mut ids = cases
            .into_iter()
            .map(|s| {
                runtime
                    .heap
                    .symbol_attributes(s)
                    .map(|d| d.id)
                    .ok_or_else(|| program(at, "expected variant constructor name"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        (
            fields,
            Some(if ids.is_empty() {
                VariantRole::Constructor
            } else {
                ids.sort_unstable();
                VariantRole::Family(ids)
            }),
        )
    } else {
        (args, None)
    };
    let (name, parent, slots, documentation, constructor, typed_repr) = match args {
        [name, parent, slots, documentation, constructor] => {
            (name, parent, slots, documentation, constructor, None)
        }
        [name, parent, slots, documentation, constructor, typed_repr] => (
            name,
            parent,
            slots,
            documentation,
            constructor,
            Some(typed_repr),
        ),
        _ => return Err(program(at, "expected a structure definition")),
    };
    let name_word = name.slot.get();
    let id = runtime
        .heap
        .symbol_attributes(name_word)
        .ok_or_else(|| program(at, "expected a structure name symbol"))?
        .id;
    super::check_definable_name(runtime, name_word, at)?;
    crate::classes::check_name_unclaimed(runtime, id, at)?;
    let parent_word = parent.slot.get();
    let previous = runtime.structure_definition(name_word);
    if let Some(def) = previous.as_ref() {
        let same_kind = match (&def.variant, &variant) {
            (None, None) => true,
            (Some(VariantRole::Family(_)), Some(VariantRole::Family(_))) => true,
            (Some(VariantRole::Constructor), Some(VariantRole::Constructor)) => {
                def.parent.as_ref().map(RootSlot::get).unwrap_or(Word::NIL) == parent_word
            }
            _ => false,
        };
        if !same_kind {
            return Err(program(at, "cannot change a sealed variant definition"));
        }
    }
    let parent_definition = runtime.structure_definition(parent_word);
    match (
        parent_definition.as_ref().and_then(|d| d.variant.as_ref()),
        &variant,
    ) {
        (Some(VariantRole::Family(cases)), Some(VariantRole::Constructor))
            if cases.binary_search(&id).is_ok() => {}
        (Some(_), _) | (_, Some(VariantRole::Constructor)) => {
            return Err(program(
                at,
                "constructor is outside its declared sealed variant family",
            ));
        }
        _ => {}
    }
    if let Some(typed_repr) = typed_repr {
        let repr = proper_list(runtime, typed_repr.slot.get(), at)
            .map_err(|_| program(at, "expected typed structure layout metadata"))?;
        let [kind_word, _element_type, total_len_word, names_word] = repr.as_slice() else {
            return Err(program(at, "expected typed structure layout metadata"));
        };
        let is_list = runtime.find_named_symbol(&Name::common_lisp("LIST")) == Some(*kind_word);
        let total_len = slot_index(runtime, *total_len_word, at)?;
        let prefix_name_forms = proper_list(runtime, *names_word, at)
            .map_err(|_| program(at, "expected typed structure prefix names"))?;
        let mut prefix_names = Vec::with_capacity(prefix_name_forms.len());
        for pair in prefix_name_forms {
            let pair = proper_list(runtime, pair, at)
                .map_err(|_| program(at, "expected typed structure name entry"))?;
            let [idx, tag] = pair.as_slice() else {
                return Err(program(at, "expected typed structure name entry"));
            };
            prefix_names.push((slot_index(runtime, *idx, at)?, runtime.root_slot(*tag)));
        }
        let slot_forms = proper_list(runtime, slots.slot.get(), at)
            .map_err(|_| program(at, "expected a list of structure slots"))?;
        let mut typed_slots = Vec::with_capacity(slot_forms.len());
        let mut names = HashSet::new();
        for slot in slot_forms {
            let fields = proper_list(runtime, slot, at)
                .map_err(|_| program(at, "expected typed structure slot metadata"))?;
            let [
                s_name,
                initform,
                type_specifier,
                reader,
                read_only,
                offset_word,
            ] = fields[..]
            else {
                return Err(program(at, "expected typed structure slot metadata"));
            };
            let spelling = runtime
                .heap
                .symbol_name(s_name)
                .ok_or_else(|| program(at, "expected a slot name symbol"))?;
            if !names.insert(spelling) {
                return Err(program(at, "duplicate structure slot"));
            }
            if reader != Word::NIL {
                super::check_definable_name(runtime, reader, at)?;
            }
            typed_slots.push(TypedSlotDefinition {
                name: runtime.root_slot(s_name),
                initform: runtime.root_slot(initform),
                type_specifier: runtime.root_slot(type_specifier),
                reader: runtime.root_slot(reader),
                read_only: read_only != Word::NIL,
                offset: slot_index(runtime, offset_word, at)?,
            });
        }
        let definition = Rc::new(TypedStructureDefinition {
            is_list,
            total_len,
            prefix_names: prefix_names.into_boxed_slice(),
            slots: typed_slots.into_boxed_slice(),
        });
        let mut registry = runtime.structures.borrow_mut();
        registry.definitions.remove(&id);
        registry.readers.retain(|_, (owner, _)| *owner != id);
        registry.typed.insert(id, definition);
        return Ok(vec![name.clone()]);
    }

    let inherited = if parent_word == Word::NIL {
        None
    } else {
        let parent = runtime
            .structure_definition(parent_word)
            .ok_or_else(|| program(at, "unknown included structure"))?;
        if runtime.structure_subtype(parent_word, name_word) {
            return Err(program(at, "circular structure inclusion"));
        }
        Some(parent)
    };

    let slot_forms = proper_list(runtime, slots.slot.get(), at)
        .map_err(|_| program(at, "expected a list of structure slots"))?;
    let mut slots = Vec::with_capacity(slot_forms.len());
    let mut names = HashSet::new();
    let mut readers = Vec::new();
    for (index, slot) in slot_forms.into_iter().enumerate() {
        let fields = proper_list(runtime, slot, at)
            .map_err(|_| program(at, "expected structure slot metadata"))?;
        let [name, initform, type_specifier, reader, read_only, direct] = fields[..] else {
            return Err(program(at, "expected structure slot metadata"));
        };
        let spelling = runtime
            .heap
            .symbol_name(name)
            .ok_or_else(|| program(at, "expected a slot name symbol"))?;
        if !names.insert(spelling) {
            return Err(program(at, "duplicate structure slot"));
        }
        if reader != Word::NIL {
            let reader_id = runtime
                .heap
                .symbol_attributes(reader)
                .ok_or_else(|| program(at, "expected a reader name symbol"))?
                .id;
            super::check_definable_name(runtime, reader, at)?;
            readers.push((reader_id, (id, index)));
        }
        slots.push(SlotDefinition {
            name: runtime.root_slot(name),
            initform: runtime.root_slot(initform),
            type_specifier: runtime.root_slot(type_specifier),
            reader: runtime.root_slot(reader),
            read_only: read_only != Word::NIL,
            direct: direct != Word::NIL,
            direct_metaobject: OnceCell::new(),
            effective_metaobject: OnceCell::new(),
        });
    }
    if let Some(parent) = inherited
        && (slots.len() < parent.slots.len()
            || slots.iter().zip(&parent.slots).any(|(slot, inherited)| {
                slot.name.get() != inherited.name.get() || inherited.read_only && !slot.read_only
            }))
    {
        return Err(program(at, "incompatible included structure slots"));
    }
    let definition = Rc::new(StructureDefinition {
        variant,
        name: runtime.root_slot(name_word),
        parent: (parent_word != Word::NIL).then(|| runtime.root_slot(parent_word)),
        slots: slots.into_boxed_slice(),
        keyword_constructor: runtime.root_slot(constructor.slot.get()),
        documentation: runtime.root_slot(documentation.slot.get()),
    });
    let mut registry = runtime.structures.borrow_mut();
    registry.readers.retain(|_, (owner, _)| *owner != id);
    registry.readers.extend(readers);
    registry.typed.remove(&id);
    registry.definitions.insert(id, definition);
    Ok(vec![name.clone()])
}

pub(crate) fn make<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let Some((name, slots)) = args.split_first() else {
        return Err(program(at, "expected a structure name"));
    };
    let definition = runtime
        .structure_definition(name.slot.get())
        .ok_or_else(|| program(at, "unknown structure type"))?;
    if matches!(definition.variant, Some(VariantRole::Family(_))) {
        return Err(program(at, "a variant family requires a constructor"));
    }
    if slots.len() != definition.slots.len() {
        return Err(program(at, "wrong number of structure slot values"));
    }
    let instance = runtime
        .heap
        .structure(
            slots.len(),
            || definition.name.get(),
            |index| slots[index].slot.get(),
        )
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(vec![runtime.root(instance)])
}

pub(crate) fn print_depth<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    if !args.is_empty() {
        return Err(program(at, "print depth takes no arguments"));
    }
    Ok(vec![
        runtime
            .fixnum(super::format::pprint::logical_block_depth(runtime) as isize)
            .expect("printer nesting fits in a fixnum"),
    ])
}

pub(crate) fn predicate<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [name, instance] = args else {
        return Err(program(at, "expected a structure name and object"));
    };
    let matches = runtime.structure_is_a(instance.slot.get(), name.slot.get());
    Ok(vec![runtime.root(if matches {
        Word::T
    } else {
        Word::NIL
    })])
}

pub(crate) fn copy<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [name, instance] = args else {
        return Err(program(at, "expected a structure name and object"));
    };
    expect_type(runtime, instance.slot.get(), name.slot.get(), at)?;
    let copied = copy_instance(runtime, &instance.slot, at)?;
    Ok(vec![runtime.root(copied)])
}

pub(super) fn copy_structure(runtime: &Runtime, value: Word, at: usize) -> Result<Word, Fault> {
    if runtime.heap.structure_layout(value).is_none() {
        return Err(Fault::type_error(runtime, 
            value,
            "STRUCTURE-OBJECT",
            EvalError::new(at, "expected a structure instance"),
        ));
    }
    copy_instance(runtime, &runtime.root_slot(value), at)
}

fn copy_instance(runtime: &Runtime, instance: &RootSlot, at: usize) -> Result<Word, Fault> {
    let (_, count) = runtime.heap.structure_layout(instance.get()).unwrap();
    Ok(runtime
        .heap
        .structure(
            count,
            || runtime.heap.structure_layout(instance.get()).unwrap().0,
            |index| runtime.heap.structure_slot(instance.get(), index).unwrap(),
        )
        .map_err(|error| EvalError::out_of_memory(at, error))?)
}

pub(crate) fn read_slot<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [instance, name, index] = args else {
        return Err(program(at, "expected a structure, name, and slot index"));
    };
    let instance = instance.slot.get();
    expect_type(runtime, instance, name.slot.get(), at)?;
    let index = slot_index(runtime, index.slot.get(), at)?;
    let value = runtime
        .heap
        .structure_slot(instance, index)
        .ok_or_else(|| program(at, "structure slot index is out of bounds"))?;
    Ok(vec![runtime.root(value)])
}

pub(crate) fn write_slot<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [instance, name, index, value] = args else {
        return Err(program(
            at,
            "expected a structure, name, slot index, and value",
        ));
    };
    let instance = instance.slot.get();
    expect_type(runtime, instance, name.slot.get(), at)?;
    let index = slot_index(runtime, index.slot.get(), at)?;
    let (actual, _) = runtime.heap.structure_layout(instance).unwrap();
    let definition = runtime
        .structure_definition(actual)
        .ok_or_else(|| program(at, "unknown structure type"))?;
    let slot = definition
        .slots
        .get(index)
        .ok_or_else(|| program(at, "structure slot index is out of bounds"))?;
    if slot.read_only {
        return Err(program(at, "cannot change a read-only structure slot"));
    }
    runtime
        .heap
        .set_structure_slot(instance, index, value.slot.get())
        .ok_or_else(|| program(at, "structure slot index is out of bounds"))?;
    Ok(vec![value.clone()])
}

fn expect_type(runtime: &Runtime, value: Word, expected: Word, at: usize) -> Result<(), Fault> {
    if runtime.structure_is_a(value, expected) {
        Ok(())
    } else {
        let name = runtime.heap.symbol_name(expected).unwrap_or_default();
        Err(Fault::type_error_specifier(runtime, 
            value,
            expected,
            EvalError::new(at, format!("expected a structure of type {name}")),
        ))
    }
}

fn slot_index(runtime: &Runtime, index: Word, at: usize) -> Result<usize, Fault> {
    index
        .as_fixnum()
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| {
            Fault::non_negative_integer(
                runtime,
                index,
                EvalError::new(at, "expected a non-negative slot index"),
            )
        })
}

fn program(at: usize, message: &'static str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}
