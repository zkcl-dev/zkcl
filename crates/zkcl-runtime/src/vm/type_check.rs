use crate::condition::{Class, Fault};
use crate::heap::{ListSpine, RootSlot, RootSlots, Word};
use crate::{EvalError, EvalErrorKind, Root, Runtime};

use super::{Continuation, ContinuationStep, Resume, Values, types};

/// Expands a derived type specifier once by calling its DEFTYPE expander with
/// the literal type specifier arguments.
///
/// The result is the specifier the expander returned; `None` means `spec` is
/// not a derived type.  TYPEP and SUBTYPEP run this synchronously when they
/// are entered from a primitive call rather than through the continuation
/// machine below.
pub(super) fn expand_derived(runtime: &Runtime, spec: Word) -> Result<Option<Word>, Fault> {
    let Some((expander, arguments)) = derived_expander(runtime, spec) else {
        return Ok(None);
    };
    expand(runtime, expander, arguments, 0).map(Some)
}

pub(super) fn expand_derived_type(
    runtime: &Runtime,
    mut spec: Word,
    at: usize,
) -> Result<Word, Fault> {
    for _ in 0..TYPE_NESTING_LIMIT {
        let Some((expander, arguments)) = derived_expander(runtime, spec) else {
            return Ok(spec);
        };
        spec = expand(runtime, expander, arguments, at)?;
    }
    Err(Fault::plain(
        Class::ProgramError,
        EvalError::new(at, "type specifier nesting exceeds the limit"),
    ))
}

pub(super) fn expand(
    runtime: &Runtime,
    expander: Word,
    arguments: Word,
    at: usize,
) -> Result<Word, Fault> {
    expand_with(runtime, expander, arguments, at, None)
}

pub(crate) fn expand_with(
    runtime: &Runtime,
    expander: Word,
    arguments: Word,
    at: usize,
    call: Option<&crate::macros::CallExpander<'_>>,
) -> Result<Word, Fault> {
    let arguments = argument_words(runtime, arguments, at)?;
    let target = runtime.root(expander);
    let arguments: Vec<Root<'_>> = arguments
        .iter()
        .map(|&argument| runtime.root(argument))
        .collect();
    let references: Vec<&Root<'_>> = arguments.iter().collect();
    if let Some(call) = call {
        return call(
            &target,
            &references,
            at,
            crate::macros::ExpansionKind::Other,
        )
        .map_err(callback_fault);
    }
    let values = runtime.call(&target, &references).map_err(callback_fault)?;
    Ok(values.first().map_or(Word::NIL, |value| value.slot.get()))
}

pub(super) fn callback_fault(error: EvalError) -> Fault {
    match error.kind() {
        EvalErrorKind::LispCondition(class) => Fault::plain(class, error),
        _ => Fault::plain(Class::ProgramError, error),
    }
}

pub(super) fn derived_expander(runtime: &Runtime, spec: Word) -> Option<(Word, Word)> {
    let (name, arguments) = runtime.heap.cons_fields(spec).unwrap_or((spec, Word::NIL));
    runtime
        .type_expander(name)
        .map(|expander| (expander, arguments))
}

/// Recognize array type checks whose element-type upgrade cannot call Lisp.
pub(super) fn is_immediate_array_check(runtime: &Runtime, spec: Word) -> bool {
    let heap = &runtime.heap;
    let Some((kind, arguments)) = heap.cons_fields(spec) else {
        return false;
    };
    if !matches!(
        types::specifier_name(runtime, kind).as_deref(),
        Some("ARRAY" | "SIMPLE-ARRAY" | "VECTOR")
    ) || derived_expander(runtime, spec).is_some()
    {
        return false;
    }
    let Some((element, _dimensions)) = heap.cons_fields(arguments) else {
        return arguments == Word::NIL;
    };
    if derived_expander(runtime, element).is_some() {
        return false;
    }
    heap.is_symbol(element)
        || heap.cons_fields(element).is_some_and(|(head, _)| {
            matches!(
                types::specifier_name(runtime, head).as_deref(),
                Some("SIGNED-BYTE" | "UNSIGNED-BYTE")
            )
        })
}

pub(super) fn argument_words(
    runtime: &Runtime,
    arguments: Word,
    at: usize,
) -> Result<Vec<Word>, Fault> {
    types::arguments(&runtime.heap, arguments, usize::MAX).ok_or_else(|| malformed_specifier(at))
}

fn malformed_specifier(at: usize) -> Fault {
    Fault::plain(
        Class::ProgramError,
        EvalError::new(at, "malformed type specifier"),
    )
}

pub(crate) const TYPE_NESTING_LIMIT: usize = 256;

/// A pending check. `And`/`Or` frames are re-entered after each member has
/// produced a boolean, which is how the machine short-circuits without
/// recursing on the Rust stack.
enum Task {
    Check(RootSlot, usize),
    Combine {
        items: Box<[RootSlot]>,
        index: usize,
        depth: usize,
        all: bool,
    },
    Not,
}

#[derive(Clone, Copy)]
enum After {
    /// A derived type expander returned the type specifier to check next.
    Expand(usize),
    /// A SATISFIES predicate returned whether the value matches.
    Satisfies,
}

/// TYPEP over a type specifier that names derived types or contains
/// SATISFIES. Both need to call back into Lisp, so the check runs as a state
/// machine on the VM: derived specifiers expand through their expander, and a
/// SATISFIES clause calls its predicate on the value. Everything else is a
/// leaf for the base matcher in `types`.
pub(super) struct TypeCheck {
    value: RootSlot,
    tasks: Vec<Task>,
    matched: bool,
    after: Option<After>,
    resume: Resume,
    at: usize,
}

impl TypeCheck {
    /// Returns `None` when the specifier needs no Lisp callback, leaving the
    /// caller on the base matcher's path.
    pub(super) fn new(
        runtime: &Runtime,
        frame: &RootSlots,
        args: std::ops::Range<usize>,
        resume: Resume,
        at: usize,
    ) -> Result<Option<Self>, Fault> {
        let environment = args.len() == 3 && frame.get(args.start + 2) != Word::NIL;
        let spec = frame.get(args.start + 1);
        // TYPEP ignores its deprecated environment argument, but the primitive
        // rejects a non-nil one; handle the check here instead so the argument
        // it is documented to ignore does not turn into an error.
        if !environment && !needs_machine(runtime, spec, 0) {
            return Ok(None);
        }
        let check = Self {
            value: runtime.root_slot(frame.get(args.start)),
            tasks: vec![Task::Check(runtime.root_slot(spec), 0)],
            matched: false,
            after: None,
            resume,
            at,
        };
        Ok(Some(check))
    }

    fn check(
        &mut self,
        runtime: &Runtime,
        spec: Word,
        depth: usize,
    ) -> Result<Option<ContinuationStep>, Fault> {
        if depth >= TYPE_NESTING_LIMIT {
            return Err(Fault::plain(
                Class::Error,
                EvalError::new(self.at, "type specifier nesting exceeds the limit"),
            ));
        }
        if let Some((expander, arguments)) = derived_expander(runtime, spec) {
            let values = argument_words(runtime, arguments, self.at)?;
            let arguments = runtime.root_frame(values.len());
            for (index, value) in values.into_iter().enumerate() {
                arguments.set(index, value);
            }
            self.after = Some(After::Expand(depth + 1));
            return Ok(Some(ContinuationStep::Call {
                target: expander,
                args: arguments,
                at: self.at,
            }));
        }
        if let Some((head, rest)) = runtime.heap.cons_fields(spec) {
            let operator = types::specifier_name(runtime, head);
            match operator.as_deref() {
                Some("AND" | "OR") => {
                    let all = operator.as_deref() == Some("AND");
                    let items = argument_words(runtime, rest, self.at)?
                        .into_iter()
                        .map(|value| runtime.root_slot(value))
                        .collect();
                    self.tasks.push(Task::Combine {
                        items,
                        index: 0,
                        depth: depth + 1,
                        all,
                    });
                    return Ok(None);
                }
                Some("NOT") => {
                    let operand = types::single_argument(&runtime.heap, rest)
                        .ok_or_else(|| malformed_specifier(self.at))?;
                    self.tasks.push(Task::Not);
                    self.tasks
                        .push(Task::Check(runtime.root_slot(operand), depth + 1));
                    return Ok(None);
                }
                Some("SATISFIES") => {
                    let predicate = types::single_argument(&runtime.heap, rest)
                        .ok_or_else(|| malformed_specifier(self.at))?;
                    let predicate = satisfies_predicate(runtime, predicate, self.at)?;
                    let arguments = runtime.root_frame(1);
                    arguments.set(0, self.value.get());
                    self.after = Some(After::Satisfies);
                    return Ok(Some(ContinuationStep::Call {
                        target: predicate,
                        args: arguments,
                        at: self.at,
                    }));
                }
                _ => {}
            }
        }
        self.matched = types::typep_at_depth(runtime, self.value.get(), spec, self.at, depth)?;
        Ok(None)
    }
}

impl Continuation for TypeCheck {
    fn step(
        &mut self,
        runtime: &Runtime,
        values: &Values,
        _returned: bool,
    ) -> Result<ContinuationStep, Fault> {
        if let Some(after) = self.after.take() {
            match after {
                After::Expand(depth) => self
                    .tasks
                    .push(Task::Check(runtime.root_slot(values.get(0)), depth)),
                After::Satisfies => self.matched = values.get(0) != Word::NIL,
            }
        }
        loop {
            let Some(task) = self.tasks.pop() else {
                return Ok(ContinuationStep::Done(if self.matched {
                    Word::T
                } else {
                    Word::NIL
                }));
            };
            match task {
                Task::Check(spec, depth) => {
                    if let Some(step) = self.check(runtime, spec.get(), depth)? {
                        return Ok(step);
                    }
                }
                Task::Not => self.matched = !self.matched,
                Task::Combine {
                    items,
                    index,
                    depth,
                    all,
                } => {
                    if index > 0 && self.matched != all {
                        continue;
                    }
                    if index == items.len() {
                        self.matched = all;
                        continue;
                    }
                    let item = items[index].clone();
                    self.tasks.push(Task::Combine {
                        items,
                        index: index + 1,
                        depth,
                        all,
                    });
                    self.tasks.push(Task::Check(item, depth));
                }
            }
        }
    }

    fn resume_to(&self) -> Resume {
        self.resume
    }
}

/// Whether the specifier can be decided without calling Lisp: a derived type
/// name or a SATISFIES clause anywhere reachable through AND, OR and NOT.
fn needs_machine(runtime: &Runtime, spec: Word, depth: usize) -> bool {
    if depth >= TYPE_NESTING_LIMIT || derived_expander(runtime, spec).is_some() {
        return true;
    }
    let Some((head, rest)) = runtime.heap.cons_fields(spec) else {
        return false;
    };
    match types::specifier_name(runtime, head).as_deref() {
        Some("AND" | "OR") => {
            let mut spine = ListSpine::new(rest);
            loop {
                match spine.next(&runtime.heap) {
                    Ok(Some((item, _))) => {
                        if needs_machine(runtime, item, depth + 1) {
                            return true;
                        }
                    }
                    // A circular or improper argument list is an error the
                    // machine reports with the specifier's position.
                    Ok(None) => return spine.tail() != Word::NIL,
                    Err(_) => return true,
                }
            }
        }
        Some("NOT") => match runtime.heap.cons_fields(rest) {
            Some((operand, _)) => needs_machine(runtime, operand, depth + 1),
            None => false,
        },
        Some("SATISFIES") => true,
        _ => false,
    }
}

pub(super) fn satisfies_predicate(runtime: &Runtime, name: Word, at: usize) -> Result<Word, Fault> {
    if !runtime.heap.is_symbol(name) {
        return Err(Fault::type_error(
            runtime,
            name,
            "SYMBOL",
            EvalError::new(at, "SATISFIES expects a predicate name"),
        ));
    }
    super::function_cell(runtime, name, at)
}
