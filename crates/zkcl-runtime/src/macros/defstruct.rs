use std::collections::{HashMap, HashSet};

use crate::heap::Word;
use crate::symbol::Package;
use crate::{EvalError, Root, Runtime};

use super::{
    COPY_STRUCTURE, Expansion, MAKE_STRUCTURE, PRINT_DEPTH, READ_STRUCTURE_SLOT,
    REGISTER_STRUCTURE, STRUCTURE_PREDICATE, call, define_at_toplevel, fresh, invalid,
    keyword_name_opt as keyword_name, list, native_call, proper_list, quote, symbol,
};

struct Slot<'rt> {
    name: Root<'rt>,
    initform: Root<'rt>,
    type_specifier: Root<'rt>,
    read_only: bool,
    reader: Root<'rt>,
    offset: usize,
    direct: bool,
}

enum Representation<'rt> {
    List,
    Vector { element_type: Root<'rt> },
}

struct TypedLayout<'rt> {
    representation: Representation<'rt>,
    prefix_names: Vec<(usize, Root<'rt>)>,
    name_offset: Option<usize>,
    total_len: usize,
}

impl<'rt> TypedLayout<'rt> {
    fn metadata(&self, runtime: &'rt Runtime) -> Expansion<'rt> {
        let mut names = Vec::with_capacity(self.prefix_names.len());
        for (offset, name) in &self.prefix_names {
            let offset = runtime
                .fixnum(*offset as isize)
                .expect("structure name offset");
            names.push(list(runtime, [&offset, name].into_iter())?);
        }
        let (kind, element_type) = match &self.representation {
            Representation::List => (symbol(runtime, "LIST")?, runtime.nil()),
            Representation::Vector { element_type } => {
                (symbol(runtime, "VECTOR")?, element_type.clone())
            }
        };
        let len = runtime
            .fixnum(self.total_len as isize)
            .expect("structure length");
        let names = list(runtime, names.iter())?;
        quote(
            runtime,
            &list(runtime, [&kind, &element_type, &len, &names].into_iter())?,
        )
    }
}

struct SlotSpec<'rt> {
    name: Root<'rt>,
    initform: Root<'rt>,
    type_specifier: Option<Root<'rt>>,
    read_only: Option<bool>,
}

impl<'rt> SlotSpec<'rt> {
    fn parse(runtime: &'rt Runtime, form: &Root<'rt>) -> Result<Self, EvalError> {
        let fields;
        let (name, rest) = if runtime.heap.is_symbol(form.slot.get()) {
            (form, &[][..])
        } else {
            fields = proper_list(runtime, form.clone(), "DEFSTRUCT slot")?;
            fields
                .split_first()
                .ok_or_else(|| bad("expects a slot name"))?
        };
        spelling(runtime, name)?;
        let (initform, options) = match rest.split_first() {
            Some((initform, options)) => (initform.clone(), options),
            None => (runtime.nil(), &[][..]),
        };
        let mut type_specifier = None;
        let mut read_only = None;
        let (options, remainder) = options.as_chunks::<2>();
        if !remainder.is_empty() {
            return Err(bad("expects slot options in keyword/value pairs"));
        }
        for [option, value] in options {
            match keyword_name(runtime, option).as_deref() {
                Some("TYPE") if type_specifier.is_none() => type_specifier = Some(value.clone()),
                Some("READ-ONLY") if read_only.is_none() => read_only = Some(!value.is_nil()),
                _ => return Err(bad("has an unknown or repeated slot option")),
            }
        }
        Ok(Self {
            name: name.clone(),
            initform,
            type_specifier,
            read_only,
        })
    }

    fn into_slot(self, runtime: &'rt Runtime, offset: usize) -> Slot<'rt> {
        Slot {
            name: self.name,
            initform: self.initform,
            type_specifier: self.type_specifier.unwrap_or(runtime.root(Word::T)),
            read_only: self.read_only.unwrap_or(false),
            reader: runtime.nil(),
            offset,
            direct: true,
        }
    }
}

struct Constructor<'rt> {
    name: Root<'rt>,
    parameters: Option<Root<'rt>>,
}

enum Printer<'rt> {
    Default,
    Object(Root<'rt>),
    Function(Root<'rt>),
}

struct DefstructOptions<'rt> {
    prefix: Option<String>,
    constructors: Vec<Constructor<'rt>>,
    copier: Root<'rt>,
    predicate: Option<Root<'rt>>,
    include: Option<(Root<'rt>, Vec<Root<'rt>>)>,
    representation: Option<Representation<'rt>>,
    variant_cases: Option<Root<'rt>>,
    printer: Option<Printer<'rt>>,
    named: bool,
    initial_offset: usize,
}

fn parse_representation<'rt>(
    runtime: &'rt Runtime,
    spec: &Root<'rt>,
) -> Result<Representation<'rt>, EvalError> {
    if let Some(name) = runtime.symbol_name(spec) {
        return match name.as_str() {
            "LIST" => Ok(Representation::List),
            "VECTOR" => Ok(Representation::Vector {
                element_type: runtime.root(Word::T),
            }),
            _ => Err(bad("expects :TYPE LIST or VECTOR")),
        };
    }
    let items = proper_list(runtime, spec.clone(), "DEFSTRUCT :TYPE")?;
    if let [head, element_type] = items.as_slice()
        && runtime.symbol_name(head).as_deref() == Some("VECTOR")
    {
        return Ok(Representation::Vector {
            element_type: element_type.clone(),
        });
    }
    Err(bad("unsupported :TYPE specifier"))
}

impl<'rt> DefstructOptions<'rt> {
    fn parse(runtime: &'rt Runtime, name: &str, forms: &[Root<'rt>]) -> Result<Self, EvalError> {
        let mut prefix = Some(format!("{name}-"));
        let mut constructors = Vec::new();
        let mut suppress_constructor = false;
        let mut copier = None;
        let mut predicate = None;
        let mut include = None;
        let mut representation = None;
        let mut variant_cases = None;
        let mut printer = None;
        let mut named = false;
        let mut initial_offset = None;
        let mut seen = HashSet::new();
        for form in forms {
            let fields;
            let (option, args) = if let Some(keyword) = keyword_name(runtime, form) {
                (keyword, &[][..])
            } else {
                fields = proper_list(runtime, form.clone(), "DEFSTRUCT option")?;
                let (key, args) = fields
                    .split_first()
                    .ok_or_else(|| bad("expects an option keyword"))?;
                (
                    keyword_name(runtime, key).ok_or_else(|| bad("expects an option keyword"))?,
                    args,
                )
            };
            if option != "CONSTRUCTOR" && !seen.insert(option.clone()) {
                return Err(bad("has a repeated option"));
            }
            match (option.as_str(), args) {
                ("%VARIANT", [cases]) => variant_cases = Some(cases.clone()),
                ("CONC-NAME", []) => prefix = None,
                ("CONC-NAME", [value]) if value.is_nil() => prefix = None,
                ("CONC-NAME", [value]) => {
                    prefix = Some(
                        runtime
                            .string_designator_text(value.slot.get())
                            .ok_or_else(|| bad("expects a string designator as the prefix"))?,
                    );
                }
                ("CONSTRUCTOR", []) => constructors.push(Constructor {
                    name: intern(runtime, &format!("MAKE-{name}"))?,
                    parameters: None,
                }),
                ("CONSTRUCTOR", [value]) if value.is_nil() => suppress_constructor = true,
                ("CONSTRUCTOR", [value] | [value, _]) => {
                    spelling(runtime, value)?;
                    if value.is_nil() {
                        return Err(bad("expects a name for a BOA constructor"));
                    }
                    constructors.push(Constructor {
                        name: value.clone(),
                        parameters: args.get(1).cloned(),
                    });
                }
                ("COPIER" | "PREDICATE", []) => {}
                ("COPIER" | "PREDICATE", [value]) => {
                    if !value.is_nil() {
                        spelling(runtime, value)?;
                    }
                    if option == "COPIER" {
                        copier = Some(value.clone());
                    } else {
                        predicate = Some(value.clone());
                    }
                }
                ("INCLUDE", [base, overrides @ ..]) => {
                    spelling(runtime, base)?;
                    include = Some((base.clone(), overrides.to_vec()));
                }
                ("PRINT-OBJECT" | "PRINT-FUNCTION", [] | [_])
                    if !runtime.heap.is_symbol(form.slot.get()) =>
                {
                    if printer.is_some() {
                        return Err(bad("allows only one printer option"));
                    }
                    printer = Some(match args.first() {
                        None => Printer::Default,
                        Some(function) if option == "PRINT-OBJECT" => {
                            Printer::Object(function.clone())
                        }
                        Some(function) => Printer::Function(function.clone()),
                    });
                }
                ("TYPE", [spec]) => representation = Some(parse_representation(runtime, spec)?),
                ("NAMED", []) => named = true,
                ("INITIAL-OFFSET", [offset]) => {
                    let count = offset
                        .as_fixnum()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| bad(":INITIAL-OFFSET expects a non-negative integer"))?;
                    initial_offset = Some(count);
                }
                _ => {
                    return Err(bad(&format!(
                        "does not support option :{option} in this form"
                    )));
                }
            }
        }
        if printer.is_some() && representation.is_some() {
            return Err(bad("does not allow a printer with :TYPE"));
        }
        if initial_offset.is_some() && representation.is_none() {
            return Err(bad(":INITIAL-OFFSET requires :TYPE"));
        }
        if suppress_constructor {
            if !constructors.is_empty() {
                return Err(bad("cannot combine :CONSTRUCTOR NIL with constructors"));
            }
        } else if constructors.is_empty() {
            constructors.push(Constructor {
                name: intern(runtime, &format!("MAKE-{name}"))?,
                parameters: None,
            });
        }
        Ok(Self {
            prefix,
            constructors,
            copier: copier.map_or_else(|| intern(runtime, &format!("COPY-{name}")), Ok)?,
            predicate,
            include,
            representation,
            variant_cases,
            printer,
            named,
            initial_offset: initial_offset.unwrap_or(0),
        })
    }
}

pub(super) fn expand<'rt>(runtime: &'rt Runtime, forms: &[Root<'rt>]) -> Expansion<'rt> {
    let Some((name_and_options, descriptions)) = forms.split_first() else {
        return Err(bad("expects a structure name"));
    };
    let header = if runtime.heap.is_symbol(name_and_options.slot.get()) {
        vec![name_and_options.clone()]
    } else {
        proper_list(
            runtime,
            name_and_options.clone(),
            "DEFSTRUCT name and options",
        )?
    };
    let Some((name, options)) = header.split_first() else {
        return Err(bad("expects a structure name"));
    };
    if runtime.heap.is_symbol(name.slot.get()) {
        crate::compile::declarations::reject_declaration(runtime, name.slot.get(), "DEFSTRUCT", 0)?;
    }
    if name.is_nil() {
        return Err(bad("expects a non-NIL structure name"));
    }
    let name_spelling = spelling(runtime, name)?;
    let options = DefstructOptions::parse(runtime, &name_spelling, options)?;
    let (doc, descriptions) = match descriptions.split_first() {
        Some((first, rest)) if runtime.heap.is_string(first.slot.get()) => (first.clone(), rest),
        _ => (runtime.nil(), descriptions),
    };
    let quoted_type = quote(runtime, name)?;
    let parent = options
        .include
        .as_ref()
        .map_or_else(|| runtime.nil(), |(parent, _)| parent.clone());
    let keyword_constructor = options
        .constructors
        .iter()
        .find(|constructor| constructor.parameters.is_none())
        .map_or_else(|| runtime.nil(), |constructor| constructor.name.clone());

    let unnamed = options.representation.is_some() && !options.named;
    let predicate = match options.predicate {
        Some(name) if unnamed && !name.is_nil() => {
            return deferred_error(runtime, ":PREDICATE requires :NAMED on a typed structure");
        }
        Some(name) => name,
        None if unnamed => runtime.nil(),
        None => intern(runtime, &format!("{name_spelling}-P"))?,
    };
    let (mut slots, ancestors, typed) = if let Some(repr) = options.representation {
        if options.named
            && let Representation::Vector { element_type } = &repr
        {
            // A named vector must be able to store its structure name.
            let storable =
                crate::vm::types::typep(runtime, name.slot.get(), element_type.slot.get(), 0)
                    .unwrap_or(false);
            if !storable {
                return deferred_error(
                    runtime,
                    "a named vector structure's name does not fit its ELEMENT-TYPE",
                );
            }
        }
        let (slots, ancestors, layout) = typed_slots(
            runtime,
            descriptions,
            options.include.as_ref(),
            repr,
            options.initial_offset,
            options.named.then_some(name),
        )?;
        (slots, ancestors, Some(layout))
    } else {
        // A sealed variant root has no fields. Its cases can be macroexpanded
        // before the root's registration runs (including a single top-level EVAL).
        let include = options
            .include
            .as_ref()
            .filter(|_| !options.variant_cases.as_ref().is_some_and(Root::is_nil));
        let (slots, ancestors) = slots(runtime, descriptions, include)?;
        (slots, ancestors, None)
    };
    let mut forms = Vec::new();
    let mut slot_fields = Vec::new();
    for slot in &mut slots {
        let reader = match &options.prefix {
            Some(prefix) if typed.is_none() && options.variant_cases.is_some() => {
                let home = runtime
                    .heap
                    .symbol_home(name.slot.get())
                    .ok_or_else(|| bad("uninterned variant constructor"))?;
                let spelling = format!("{prefix}{}", spelling(runtime, &slot.name)?);
                let (word, _) = runtime.intern_package(home, &spelling, 0)?;
                runtime.root(word)
            }
            Some(prefix) => intern(
                runtime,
                &format!("{prefix}{}", spelling(runtime, &slot.name)?),
            )?,
            None => slot.name.clone(),
        };
        let reader_id = runtime
            .heap
            .symbol_attributes(reader.slot.get())
            .expect("slot reader symbol")
            .id;
        if !ancestors.contains(&reader_id) {
            let instance = fresh(runtime, "STRUCTURE")?;
            let index = runtime
                .fixnum(
                    slot.offset
                        .try_into()
                        .map_err(|_| bad("has too many slots"))?,
                )
                .ok_or_else(|| bad("has too many slots"))?;
            let access = match typed.as_ref().map(|layout| &layout.representation) {
                Some(Representation::List) => {
                    call(runtime, "NTH", [&index, &instance].into_iter())?
                }
                Some(Representation::Vector { .. }) => {
                    call(runtime, "AREF", [&instance, &index].into_iter())?
                }
                None => native_call(
                    runtime,
                    READ_STRUCTURE_SLOT,
                    [&instance, &quoted_type, &index],
                )?,
            };
            forms.push(defun(
                runtime,
                &reader,
                std::slice::from_ref(&instance),
                access.clone(),
            )?);
            if typed.is_some() && !slot.read_only {
                let value = fresh(runtime, "VALUE")?;
                let setter = call(runtime, "SETF", [&reader].into_iter())?;
                let assign = call(runtime, "SETF", [&access, &value].into_iter())?;
                forms.push(defun(runtime, &setter, &[value, instance], assign)?);
            }
            slot.reader = reader;
        }
        let read_only = runtime.root(if slot.read_only { Word::T } else { Word::NIL });
        let layout = if typed.is_some() {
            runtime
                .fixnum(slot.offset as isize)
                .expect("structure slot offset")
        } else {
            runtime.root(if slot.direct { Word::T } else { Word::NIL })
        };
        let fields = list(
            runtime,
            [
                &slot.name,
                &slot.initform,
                &slot.type_specifier,
                &slot.reader,
                &read_only,
                &layout,
            ]
            .into_iter(),
        )?;
        slot_fields.push(fields);
    }
    let representation = typed
        .as_ref()
        .map(|layout| layout.metadata(runtime))
        .transpose()?;
    let metadata = quote(runtime, &list(runtime, slot_fields.iter())?)?;
    let parent = quote(runtime, &parent)?;
    let keyword_constructor = quote(runtime, &keyword_constructor)?;
    let mut operands = vec![
        quoted_type.clone(),
        parent,
        metadata,
        doc,
        keyword_constructor,
    ];
    if let Some(representation) = representation {
        operands.push(representation);
    } else if let Some(cases) = options.variant_cases {
        operands.push(runtime.nil());
        operands.push(quote(runtime, &cases)?);
    }
    let register = native_call(runtime, REGISTER_STRUCTURE, operands.iter())?;
    forms.insert(0, register);

    for constructor in options.constructors {
        let definition = match &typed {
            Some(layout) => expand_typed_constructor(runtime, constructor, &slots, layout)?,
            None => expand_constructor(runtime, &quoted_type, constructor, &slots)?,
        };
        forms.push(definition);
    }
    if let Some(layout) = typed {
        if !options.copier.is_nil() {
            let obj = fresh(runtime, "STRUCTURE")?;
            let copier_op = match &layout.representation {
                Representation::List => "COPY-LIST",
                Representation::Vector { .. } => "COPY-SEQ",
            };
            let body = call(runtime, copier_op, [&obj].into_iter())?;
            forms.push(defun(runtime, &options.copier, &[obj], body)?);
        }
        if let Some(name_idx) = layout.name_offset
            && !predicate.is_nil()
        {
            let obj = fresh(runtime, "STRUCTURE")?;
            let body = typed_predicate_body(
                runtime,
                &obj,
                name_idx,
                &quoted_type,
                &layout.representation,
            )?;
            forms.push(defun(runtime, &predicate, &[obj], body)?);
        }
    } else {
        for (name, operation) in [
            (options.copier, COPY_STRUCTURE),
            (predicate, STRUCTURE_PREDICATE),
        ] {
            if !name.is_nil() {
                let instance = fresh(runtime, "STRUCTURE")?;
                let expression = native_call(runtime, operation, [&quoted_type, &instance])?;
                forms.push(defun(runtime, &name, &[instance], expression)?);
            }
        }
        if let Some(printer) = options.printer {
            forms.push(printer.method(runtime, name)?);
        }
    }
    forms.push(quoted_type);
    define_at_toplevel(runtime, call(runtime, "PROGN", forms.iter())?)
}

impl<'rt> Printer<'rt> {
    fn method(self, runtime: &'rt Runtime, structure: &Root<'rt>) -> Expansion<'rt> {
        let object = fresh(runtime, "OBJECT")?;
        let stream = fresh(runtime, "STREAM")?;
        let specialized = list(runtime, [&object, structure].into_iter())?;
        let parameters = list(runtime, [&specialized, &stream].into_iter())?;
        let body = match self {
            Self::Default => {
                let method = runtime.root(
                    crate::vm::Primitive::named("%DEFAULT-PRINT-OBJECT")
                        .expect("default printer method"),
                );
                let nil = runtime.nil();
                call(
                    runtime,
                    "FUNCALL",
                    [&method, &nil, &nil, &object, &stream].into_iter(),
                )?
            }
            Self::Object(name) => {
                let function = call(runtime, "FUNCTION", [&name].into_iter())?;
                call(
                    runtime,
                    "FUNCALL",
                    [&function, &object, &stream].into_iter(),
                )?
            }
            Self::Function(name) => {
                let function = call(runtime, "FUNCTION", [&name].into_iter())?;
                let depth = native_call(runtime, PRINT_DEPTH, [])?;
                call(
                    runtime,
                    "FUNCALL",
                    [&function, &object, &stream, &depth].into_iter(),
                )?
            }
        };
        call(
            runtime,
            "DEFMETHOD",
            [&symbol(runtime, "PRINT-OBJECT")?, &parameters, &body].into_iter(),
        )
    }
}

fn slots<'rt>(
    runtime: &'rt Runtime,
    descriptions: &[Root<'rt>],
    include: Option<&(Root<'rt>, Vec<Root<'rt>>)>,
) -> Result<(Vec<Slot<'rt>>, HashSet<usize>), EvalError> {
    let mut slots = Vec::new();
    let mut ancestors = HashSet::new();
    if let Some((parent, overrides)) = include {
        let parent = runtime
            .structure_definition(parent.slot.get())
            .ok_or_else(|| bad("includes an undefined structure"))?;
        slots.extend(parent.slots.iter().enumerate().map(|(offset, slot)| Slot {
            name: runtime.root(slot.name.get()),
            initform: runtime.root(slot.initform.get()),
            type_specifier: runtime.root(slot.type_specifier.get()),
            read_only: slot.read_only,
            reader: runtime.nil(),
            offset,
            direct: false,
        }));
        let mut ancestor = Some(parent);
        while let Some(definition) = ancestor {
            ancestors.extend(definition.slots.iter().filter_map(|slot| {
                let reader = slot.reader.get();
                (reader != Word::NIL).then(|| {
                    runtime
                        .heap
                        .symbol_attributes(reader)
                        .expect("registered reader")
                        .id
                })
            }));
            ancestor = definition
                .parent
                .as_ref()
                .and_then(|parent| runtime.structure_definition(parent.get()));
        }
        apply_slot_overrides(runtime, &mut slots, overrides)?;
    }
    let first = slots.len();
    append_slots(runtime, &mut slots, descriptions, first)?;
    Ok((slots, ancestors))
}

fn typed_slots<'rt>(
    runtime: &'rt Runtime,
    descriptions: &[Root<'rt>],
    include: Option<&(Root<'rt>, Vec<Root<'rt>>)>,
    representation: Representation<'rt>,
    initial_offset: usize,
    name: Option<&Root<'rt>>,
) -> Result<(Vec<Slot<'rt>>, HashSet<usize>, TypedLayout<'rt>), EvalError> {
    let mut slots = Vec::new();
    let mut ancestors = HashSet::new();
    let mut prefix_names = Vec::new();
    let mut base_len = 0usize;
    if let Some((parent_name, overrides)) = include {
        let parent = runtime
            .typed_structure_definition(parent_name.slot.get())
            .ok_or_else(|| bad("includes an undefined or untyped structure"))?;
        let expected_list = matches!(representation, Representation::List);
        if parent.is_list != expected_list {
            return Err(bad("included structure has an incompatible :TYPE"));
        }
        base_len = parent.total_len;
        for (idx, tag) in &parent.prefix_names {
            prefix_names.push((*idx, runtime.root(tag.get())));
        }
        for slot in &parent.slots {
            let reader = slot.reader.get();
            if reader != Word::NIL
                && let Some(data) = runtime.heap.symbol_attributes(reader)
            {
                ancestors.insert(data.id);
            }
            slots.push(Slot {
                name: runtime.root(slot.name.get()),
                initform: runtime.root(slot.initform.get()),
                type_specifier: runtime.root(slot.type_specifier.get()),
                read_only: slot.read_only,
                reader: runtime.nil(),
                offset: slot.offset,
                direct: false,
            });
        }
        apply_slot_overrides(runtime, &mut slots, overrides)?;
    }
    let prefix_len = base_len + initial_offset;
    let name_offset = name.map(|name| {
        prefix_names.push((prefix_len, name.clone()));
        prefix_len
    });
    let first = prefix_len + usize::from(name.is_some());
    append_slots(runtime, &mut slots, descriptions, first)?;
    let layout = TypedLayout {
        representation,
        prefix_names,
        name_offset,
        total_len: first + descriptions.len(),
    };
    Ok((slots, ancestors, layout))
}

fn apply_slot_overrides<'rt>(
    runtime: &'rt Runtime,
    slots: &mut [Slot<'rt>],
    overrides: &[Root<'rt>],
) -> Result<(), EvalError> {
    let mut overridden = HashSet::new();
    for form in overrides {
        let replacement = SlotSpec::parse(runtime, form)?;
        let name = spelling(runtime, &replacement.name)?;
        let slot = slots
            .iter_mut()
            .find(|slot| runtime.symbol_name(&slot.name).as_deref() == Some(name.as_str()))
            .ok_or_else(|| bad(&format!("overrides unknown included slot {name}")))?;
        if !overridden.insert(name) {
            return Err(bad("overrides an included slot twice"));
        }
        if slot.read_only && replacement.read_only == Some(false) {
            return Err(bad("cannot make an included read-only slot writable"));
        }
        slot.initform = replacement.initform;
        slot.direct = true;
        if let Some(type_specifier) = replacement.type_specifier {
            slot.type_specifier = type_specifier;
        }
        if let Some(read_only) = replacement.read_only {
            slot.read_only = read_only;
        }
    }
    Ok(())
}

fn append_slots<'rt>(
    runtime: &'rt Runtime,
    slots: &mut Vec<Slot<'rt>>,
    descriptions: &[Root<'rt>],
    first_offset: usize,
) -> Result<(), EvalError> {
    let mut names = slots
        .iter()
        .map(|slot| spelling(runtime, &slot.name))
        .collect::<Result<HashSet<_>, _>>()?;
    for (index, form) in descriptions.iter().enumerate() {
        let spec = SlotSpec::parse(runtime, form)?;
        let name = spelling(runtime, &spec.name)?;
        if !names.insert(name.clone()) {
            return Err(bad(&format!("has duplicate slot {name}")));
        }
        slots.push(spec.into_slot(runtime, first_offset + index));
    }
    Ok(())
}

fn constructor_params_and_values<'rt>(
    runtime: &'rt Runtime,
    constructor: Constructor<'rt>,
    slots: &[Slot<'rt>],
) -> Result<(Root<'rt>, Vec<Root<'rt>>, Vec<Root<'rt>>), EvalError> {
    let (parameters, values) = match constructor.parameters {
        Some(boa) => boa_constructor(runtime, boa, slots)?,
        None => {
            let mut parameters = vec![symbol(runtime, "&KEY")?];
            let mut values = Vec::new();
            for slot in slots {
                let variable = fresh(runtime, "SLOT")?;
                let keyword = runtime.root(
                    runtime
                        .intern_package(Package::KEYWORD, &spelling(runtime, &slot.name)?, 0)?
                        .0,
                );
                let key = list(runtime, [&keyword, &variable].into_iter())?;
                parameters.push(list(runtime, [&key, &slot.initform].into_iter())?);
                values.push(variable);
            }
            (parameters, values)
        }
    };
    Ok((constructor.name, parameters, values))
}

fn expand_typed_constructor<'rt>(
    runtime: &'rt Runtime,
    constructor: Constructor<'rt>,
    slots: &[Slot<'rt>],
    layout: &TypedLayout<'rt>,
) -> Expansion<'rt> {
    let (name, parameters, values) = constructor_params_and_values(runtime, constructor, slots)?;
    let mut elements = vec![runtime.nil(); layout.total_len];
    for (idx, tag) in &layout.prefix_names {
        elements[*idx] = quote(runtime, tag)?;
    }
    for (slot, value) in slots.iter().zip(values) {
        elements[slot.offset] = value;
    }
    let body = match &layout.representation {
        Representation::List => call(runtime, "LIST", elements.iter())?,
        Representation::Vector { element_type } => {
            if runtime.symbol_name(element_type).as_deref() == Some("T") {
                call(runtime, "VECTOR", elements.iter())?
            } else {
                let len = runtime.fixnum(layout.total_len as isize).unwrap();
                let key_elt = runtime.root(
                    runtime
                        .intern_package(Package::KEYWORD, "ELEMENT-TYPE", 0)?
                        .0,
                );
                let key_init = runtime.root(
                    runtime
                        .intern_package(Package::KEYWORD, "INITIAL-CONTENTS", 0)?
                        .0,
                );
                let initial = call(runtime, "LIST", elements.iter())?;
                call(
                    runtime,
                    "MAKE-ARRAY",
                    [
                        &len,
                        &key_elt,
                        &quote(runtime, element_type)?,
                        &key_init,
                        &initial,
                    ]
                    .into_iter(),
                )?
            }
        }
    };
    defun(runtime, &name, &parameters, body)
}

fn typed_predicate_body<'rt>(
    runtime: &'rt Runtime,
    obj: &Root<'rt>,
    name_idx: usize,
    quoted_type: &Root<'rt>,
    repr: &Representation<'rt>,
) -> Expansion<'rt> {
    match repr {
        Representation::List => {
            if name_idx == 0 {
                let consp = call(runtime, "CONSP", std::slice::from_ref(obj).iter())?;
                let car = call(runtime, "CAR", std::slice::from_ref(obj).iter())?;
                let eq = call(runtime, "EQ", [&car, quoted_type].into_iter())?;
                call(runtime, "AND", [&consp, &eq].into_iter())
            } else {
                let cur = fresh(runtime, "CUR")?;
                let idx_word = runtime.fixnum(name_idx as isize).unwrap();
                let dotimes_var = fresh(runtime, "I")?;
                let dotimes_spec = list(runtime, [&dotimes_var, &idx_word].into_iter())?;
                let consp_cur = call(runtime, "CONSP", std::slice::from_ref(&cur).iter())?;
                let cdr_cur = call(runtime, "CDR", std::slice::from_ref(&cur).iter())?;
                let next = call(
                    runtime,
                    "IF",
                    [&consp_cur, &cdr_cur, &runtime.nil()].into_iter(),
                )?;
                let step = call(runtime, "SETQ", [&cur, &next].into_iter())?;
                let walk = call(runtime, "DOTIMES", [&dotimes_spec, &step].into_iter())?;
                let car_cur = call(runtime, "CAR", std::slice::from_ref(&cur).iter())?;
                let eq = call(runtime, "EQ", [&car_cur, quoted_type].into_iter())?;
                let check = call(runtime, "AND", [&consp_cur, &eq].into_iter())?;
                let pair = list(runtime, [&cur, obj].into_iter())?;
                let bindings = list(runtime, std::iter::once(&pair))?;
                call(runtime, "LET", [&bindings, &walk, &check].into_iter())
            }
        }
        Representation::Vector { .. } => {
            let vectorp = call(runtime, "VECTORP", std::slice::from_ref(obj).iter())?;
            let length = call(runtime, "LENGTH", std::slice::from_ref(obj).iter())?;
            let idx_word = runtime.fixnum(name_idx as isize).unwrap();
            let in_bounds = call(runtime, ">", [&length, &idx_word].into_iter())?;
            let elt = call(runtime, "AREF", [obj, &idx_word].into_iter())?;
            let eq = call(runtime, "EQ", [&elt, quoted_type].into_iter())?;
            call(runtime, "AND", [&vectorp, &in_bounds, &eq].into_iter())
        }
    }
}

fn expand_constructor<'rt>(
    runtime: &'rt Runtime,
    quoted_type: &Root<'rt>,
    constructor: Constructor<'rt>,
    slots: &[Slot<'rt>],
) -> Expansion<'rt> {
    let (name, parameters, values) = constructor_params_and_values(runtime, constructor, slots)?;
    let mut arguments = vec![quoted_type.clone()];
    arguments.extend(values);
    let body = native_call(runtime, MAKE_STRUCTURE, arguments.iter())?;
    defun(runtime, &name, &parameters, body)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParameterKind {
    Required,
    Optional,
    Rest,
    Key,
    Aux,
}

fn boa_constructor<'rt>(
    runtime: &'rt Runtime,
    parameters: Root<'rt>,
    slots: &[Slot<'rt>],
) -> Result<(Vec<Root<'rt>>, Vec<Root<'rt>>), EvalError> {
    let fields = proper_list(runtime, parameters, "DEFSTRUCT BOA lambda list")?;
    let mut parameters = Vec::with_capacity(fields.len());
    let mut variables = HashMap::new();
    let defaults = slots
        .iter()
        .map(|slot| Ok((spelling(runtime, &slot.name)?, &slot.initform)))
        .collect::<Result<HashMap<_, _>, EvalError>>()?;
    let mut kind = ParameterKind::Required;
    for field in fields {
        if runtime.heap.symbol_home(field.slot.get()) == Some(Package::COMMON_LISP) {
            let next = match runtime.symbol_name(&field).as_deref() {
                Some("&OPTIONAL") => Some(ParameterKind::Optional),
                Some("&REST") => Some(ParameterKind::Rest),
                Some("&KEY") => Some(ParameterKind::Key),
                Some("&AUX") => Some(ParameterKind::Aux),
                Some("&ALLOW-OTHER-KEYS") => Some(kind),
                _ => None,
            };
            if let Some(next) = next {
                kind = next;
                parameters.push(field);
                continue;
            }
        }
        if matches!(kind, ParameterKind::Required | ParameterKind::Rest) {
            variables.insert(spelling(runtime, &field)?, field.clone());
            parameters.push(field);
            continue;
        }
        let fields = if runtime.heap.is_symbol(field.slot.get()) {
            vec![field.clone()]
        } else {
            proper_list(runtime, field.clone(), "DEFSTRUCT BOA parameter")?
        };
        let Some(head) = fields.first() else {
            return Err(bad("expects a BOA parameter name"));
        };
        let variable = if kind == ParameterKind::Key && !runtime.heap.is_symbol(head.slot.get()) {
            let fields = proper_list(runtime, head.clone(), "DEFSTRUCT BOA keyword")?;
            let [_, variable] = fields.as_slice() else {
                return Err(bad("expects a keyword and BOA parameter name"));
            };
            variable.clone()
        } else {
            head.clone()
        };
        let name = spelling(runtime, &variable)?;
        let default = defaults.get(&name).copied();
        variables.insert(name, variable);
        if let Some(supplied) = fields.get(2) {
            variables.insert(spelling(runtime, supplied)?, supplied.clone());
        }
        if fields.len() == 1
            && matches!(kind, ParameterKind::Optional | ParameterKind::Key)
            && let Some(default) = default
        {
            parameters.push(list(runtime, [head, default].into_iter())?);
        } else {
            parameters.push(field);
        }
    }
    let values = slots
        .iter()
        .map(|slot| {
            let name = spelling(runtime, &slot.name)?;
            Ok(variables.get(&name).unwrap_or(&slot.initform).clone())
        })
        .collect::<Result<_, EvalError>>()?;
    Ok((parameters, values))
}

fn defun<'rt>(
    runtime: &'rt Runtime,
    name: &Root<'rt>,
    parameters: &[Root<'rt>],
    body: Root<'rt>,
) -> Expansion<'rt> {
    let parameters = list(runtime, parameters.iter())?;
    call(runtime, "DEFUN", [name, &parameters, &body].into_iter())
}

fn intern<'rt>(runtime: &'rt Runtime, text: &str) -> Expansion<'rt> {
    let (word, _) = runtime.intern_package(runtime.current_package_at(0)?, text, 0)?;
    Ok(runtime.root(word))
}

fn spelling(runtime: &Runtime, symbol: &Root<'_>) -> Result<String, EvalError> {
    runtime
        .symbol_name(symbol)
        .ok_or_else(|| bad("expects a symbol"))
}

fn bad(message: &str) -> EvalError {
    invalid("DEFSTRUCT", message)
}

/// Invalid DEFSTRUCT definitions expand to a form that signals SIMPLE-ERROR
/// when evaluated. A native expander's own error is reported as
/// PROGRAM-ERROR, but these option conflicts are ordinary user errors.
fn deferred_error<'rt>(runtime: &'rt Runtime, message: &str) -> Expansion<'rt> {
    let text = runtime
        .heap
        .string(&format!("DEFSTRUCT {message}"))
        .map_err(|error| EvalError::out_of_memory(0, error))?;
    let text = runtime.root(text);
    call(runtime, "ERROR", [&text].into_iter())
}
