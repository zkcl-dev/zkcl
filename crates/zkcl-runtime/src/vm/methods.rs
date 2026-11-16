use std::sync::Arc;

use crate::condition::{Class, Fault};
use crate::heap::{GenericFields, MethodFields, RootSlot, Word};
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime};

use super::{classes, equality, generic, proper_list};

#[derive(Clone, Copy)]
enum Op {
    Find,
    Qualifiers,
    Applicable,
    Add,
    Remove,
}

pub(crate) fn initialize(runtime: &Runtime) -> Result<(), EvalError> {
    for (name, op, signature, specializers) in [
        (
            "FIND-METHOD",
            Op::Find,
            "(generic-function qualifiers specializers &optional errorp)",
            &["STANDARD-GENERIC-FUNCTION", "T", "T"][..],
        ),
        (
            "METHOD-QUALIFIERS",
            Op::Qualifiers,
            "(method)",
            &["METHOD"][..],
        ),
        (
            "COMPUTE-APPLICABLE-METHODS",
            Op::Applicable,
            "(generic-function arguments)",
            &["STANDARD-GENERIC-FUNCTION", "T"][..],
        ),
        (
            "ADD-METHOD",
            Op::Add,
            "(generic-function method)",
            &["STANDARD-GENERIC-FUNCTION", "METHOD"][..],
        ),
        (
            "REMOVE-METHOD",
            Op::Remove,
            "(generic-function method)",
            &["STANDARD-GENERIC-FUNCTION", "METHOD"][..],
        ),
    ] {
        let generic = generic::BuiltinGeneric::standard(runtime, name, signature)?;
        let callback = {
            let mut natives = runtime.natives.borrow_mut();
            let function = Word::native(natives.len());
            natives.push(Arc::new(move |runtime, args, at| {
                let result = run(runtime, op, &args[2..], at)?;
                Ok(vec![runtime.root(result)])
            }));
            function
        };
        generic.add(specializers, callback)?;
    }
    Ok(())
}

fn run(runtime: &Runtime, op: Op, args: &[Root<'_>], at: usize) -> Result<Word, Fault> {
    let get = |index: usize| args[index].slot.get();
    let target = get(0);
    match op {
        Op::Qualifiers => {
            let list = method_info(runtime, target, at)?.qualifiers;
            list_from_words(
                runtime,
                proper_list(runtime, list, at).expect("registered method qualifier list"),
                at,
            )
        }
        Op::Find => find(
            runtime,
            target,
            get(1),
            get(2),
            args.len() == 3 || get(3) != Word::NIL,
            at,
        ),
        Op::Applicable => {
            let info = generic_info(runtime, target, at)?;
            let required = info.call_signature.required;
            let arguments = checked_list(runtime, get(1), at)?;
            let count = arguments.len().min(required);
            let argument_frame = runtime.root_frame(count);
            for (index, argument) in arguments.into_iter().take(count).enumerate() {
                argument_frame.set(index, argument);
            }
            let methods =
                generic::applicable_methods(runtime, target, &argument_frame, 0..count, at)?;
            list_from_words(runtime, methods, at)
        }
        Op::Add => {
            add(runtime, target, get(1), at)?;
            Ok(get(0))
        }
        Op::Remove => {
            remove(runtime, target, get(1), at)?;
            Ok(target)
        }
    }
}

/// Relation spines are private. Readers return a fresh spine, so deletion can
/// unlink a cell without allocation even while an update callback is unwinding.
pub(super) fn change_relation(
    rt: &Runtime,
    object: Word,
    member: Word,
    dependents: bool,
    add: bool,
    at: usize,
) -> Result<(), Fault> {
    let head = rt
        .heap
        .mop_relations(object, dependents)
        .ok_or_else(|| program_error(at, "invalid metaobject relation"))?;
    let mut tail = head;
    let mut before = None;
    while let Some((entry, next)) = rt.heap.cons_fields(tail) {
        if entry == member {
            if !add {
                if let Some(prev) = before {
                    rt.heap.set_cons_cdr(prev, next);
                } else {
                    rt.heap.set_mop_relations(object, dependents, next);
                }
            }
            return Ok(());
        }
        before = Some(tail);
        tail = next;
    }
    if add {
        let obj = rt.root_slot(object);
        let item = rt.root_slot(member);
        let head = rt.root_slot(head);
        let new = rt
            .heap
            .cons(|| (item.get(), head.get()))
            .map_err(|e| EvalError::out_of_memory(at, e))?;
        rt.heap.set_mop_relations(obj.get(), dependents, new);
    }
    Ok(())
}

pub(super) fn update_specializer_direct_methods(
    rt: &Runtime,
    method: &RootSlot,
    add: bool,
    at: usize,
) -> Result<(), Fault> {
    let info = method_info(rt, method.get(), at)?;
    let mut seen = std::collections::HashSet::new();
    let specs = proper_list(rt, info.specializers, at)?
        .into_iter()
        .filter(|w| seen.insert(*w))
        .map(|word| rt.root_slot(word))
        .collect::<Vec<_>>();
    if !add {
        for spec in specs {
            change_relation(rt, spec.get(), method.get(), false, false, at)?;
        }
        return Ok(());
    }
    // Every new cons is rooted before any reverse edge is published. An
    // allocation failure therefore leaves all specializers at their old state.
    let mut changes = Vec::new();
    for spec in specs {
        let old = rt
            .heap
            .mop_relations(spec.get(), false)
            .ok_or_else(|| program_error(at, "invalid method specializer"))?;
        let mut tail = old;
        while let Some((entry, next)) = rt.heap.cons_fields(tail) {
            if entry == method.get() {
                break;
            }
            tail = next;
        }
        if tail == Word::NIL {
            let head = rt.root_slot(old);
            let entry = rt
                .heap
                .cons(|| (method.get(), head.get()))
                .map_err(|e| EvalError::out_of_memory(at, e))?;
            changes.push((spec, rt.root_slot(entry)));
        }
    }
    for (spec, head) in changes {
        rt.heap.set_mop_relations(spec.get(), false, head.get());
    }
    Ok(())
}

// Bootstrap definitions have no Lisp protocol to maintain their reverse links.
// Once the MOP is live, the ADD/REMOVE-DIRECT-METHOD methods own those updates.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MethodUpdate {
    Native,
    Protocol,
}

pub(super) fn check_method(
    runtime: &Runtime,
    generic: Word,
    method: Word,
    at: usize,
) -> Result<bool, Fault> {
    let gf = generic_info(runtime, generic, at)?;
    let method = method_info(runtime, method, at)?;
    if method.function == Word::UNBOUND || method.owner != Word::NIL {
        return Err(program_error(
            at,
            "method is uninitialized or already attached",
        ));
    }
    if gf.lambda_list == Word::UNBOUND {
        return Ok(false);
    }
    generic::check_method_congruence(runtime, generic, method.lambda_list, at)?;
    Ok(true)
}

pub(crate) fn add(runtime: &Runtime, generic: Word, method: Word, at: usize) -> Result<(), Fault> {
    if generic != Word::NIL && method_info(runtime, method, at)?.owner == generic {
        return Ok(());
    }
    attach(runtime, generic, method, MethodUpdate::Native, at)
}

pub(super) fn attach(
    runtime: &Runtime,
    generic: Word,
    method: Word,
    update: MethodUpdate,
    at: usize,
) -> Result<(), Fault> {
    let generic = runtime.root_slot(generic);
    let method = runtime.root_slot(method);
    if !check_method(runtime, generic.get(), method.get(), at)? {
        return Err(program_error(at, "generic function has no lambda list"));
    }

    let head = runtime.heap.generic_info(generic.get()).unwrap().methods;
    let info = runtime.heap.method_info(method.get()).unwrap();
    let mut cell = head;
    while let Some((previous, next)) = runtime.heap.cons_fields(cell) {
        let other = runtime
            .heap
            .method_info(previous)
            .expect("registered method");
        if equality::equal(&runtime.heap, info.qualifiers, other.qualifiers)
            && same_specializers(runtime, info.specializers, other.specializers)
        {
            // REMOVE-METHOD may leave the old method installed or install a
            // different method reentrantly. Never displace it behind the MOP.
            if update == MethodUpdate::Protocol {
                return Err(program_error(at, "an agreeing method is still attached"));
            }
            // The method list is private to the generic function; public
            // list-valued queries produce fresh spines.
            let cell = runtime.root_slot(cell);
            let previous = runtime.root_slot(previous);
            update_specializer_direct_methods(runtime, &method, true, at)?;
            update_specializer_direct_methods(runtime, &previous, false, at)?;
            runtime.heap.set_cons_car(cell.get(), method.get());
            runtime.heap.invalidate_standard_dispatch(generic.get());
            runtime.heap.set_method_owner(previous.get(), Word::NIL);
            runtime.heap.set_method_owner(method.get(), generic.get());
            if let Some(protocol) = runtime.invocation_protocol.get() {
                protocol.changed(runtime, generic.get());
            }
            return Ok(());
        }
        cell = next;
    }

    let head = runtime.root_slot(head);
    let updated = runtime
        .heap
        .cons(|| (method.get(), head.get()))
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    let updated = runtime.root_slot(updated);
    if update == MethodUpdate::Native {
        update_specializer_direct_methods(runtime, &method, true, at)?;
    }
    runtime
        .heap
        .set_generic_methods(generic.get(), updated.get());
    runtime.heap.set_method_owner(method.get(), generic.get());
    if let Some(protocol) = runtime.invocation_protocol.get() {
        protocol.changed(runtime, generic.get());
    }
    Ok(())
}

pub(crate) fn remove(
    runtime: &Runtime,
    generic: Word,
    method: Word,
    at: usize,
) -> Result<(), Fault> {
    detach(runtime, generic, method, MethodUpdate::Native, at)
}

pub(super) fn detach(
    runtime: &Runtime,
    generic: Word,
    method: Word,
    update: MethodUpdate,
    at: usize,
) -> Result<(), Fault> {
    let mut cell = generic_info(runtime, generic, at)?.methods;
    method_info(runtime, method, at)?;
    let mut previous = None;
    while let Some((entry, next)) = runtime.heap.cons_fields(cell) {
        if entry == method {
            if update == MethodUpdate::Native {
                update_specializer_direct_methods(runtime, &runtime.root_slot(method), false, at)?;
            }
            if let Some(previous) = previous {
                runtime.heap.set_cons_cdr(previous, next);
                runtime.heap.invalidate_standard_dispatch(generic);
            } else {
                runtime.heap.set_generic_methods(generic, next);
            }
            runtime.heap.set_method_owner(method, Word::NIL);
            if let Some(protocol) = runtime.invocation_protocol.get() {
                protocol.changed(runtime, generic);
            }
            break;
        }
        previous = Some(cell);
        cell = next;
    }
    Ok(())
}

fn find(
    runtime: &Runtime,
    generic: Word,
    qualifiers: Word,
    specializers: Word,
    errorp: bool,
    at: usize,
) -> Result<Word, Fault> {
    let generic = runtime.root_slot(generic);
    let qualifiers = runtime.root_slot(qualifiers);
    let info = generic_info(runtime, generic.get(), at)?;
    checked_list(runtime, qualifiers.get(), at)?;
    let specializers = checked_list(runtime, specializers, at)?;
    if specializers.len() != required_parameters(runtime, info.lambda_list).len() {
        return Err(program_error(
            at,
            "FIND-METHOD specializer count must match the generic function's required arguments",
        ));
    }
    let specializers = specializers
        .into_iter()
        .map(|word| runtime.root_slot(word))
        .collect::<Vec<_>>();
    let eql = runtime.root_slot(standard_symbol(runtime, "EQL"));
    let specializers = specializers
        .iter()
        .map(|word| {
            let word = word.get();
            if runtime.heap.is_specializer(word) {
                return Ok(runtime.root_slot(word));
            }
            if runtime.heap.is_symbol(word) {
                let class = classes::find_named_class(runtime, word, at)?
                    .ok_or_else(|| program_error(at, "no class with that name"))?;
                return Ok(runtime.root_slot(class));
            }
            if let Some((operator, operands)) = runtime.heap.cons_fields(word)
                && operator == eql.get()
                && let Some((value, Word::NIL)) = runtime.heap.cons_fields(operands)
            {
                return super::mop::intern_eql_specializer(runtime, value, at)
                    .map(|word| runtime.root_slot(word));
            }
            Err(program_error(
                at,
                "FIND-METHOD expects a class or (EQL value) specializer",
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut methods = generic_info(runtime, generic.get(), at)?.methods;
    while let Some((method, next)) = runtime.heap.cons_fields(methods) {
        let info = runtime.heap.method_info(method).expect("registered method");
        if equality::equal(&runtime.heap, qualifiers.get(), info.qualifiers) {
            let mut candidates = info.specializers;
            let matched = specializers.iter().all(|wanted| {
                let Some((candidate, next)) = runtime.heap.cons_fields(candidates) else {
                    return false;
                };
                candidates = next;
                wanted.get() == candidate
            });
            if matched && candidates == Word::NIL {
                return Ok(method);
            }
        }
        methods = next;
    }

    if errorp {
        Err(program_error(
            at,
            "no method with the requested qualifiers and specializers",
        ))
    } else {
        Ok(Word::NIL)
    }
}

fn same_specializers(runtime: &Runtime, mut left: Word, mut right: Word) -> bool {
    while let (Some((a, left_rest)), Some((b, right_rest))) = (
        runtime.heap.cons_fields(left),
        runtime.heap.cons_fields(right),
    ) {
        if a != b {
            return false;
        }
        left = left_rest;
        right = right_rest;
    }
    left == Word::NIL && right == Word::NIL
}

pub(super) fn required_parameters(runtime: &Runtime, mut lambda_list: Word) -> Vec<Word> {
    let markers = ["&OPTIONAL", "&REST", "&KEY", "&ALLOW-OTHER-KEYS", "&AUX"]
        .map(|name| standard_symbol(runtime, name));
    let mut parameters = Vec::new();
    while let Some((parameter, rest)) = runtime.heap.cons_fields(lambda_list) {
        if markers.contains(&parameter) {
            break;
        }
        parameters.push(parameter);
        lambda_list = rest;
    }
    parameters
}

fn standard_symbol(runtime: &Runtime, name: &str) -> Word {
    runtime
        .find_named_symbol(&Name::common_lisp(name))
        .expect("canonical symbol")
}

fn generic_info(runtime: &Runtime, word: Word, at: usize) -> Result<GenericFields, Fault> {
    runtime.heap.generic_info(word).ok_or_else(|| {
        Fault::type_error(runtime, 
            word,
            "GENERIC-FUNCTION",
            EvalError::new(at, "expected a generic function"),
        )
    })
}

fn method_info(runtime: &Runtime, word: Word, at: usize) -> Result<MethodFields, Fault> {
    runtime
        .heap
        .method_info(word)
        .ok_or_else(|| Fault::type_error(runtime, word, "METHOD", EvalError::new(at, "expected a method")))
}

fn checked_list(runtime: &Runtime, list: Word, at: usize) -> Result<Vec<Word>, Fault> {
    proper_list(runtime, list, at).map_err(|error| Fault::type_error(runtime, list, "LIST", error))
}

pub(crate) fn list_from_words(
    runtime: &Runtime,
    words: Vec<Word>,
    at: usize,
) -> Result<Word, Fault> {
    let roots = runtime.root_frame(words.len() + 1);
    let tail = words.len();
    for (index, word) in words.into_iter().enumerate() {
        roots.set(index, word);
    }
    for index in (0..tail).rev() {
        let pair = runtime
            .heap
            .cons(|| (roots.get(index), roots.get(tail)))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        roots.set(tail, pair);
    }
    Ok(roots.get(tail))
}

fn program_error(at: usize, message: &str) -> Fault {
    Fault::plain(Class::ProgramError, EvalError::new(at, message))
}
