use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::heap::Word;
use crate::read::{Form, FormKind};
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime};

const MAX_FORM_DEPTH: usize = 256;

pub(crate) fn to_value<'rt>(runtime: &'rt Runtime, form: &Form) -> Result<Root<'rt>, EvalError> {
    Ok(runtime.root(materialize(runtime, form)?))
}

fn materialize(runtime: &Runtime, form: &Form) -> Result<Word, EvalError> {
    if let Some(value) = form.original_object() {
        return Ok(value.get());
    }
    let allocation_error = |error| EvalError::out_of_memory(form.at, error);
    match &form.kind {
        FormKind::Integer(value) => Word::fixnum(*value).ok_or_else(|| {
            EvalError::macro_expansion(form.at, "integer exceeds the supported fixnum range")
        }),
        FormKind::Character(value) => Ok(Word::character(*value)),
        FormKind::String(text) => runtime.heap.string(text).map_err(allocation_error),
        FormKind::Literal(root) => Ok(root.get()),
        FormKind::Name(name) => runtime.intern(name, form.at),
        FormKind::List(items, tail) => {
            let tail = match tail {
                Some(tail) => materialize(runtime, tail)?,
                None => Word::NIL,
            };
            if items.is_empty() {
                return Ok(tail);
            }
            // Recursive conversion or the next cons allocation can move the
            // partial result. Reuse two traced slots for the spine and item.
            let slots = runtime.root_frame(2);
            slots.set(0, tail);
            for item in items.iter().rev() {
                slots.set(1, materialize(runtime, item)?);
                let pair = runtime
                    .heap
                    .cons(|| (slots.get(1), slots.get(0)))
                    .map_err(allocation_error)?;
                slots.set(0, pair);
            }
            Ok(slots.get(0))
        }
    }
}

pub(crate) fn from_value(
    runtime: &Runtime,
    value: &Root<'_>,
    at: usize,
) -> Result<Form, EvalError> {
    convert_value(runtime, value, at, false)
}

pub(crate) fn from_quasiquote_value(
    runtime: &Runtime,
    value: &Root<'_>,
    at: usize,
) -> Result<Form, EvalError> {
    convert_value(runtime, value, at, true)
}

fn convert_value(
    runtime: &Runtime,
    value: &Root<'_>,
    at: usize,
    in_quasiquote: bool,
) -> Result<Form, EvalError> {
    // Conversion performs only Rust allocations. The root keeps the entire form
    // reachable, and no Lisp values can move during this traversal.
    ValueToForm {
        runtime,
        at,
        in_quasiquote,
        active: FxHashSet::default(),
        spine: Vec::new(),
        names: FxHashMap::default(),
    }
    .convert(value.slot.get())
}

pub(crate) fn name_of_word(runtime: &Runtime, word: Word) -> Option<Name> {
    Name::from_word(runtime, word)
}

pub(crate) fn refresh_reader_syntax(
    runtime: &Runtime,
    form: &Form,
) -> Result<Option<Form>, EvalError> {
    if let Some(value) = form.object_needing_syntax() {
        // Executable use sees the object as it stands after reading, even
        // if a subsequent reader macro changed it through another reference.
        let mut decoded = from_value(runtime, &runtime.root(value.get()), form.at)?;
        let mut source = form.source.clone();
        source.origin = crate::read::FormOrigin::Reader;
        decoded.generated_from(source);
        return Ok(Some(decoded.retain_object(Rc::clone(value))));
    }
    Ok(None)
}

struct ValueToForm<'rt> {
    runtime: &'rt Runtime,
    at: usize,
    in_quasiquote: bool,
    active: FxHashSet<Word>,
    spine: Vec<Word>,
    names: FxHashMap<Word, Name>,
}

struct ListConversionFrame {
    word: Word,
    spine_start: usize,
    items: Vec<Form>,
    rest: ListContinuation,
}

enum ListContinuation {
    ConsTail(Word),
    DottedTail,
    QuotedOperand(Word),
}

enum FormConversionStep {
    ConvertObject(Word),
    FormReady(Form),
}

impl ValueToForm<'_> {
    fn convert(&mut self, word: Word) -> Result<Form, EvalError> {
        let mut lists = Vec::new();
        let mut step = FormConversionStep::ConvertObject(word);
        loop {
            let result = match step {
                FormConversionStep::ConvertObject(word) => self.convert_object(word, &mut lists),
                FormConversionStep::FormReady(form) if lists.is_empty() => return Ok(form),
                FormConversionStep::FormReady(form) => self.append_list_item(form, &mut lists),
            };
            step = match result {
                Ok(next) => next,
                Err(error) => {
                    // A quasiquote template may quote circular data. Keep the
                    // original object if converting the quoted operand fails.
                    let Some((index, operand)) =
                        lists.iter().enumerate().rev().find_map(|(index, frame)| {
                            match frame.rest {
                                ListContinuation::QuotedOperand(operand) => Some((index, operand)),
                                _ => None,
                            }
                        })
                    else {
                        return Err(error);
                    };
                    if let Some(child) = lists.get(index + 1) {
                        self.leave_list_spine(child.spine_start);
                        lists.truncate(index + 1);
                    }
                    FormConversionStep::FormReady(Form::new(self.at, literal(self.runtime, operand)))
                }
            };
        }
    }

    fn convert_object(
        &mut self,
        word: Word,
        lists: &mut Vec<ListConversionFrame>,
    ) -> Result<FormConversionStep, EvalError> {
        if lists.len() == MAX_FORM_DEPTH {
            return Err(EvalError::macro_expansion(
                self.at,
                "macro expansion exceeds the form nesting limit",
            ));
        }
        let kind = if let Some(number) = word.as_fixnum() {
            FormKind::Integer(number)
        } else if let Some(character) = word.as_character() {
            FormKind::Character(character)
        } else if let Some((first, tail)) = self.runtime.heap.cons_fields(word) {
            let spine_start = self.spine.len();
            self.enter_cons(word)?;
            lists.push(ListConversionFrame {
                word,
                spine_start,
                items: Vec::new(),
                rest: ListContinuation::ConsTail(tail),
            });
            return Ok(FormConversionStep::ConvertObject(first));
        } else if let Some(symbol) = self.symbol(word) {
            FormKind::Name(symbol)
        } else {
            literal(self.runtime, word)
        };
        Ok(FormConversionStep::FormReady(Form::new(self.at, kind)))
    }

    fn append_list_item(
        &mut self,
        form: Form,
        lists: &mut Vec<ListConversionFrame>,
    ) -> Result<FormConversionStep, EvalError> {
        let frame = lists.last_mut().expect("an enclosing list");
        let dotted = match frame.rest {
            ListContinuation::DottedTail => Some(Box::new(form)),
            ListContinuation::QuotedOperand(_) => {
                frame.items.push(form);
                None
            }
            ListContinuation::ConsTail(tail) => {
                let quote = frame.items.is_empty()
                    && form.name().is_some_and(|name| name.operator() == "QUOTE");
                frame.items.push(form);
                if let Some((item, rest)) = self.runtime.heap.cons_fields(tail) {
                    if quote && rest == Word::NIL {
                        if self.in_quasiquote {
                            // Inside a backquote, QUOTE's operand can contain commas.
                            frame.rest = ListContinuation::QuotedOperand(item);
                            return Ok(FormConversionStep::ConvertObject(item));
                        }
                        // (QUOTE name) can also be a binding list or CASE clause.
                        // Keep the symbol's syntax and identity for either use.
                        let kind = self
                            .symbol(item)
                            .map(FormKind::Name)
                            .unwrap_or_else(|| literal(self.runtime, item));
                        frame.items.push(Form::new(self.at, kind));
                    } else {
                        self.enter_cons(tail)?;
                        frame.rest = ListContinuation::ConsTail(rest);
                        return Ok(FormConversionStep::ConvertObject(item));
                    }
                } else if tail != Word::NIL {
                    frame.rest = ListContinuation::DottedTail;
                    return Ok(FormConversionStep::ConvertObject(tail));
                }
                None
            }
        };
        let frame = lists.pop().expect("finished list");
        self.leave_list_spine(frame.spine_start);
        // Re-materializing a macro call or literal must preserve object identity.
        let form = Form::new(self.at, FormKind::List(frame.items, dotted))
            .retain_object(Rc::new(self.runtime.root_slot(frame.word)));
        Ok(FormConversionStep::FormReady(form))
    }

    fn leave_list_spine(&mut self, mark: usize) {
        for word in self.spine.drain(mark..) {
            self.active.remove(&word);
        }
    }

    fn enter_cons(&mut self, cons: Word) -> Result<(), EvalError> {
        if !self.active.insert(cons) {
            return Err(EvalError::macro_expansion(
                self.at,
                "circular executable form",
            ));
        }
        self.spine.push(cons);
        Ok(())
    }

    fn symbol(&mut self, word: Word) -> Option<Name> {
        if let Some(name) = self.names.get(&word) {
            return Some(name.clone());
        }
        let name = Name::from_word(self.runtime, word)?;
        self.names.insert(word, name.clone());
        Some(name)
    }
}

fn literal(runtime: &Runtime, word: Word) -> FormKind {
    // The compiler shares this GC root until the literal is installed in a
    // materialized function's constant table.
    FormKind::Literal(Rc::new(runtime.root_slot(word)))
}
