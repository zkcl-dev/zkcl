use std::ops::Range;
use std::rc::Rc;

use super::{FunctionName, classes, equality::eql, is_function, methods, proper_list};
use crate::compile::{CallSignature, call_signature};
use crate::condition::{Class, Fault};
use crate::heap::{GenericCallSignature, GenericFields, MethodFields, RootSlot, RootSlots, Word};
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime};

pub(crate) fn define_generic<'rt>(
    runtime: &'rt Runtime,
    arguments: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [name, lambda_list, precedence, combination, documentation] = arguments else {
        return Err(problem(at, "invalid generic function definition"));
    };
    let signature = call_signature(runtime, lambda_list, at)?;
    let prepared_signature =
        prepare_generic_call_signature(runtime, &signature, precedence.slot.get(), at)?;
    runtime
        .method_combinations
        .get()
        .expect("initialized runtime")
        .parse(runtime, combination.slot.get(), at)?;
    let fields = || GenericFields {
        name: name.slot.get(),
        lambda_list: lambda_list.slot.get(),
        call_signature: prepared_signature(),
        precedence: precedence.slot.get(),
        combination: combination.slot.get(),
        documentation: documentation.slot.get(),
        methods: Word::NIL,
    };
    let function_name = FunctionName::parse(runtime, name.slot.get(), at)?;
    let generic = if let Some(existing) = function_name.get(runtime) {
        let Some(info) = runtime.heap.generic_info(existing) else {
            return Err(problem(
                at,
                "the name is bound to an ordinary function or macro",
            ));
        };
        let generic = runtime.root(existing);
        let definition = runtime
            .heap
            .generic_inline_definition(existing, None)
            .unwrap();
        let mut retained = Vec::new();
        let mut removed = Vec::new();
        for method in proper_list(runtime, info.methods, at)? {
            let method_info = runtime.heap.method_info(method).expect("registered method");
            let method = runtime.root(method);
            if definition != Word::NIL && method_info.inline_definition == definition {
                removed.push(method);
            } else {
                retained.push(method);
            }
        }
        for method in &retained {
            let info = runtime
                .heap
                .method_info(method.slot.get())
                .expect("registered method");
            let method_lambda_list = runtime.root(info.lambda_list);
            let method_signature = call_signature(runtime, &method_lambda_list, at)?;
            congruent(&signature, &method_signature, at)?;
        }
        if !removed.is_empty() {
            let methods = list(runtime, &retained, at)?;
            runtime
                .heap
                .set_generic_methods(generic.slot.get(), methods.slot.get());
            for method in removed {
                methods::update_specializer_direct_methods(runtime, &method.slot, false, at)?;
                runtime.heap.set_method_owner(method.slot.get(), Word::NIL);
            }
        }
        runtime
            .heap
            .generic_inline_definition(generic.slot.get(), Some(Word::NIL));
        let info = fields();
        runtime.heap.set_generic_definition(
            generic.slot.get(),
            info.lambda_list,
            info.call_signature,
            info.precedence,
            info.combination,
            info.documentation,
        );
        generic
    } else {
        new_generic(runtime, fields, at)?
    };
    Ok(vec![generic])
}

pub(crate) fn define_method<'rt>(
    runtime: &'rt Runtime,
    arguments: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [
        name,
        qualifiers,
        raw_specializers,
        lambda_list,
        function,
        documentation,
        inline,
    ] = arguments
    else {
        return Err(problem(at, "invalid method definition"));
    };
    if !is_function(runtime, function.slot.get()) {
        return Err(problem(at, "a method requires a function body"));
    }
    let specializers = resolve_specializers(runtime, raw_specializers.slot.get(), at)?;
    let signature = call_signature(runtime, lambda_list, at)?;
    if specializers.len() != signature.required.len() {
        return Err(problem(
            at,
            "the method needs one specializer per required parameter",
        ));
    }
    let specializers = list(runtime, &specializers, at)?;
    let function_name = FunctionName::parse(runtime, name.slot.get(), at)?;
    let generic = if let Some(existing) = function_name.get(runtime) {
        if runtime.heap.generic_info(existing).is_none() {
            return Err(problem(
                at,
                "the name is bound to an ordinary function or macro",
            ));
        }
        runtime.root(existing)
    } else {
        let lambda_list = implicit_lambda_list(runtime, &signature, at)?;
        let prepared_signature =
            prepare_signature_with_default_precedence(runtime, &lambda_list, at)?;
        let combination = list(runtime, &[symbol(runtime, "STANDARD", at)?], at)?;
        new_generic(
            runtime,
            || GenericFields {
                name: name.slot.get(),
                lambda_list: lambda_list.slot.get(),
                call_signature: prepared_signature(),
                precedence: Word::NIL,
                combination: combination.slot.get(),
                documentation: Word::NIL,
                methods: Word::NIL,
            },
            at,
        )?
    };
    let definition = if inline.is_nil() {
        runtime.nil()
    } else {
        runtime.root(inline_definition(runtime, &generic, at)?)
    };
    let method = runtime.root(
        runtime
            .heap
            .method(|| MethodFields {
                qualifiers: qualifiers.slot.get(),
                specializers: specializers.slot.get(),
                lambda_list: lambda_list.slot.get(),
                function: function.slot.get(),
                documentation: documentation.slot.get(),
                owner: Word::NIL,
                accessor_slot: Word::NIL,
                inline_definition: definition.slot.get(),
            })
            .map_err(|error| EvalError::out_of_memory(at, error))?,
    );
    methods::add(runtime, generic.slot.get(), method.slot.get(), at)?;
    Ok(vec![method])
}

/// Methods from one DEFGENERIC share a token even if they are detached and
/// subsequently attached elsewhere. A later definition starts a new group.
pub(super) fn inline_definition(
    runtime: &Runtime,
    gf: &Root<'_>,
    at: usize,
) -> Result<Word, Fault> {
    let token = runtime
        .heap
        .generic_inline_definition(gf.slot.get(), None)
        .ok_or_else(|| problem(at, "expected a generic function"))?;
    if token != Word::NIL {
        return Ok(token);
    }
    let token = runtime
        .heap
        .cons(|| (Word::NIL, Word::NIL))
        .map_err(|e| EvalError::out_of_memory(at, e))?;
    runtime
        .heap
        .generic_inline_definition(gf.slot.get(), Some(token));
    Ok(token)
}

fn new_generic<'rt>(
    runtime: &'rt Runtime,
    fields: impl FnOnce() -> GenericFields,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let generic = runtime
        .heap
        .generic_function(fields)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let name = runtime
        .heap
        .generic_info(generic)
        .expect("allocated generic")
        .name;
    FunctionName::parse(runtime, name, at)?.install(runtime, generic, at)?;
    Ok(runtime.root(generic))
}

/// A bootstrap generic and the lambda list shared by its standard methods.
pub(super) struct BuiltinGeneric<'rt> {
    function: Root<'rt>,
    signature: Root<'rt>,
}

impl<'rt> BuiltinGeneric<'rt> {
    pub(super) fn standard(
        runtime: &'rt Runtime,
        name: &str,
        signature: &str,
    ) -> Result<Self, EvalError> {
        let name = runtime.root(runtime.intern_common_lisp(name, 0)?);
        Self::with_signature_source(&name, signature)
    }

    pub(super) fn setter(
        runtime: &'rt Runtime,
        source: &str,
        signature: &str,
    ) -> Result<Self, EvalError> {
        let name = crate::form_value::to_value(runtime, &crate::read::read(runtime, source)?)?;
        Self::with_signature_source(&name, signature)
    }

    pub(super) fn with_signature_source(name: &Root<'rt>, source: &str) -> Result<Self, EvalError> {
        let runtime = name.owner;
        let signature = crate::form_value::to_value(runtime, &crate::read::read(runtime, source)?)?;
        Self::install(name, signature)
    }

    pub(super) fn install(name: &Root<'rt>, signature: Root<'rt>) -> Result<Self, EvalError> {
        let runtime = name.owner;
        let prepared = prepare_signature_with_default_precedence(runtime, &signature, 0)
            .map_err(|fault| fault.host)?;
        let standard = runtime.root(runtime.intern_common_lisp("STANDARD", 0)?);
        let combination = list(runtime, &[standard], 0).map_err(|fault| fault.host)?;
        let generic = runtime
            .heap
            .generic_function(|| GenericFields {
                name: name.slot.get(),
                lambda_list: signature.slot.get(),
                call_signature: prepared(),
                precedence: Word::NIL,
                combination: combination.slot.get(),
                documentation: Word::NIL,
                methods: Word::NIL,
            })
            .map_err(|error| EvalError::out_of_memory(0, error))?;
        match FunctionName::parse(runtime, name.slot.get(), 0).map_err(|fault| fault.host)? {
            FunctionName::Symbol(symbol) => runtime.heap.set_symbol_cell(symbol, true, generic),
            FunctionName::Setf(accessor) => runtime.heap.set_symbol_setter(accessor, generic),
        };
        Ok(Self {
            function: runtime.root(generic),
            signature,
        })
    }

    pub(super) fn add(&self, specializers: &[&str], entry: Word) -> Result<(), EvalError> {
        let runtime = self.function.owner;
        let entry = runtime.root(entry);
        let specializers = specializers
            .iter()
            .map(|name| classes::builtin_class(runtime, name, 0).map(|class| runtime.root(class)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|fault| fault.host)?;
        let specializers = list(runtime, &specializers, 0).map_err(|fault| fault.host)?;
        let method = runtime.root(
            runtime
                .heap
                .method(|| MethodFields {
                    qualifiers: Word::NIL,
                    specializers: specializers.slot.get(),
                    lambda_list: self.signature.slot.get(),
                    function: entry.slot.get(),
                    documentation: Word::NIL,
                    owner: self.function.slot.get(),
                    accessor_slot: Word::NIL,
                    inline_definition: Word::NIL,
                })
                .map_err(|error| EvalError::out_of_memory(0, error))?,
        );
        let updated = runtime.root(
            runtime
                .heap
                .cons(|| {
                    let info = runtime
                        .heap
                        .generic_info(self.function.slot.get())
                        .expect("bootstrap generic function");
                    (method.slot.get(), info.methods)
                })
                .map_err(|error| EvalError::out_of_memory(0, error))?,
        );
        methods::update_specializer_direct_methods(runtime, &method.slot, true, 0)
            .map_err(|fault| fault.host)?;
        runtime
            .heap
            .set_generic_methods(self.function.slot.get(), updated.slot.get());
        Ok(())
    }
}

pub(super) fn check_method_congruence(
    runtime: &Runtime,
    generic: Word,
    method_lambda_list: Word,
    at: usize,
) -> Result<(), Fault> {
    let info = runtime
        .heap
        .generic_info(generic)
        .expect("generic function");
    let generic = runtime.root(info.lambda_list);
    let method = runtime.root(method_lambda_list);
    let generic = call_signature(runtime, &generic, at)?;
    let method = call_signature(runtime, &method, at)?;
    congruent(&generic, &method, at)
}

pub(super) fn congruent(
    generic: &CallSignature,
    method: &CallSignature,
    at: usize,
) -> Result<(), Fault> {
    let variadic = |s: &CallSignature| s.rest.is_some() || s.keywords.is_some();
    if generic.required.len() != method.required.len()
        || generic.optional.len() != method.optional.len()
        || variadic(generic) != variadic(method)
    {
        return Err(problem(
            at,
            "method and generic function lambda lists are not congruent",
        ));
    }
    if let (Some(required), Some(accepted)) = (&generic.keywords, &method.keywords)
        && !method.allow_other_keys
        && required.iter().any(|key| {
            !accepted
                .iter()
                .any(|candidate| candidate.key() == key.key())
        })
    {
        return Err(problem(
            at,
            "the method does not accept every generic function keyword",
        ));
    }
    Ok(())
}

pub(super) fn fixed_dispatch_arity(signature: GenericCallSignature, count: usize) -> bool {
    matches!(count, 1 | 2)
        && signature.required == count
        && signature.positional == count
        && !signature.accepts_extra
}

// A single class identity determines the complete ordered set exactly when
// all non-T specializers across the generic are classes in the same position.
// Writers conventionally dispatch on their second argument.
pub(super) fn single_dispatch_argument(runtime: &Runtime, mut methods: Word) -> Option<usize> {
    let mut position = None;
    while let Some((method, next)) = runtime.heap.cons_fields(methods) {
        let mut specs = runtime
            .heap
            .method_info(method)
            .expect("registered method")
            .specializers;
        let mut index = 0;
        while let Some((spec, tail)) = runtime.heap.cons_fields(specs) {
            let class = runtime.heap.class_info(spec)?;
            if class.origin_name != Word::T {
                if position.is_some_and(|previous| previous != index) {
                    return None;
                }
                position = Some(index);
            }
            index += 1;
            specs = tail;
        }
        methods = next;
    }
    Some(position.unwrap_or(0))
}

pub(super) fn applicable_methods(
    runtime: &Runtime,
    generic: Word,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<Vec<Word>, Fault> {
    let generic = runtime.root(generic);
    let signature = generic_call_signature(runtime, generic.slot.get());
    validate_arity(signature, args.len(), at)?;
    rank_value_methods(
        runtime,
        &generic,
        argument_precedence_positions(runtime, signature),
        |position| classes::class_of(runtime, frame.get(args.start + position), at),
        |position| Some(frame.get(args.start + position)),
        at,
    )
}

pub(super) fn validate_arity(
    signature: GenericCallSignature,
    count: usize,
    at: usize,
) -> Result<(), Fault> {
    if count < signature.required || count > signature.positional && !signature.accepts_extra {
        return Err(problem(
            at,
            "wrong number of arguments for the generic function",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(super) enum InitializationArgument<'a> {
    Value(&'a RootSlot),
    NewInstance(&'a RootSlot),
}

pub(super) fn applicable_initialization_methods(
    runtime: &Runtime,
    generic: &Root<'_>,
    arguments: &[InitializationArgument<'_>],
    at: usize,
) -> Result<Vec<Word>, Fault> {
    let signature = generic_call_signature(runtime, generic.slot.get());
    if arguments.len() != signature.required {
        return Err(problem(
            at,
            "wrong number of initialization method arguments",
        ));
    }
    rank_value_methods(
        runtime,
        generic,
        argument_precedence_positions(runtime, signature),
        |position| match arguments[position] {
            InitializationArgument::Value(value) => classes::class_of(runtime, value.get(), at),
            InitializationArgument::NewInstance(class) => Ok(class.get()),
        },
        |position| match arguments[position] {
            InitializationArgument::Value(value) => Some(value.get()),
            // Before allocation no existing object can be EQL to the new instance.
            InitializationArgument::NewInstance(_) => None,
        },
        at,
    )
}

fn generic_call_signature(runtime: &Runtime, generic: Word) -> GenericCallSignature {
    runtime
        .heap
        .generic_call_signature(generic)
        .expect("standard generic function")
}

fn argument_precedence_positions(
    runtime: &Runtime,
    signature: GenericCallSignature,
) -> impl ExactSizeIterator<Item = usize> + Clone {
    let mut tail = signature.precedence_positions;
    let order = if tail == Word::NIL {
        None
    } else {
        let mut positions = Vec::with_capacity(signature.required);
        while let Some((position, next)) = runtime.heap.cons_fields(tail) {
            positions.push(position.as_fixnum().expect("validated argument position") as usize);
            tail = next;
        }
        Some(Rc::<[_]>::from(positions))
    };
    (0..signature.required).map(move |index| order.as_ref().map_or(index, |order| order[index]))
}

fn rank_value_methods(
    runtime: &Runtime,
    generic: &Root<'_>,
    positions: impl ExactSizeIterator<Item = usize> + Clone,
    class_at: impl Fn(usize) -> Result<Word, Fault>,
    value_at: impl Fn(usize) -> Option<Word>,
    at: usize,
) -> Result<Vec<Word>, Fault> {
    let precedence = (0..positions.len())
        .map(|position| classes::class_precedence(runtime, class_at(position)?, at))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rank_methods(
        runtime,
        generic.slot.get(),
        positions,
        &precedence,
        |position, value| eql(&runtime.heap, value, value_at(position)?).then_some(EqlMatch::Exact),
        at,
    )?
    .expect("known or fresh argument values determine applicability"))
}

pub(super) fn applicable_methods_using_classes(
    runtime: &Runtime,
    generic: &Root<'_>,
    classes: &[RootSlot],
    at: usize,
) -> Result<Option<Vec<Word>>, Fault> {
    let signature = generic_call_signature(runtime, generic.slot.get());
    if classes.len() != signature.required {
        return Err(problem(at, "provide one class for each required argument"));
    }
    let positions = argument_precedence_positions(runtime, signature);
    let precedence = classes
        .iter()
        .map(|class| super::classes::class_precedence(runtime, class.get(), at))
        .collect::<Result<Vec<_>, _>>()?;
    rank_methods(
        runtime,
        generic.slot.get(),
        positions,
        &precedence,
        |position, value| {
            super::classes::has_direct_class(runtime, value, classes[position].get())
                .then_some(EqlMatch::Possible)
        },
        at,
    )
}

enum EqlMatch {
    Exact,
    Possible,
}

fn rank_methods(
    runtime: &Runtime,
    generic: Word,
    positions: impl Iterator<Item = usize> + Clone,
    precedence: &[Rc<[Rc<RootSlot>]>],
    eql_match: impl Fn(usize, Word) -> Option<EqlMatch>,
    at: usize,
) -> Result<Option<Vec<Word>>, Fault> {
    let info = runtime
        .heap
        .generic_info(generic)
        .expect("rooted generic function");
    let mut matching = Vec::new();
    for method in proper_list(runtime, info.methods, at)? {
        let specializers = runtime
            .heap
            .method_info(method)
            .expect("registered method")
            .specializers;
        let specializers = proper_list(runtime, specializers, at)?;
        if specializers.iter().any(|&s| {
            runtime.heap.class_info(s).is_none() && runtime.heap.eql_specializer_object(s).is_none()
        }) {
            return Err(problem(
                at,
                "custom specializers require a method on the applicability protocol",
            ));
        }
        let mut uncertain = false;
        let rank = |position: usize| {
            let specializer = specializers[position];
            if let Some(value) = runtime.heap.eql_specializer_object(specializer) {
                uncertain |= matches!(eql_match(position, value)?, EqlMatch::Possible);
                Some(0)
            } else {
                precedence[position]
                    .iter()
                    .position(|class| class.get() == specializer)
                    .map(|rank| rank + 1)
            }
        };
        if let Some(key) = positions.clone().map(rank).collect::<Option<Vec<_>>>() {
            if uncertain {
                return Ok(None);
            }
            matching.push((key, method));
        }
    }
    matching.sort_by(|(a, _), (b, _)| a.cmp(b));
    Ok(Some(
        matching.into_iter().map(|(_, method)| method).collect(),
    ))
}

pub(super) fn validate_arguments(
    runtime: &Runtime,
    generic: Word,
    applicable: &[RootSlot],
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<(), Fault> {
    let signature = generic_call_signature(runtime, generic);
    let positional = signature.positional;
    if args.len() <= positional {
        return Ok(());
    }
    let keyworded = signature.keywords.is_some();
    let mut allow = signature.allow_other_keys;
    let mut uses_keywords = keyworded;
    let mut keys = proper_list(runtime, signature.keywords.unwrap_or(Word::NIL), at)?
        .into_iter()
        .map(|word| runtime.root_slot(word))
        .collect::<Vec<_>>();
    let mut names = Vec::new();
    for method in applicable {
        let method = runtime
            .heap
            .method_info(method.get())
            .expect("rooted method");
        let method = call_signature(runtime, &runtime.root(method.lambda_list), at)?;
        uses_keywords |= method.keywords.is_some();
        allow |= method.allow_other_keys;
        names.extend(method.keywords.into_iter().flatten());
    }
    if !uses_keywords {
        return Ok(());
    }
    let supplied = args.start + positional..args.end;
    if !supplied.len().is_multiple_of(2) {
        return Err(problem(
            at,
            "odd number of generic function keyword arguments",
        ));
    }
    if !keyworded {
        // A generic function with only &REST hands keywords to its methods,
        // which validate the ones they actually declare.
        return Ok(());
    }
    if supplied
        .clone()
        .step_by(2)
        .any(|index| !runtime.heap.is_symbol(frame.get(index)))
    {
        return Err(problem(
            at,
            "generic function keyword argument names must be symbols",
        ));
    }
    for name in &names {
        keys.push(runtime.root_slot(runtime.intern(name, at)?));
    }
    let override_key = runtime.intern(&Name::keyword("ALLOW-OTHER-KEYS"), at)?;
    let keys: Vec<_> = keys.iter().map(RootSlot::get).collect();
    super::keyword_arguments(
        runtime,
        &keys,
        supplied,
        |i| frame.get(i),
        allow,
        override_key,
    )
    .map_err(|error| error.program_error(at))?;
    Ok(())
}

fn prepare_signature_with_default_precedence<'rt>(
    runtime: &'rt Runtime,
    lambda_list: &Root<'_>,
    at: usize,
) -> Result<impl Fn() -> GenericCallSignature + use<'rt>, Fault> {
    prepare_generic_call_signature(
        runtime,
        &call_signature(runtime, lambda_list, at)?,
        Word::NIL,
        at,
    )
}

pub(super) fn prepare_generic_call_signature<'rt>(
    runtime: &'rt Runtime,
    signature: &CallSignature,
    precedence: Word,
    at: usize,
) -> Result<impl Fn() -> GenericCallSignature + use<'rt>, Fault> {
    let precedence_positions = if precedence == Word::NIL {
        runtime.nil()
    } else {
        let positions = resolve_argument_precedence_order(runtime, precedence, signature, at)?
            .into_iter()
            .map(|position| Word::fixnum(position as isize).expect("argument position"))
            .collect();
        runtime.root(methods::list_from_words(runtime, positions, at)?)
    };
    let keywords = if let Some(names) = &signature.keywords {
        let names = names
            .iter()
            .map(|name| runtime.intern(name, at).map(|word| runtime.root(word)))
            .collect::<Result<Vec<_>, _>>()?;
        Some(list(runtime, &names, at)?)
    } else {
        None
    };
    let required = signature.required.len();
    let positional = required + signature.optional.len();
    let accepts_extra = signature.rest.is_some() || signature.keywords.is_some();
    let allow_other_keys = signature.allow_other_keys;
    Ok(move || GenericCallSignature {
        required,
        positional,
        accepts_extra,
        allow_other_keys,
        keywords: keywords.as_ref().map(|root| root.slot.get()),
        precedence_positions: precedence_positions.slot.get(),
    })
}

fn resolve_argument_precedence_order(
    runtime: &Runtime,
    order: Word,
    signature: &CallSignature,
    at: usize,
) -> Result<Vec<usize>, Fault> {
    let required = &signature.required;
    let order = proper_list(runtime, order, at)?;
    let mut positions = Vec::with_capacity(required.len());
    for name in order {
        let some = required
            .iter()
            .position(|var| var.rooted_word() == Some(name));
        let Some(position) = some.filter(|index| !positions.contains(index)) else {
            return Err(problem(at, "invalid argument precedence order"));
        };
        positions.push(position);
    }
    if positions.len() != required.len() {
        return Err(problem(
            at,
            "argument precedence order omits a required parameter",
        ));
    }
    Ok(positions)
}

pub(super) fn resolve_specializers(
    runtime: &Runtime,
    raw: Word,
    at: usize,
) -> Result<Vec<Root<'_>>, Fault> {
    let specializers: Vec<_> = proper_list(runtime, raw, at)?
        .into_iter()
        .map(|word| runtime.root(word))
        .collect();
    let eql_symbol = runtime.intern_common_lisp("EQL", at)?;
    let eql_symbol = runtime.root_slot(eql_symbol);
    specializers
        .into_iter()
        .map(|specializer| {
            let word = specializer.slot.get();
            if runtime.heap.class_info(word).is_some() {
                return Ok(specializer);
            }
            if let Some((head, value)) = runtime.heap.cons_fields(word)
                && head == eql_symbol.get()
            {
                super::mop::intern_eql_specializer(runtime, value, at)
                    .map(|word| runtime.root(word))
            } else if let Some(class) = classes::find_named_class(runtime, word, at)? {
                Ok(runtime.root(class))
            } else {
                let name = runtime
                    .heap
                    .symbol_name(specializer.slot.get())
                    .expect("class name symbol");
                Err(problem(at, format!("no class named {name}")))
            }
        })
        .collect()
}

pub(super) fn implicit_lambda_list<'rt>(
    runtime: &'rt Runtime,
    signature: &CallSignature,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let mut params = Vec::new();
    let mut append = |names: &[Name]| -> Result<(), Fault> {
        for name in names {
            params.push(runtime.root(runtime.intern(name, at)?));
        }
        Ok(())
    };
    append(&signature.required)?;
    if !signature.optional.is_empty() {
        let marker = Name::common_lisp("&OPTIONAL");
        append(&[marker])?;
        append(&signature.optional)?;
    }
    if let Some(rest) = &signature.rest {
        append(&[Name::common_lisp("&REST"), rest.clone()])?;
    }
    if signature.keywords.is_some() {
        append(&[Name::common_lisp("&KEY")])?;
    }
    list(runtime, &params, at)
}

pub(super) fn copy_lambda_list_and_precedence<'rt>(
    runtime: &'rt Runtime,
    lambda_list: &Root<'_>,
    precedence: &Root<'_>,
    at: usize,
) -> Result<(Root<'rt>, Root<'rt>), Fault> {
    let lambda_list = runtime.root(super::list::copy_tree(runtime, lambda_list.slot.get(), at)?);
    let precedence = runtime.root(super::list::copy_tree(runtime, precedence.slot.get(), at)?);
    Ok((lambda_list, precedence))
}

fn symbol<'rt>(runtime: &'rt Runtime, spelling: &str, at: usize) -> Result<Root<'rt>, Fault> {
    Ok(runtime.root(runtime.intern_common_lisp(spelling, at)?))
}

pub(super) fn list<'rt>(
    runtime: &'rt Runtime,
    elements: &[Root<'rt>],
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let mut list = runtime.nil();
    for element in elements.iter().rev() {
        list = runtime
            .cons(element, &list)
            .map_err(|error| EvalError::out_of_memory(at, error))?;
    }
    Ok(list)
}

fn problem(at: usize, message: impl Into<String>) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}
