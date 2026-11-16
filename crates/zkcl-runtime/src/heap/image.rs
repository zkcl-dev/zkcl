//! Image objects use typed constructors and the collector's slot descriptors.
//! No process addresses, Rust layouts, or executable pointers enter the file.
use std::cell::Cell as LocalCell;

use super::*;
use crate::EvalError;
use crate::package::Package;
use crate::symbol::SymbolAttributes;

pub(super) struct Data<'a>(pub(super) &'a [u64]);
impl Data<'_> {
    pub(super) fn number(&mut self) -> Result<usize, EvalError> {
        let (first, tail) = self.0.split_first().ok_or_else(invalid)?;
        self.0 = tail;
        usize::try_from(*first).map_err(|_| invalid())
    }
    pub(super) fn flag(&mut self) -> Result<bool, EvalError> {
        match self.number()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(invalid()),
        }
    }
    pub(super) fn done(self) -> Result<(), EvalError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(invalid())
        }
    }
}
pub(super) fn invalid() -> EvalError {
    EvalError::new(0, "invalid runtime image object shape")
}

struct Slots(Vec<ValueSlot>);
impl SlotVisitor for Slots {
    fn visit_slot(&mut self, slot: ValueSlot) {
        self.0.push(slot);
    }
}
fn slots(word: Word) -> Vec<ValueSlot> {
    let mut visitor = Slots(Vec::new());
    if let Some(base) = object_address(word, ObjectKind::GenericFunction) {
        generic::scan_generic_state(base, &mut visitor);
        return visitor.0;
    }
    scan_object_slots(
        word.object_address().expect("image heap object"),
        &mut visitor,
        &mut weak::WeakObjects::default(),
        &mut FxHashSet::default(),
    );
    if let Some(slot) = weak::image_slot(word) {
        visitor.0.push(slot);
    }
    visitor.0
}
impl Heap {
    pub(crate) fn image_object(&self, word: Word) -> Option<(String, Vec<u64>, Vec<Word>)> {
        let mut shape = None;
        if let Some(symbol) = self.object_ref::<Symbol>(word) {
            let a = symbol.attributes.get();
            shape = Some((
                "symbol",
                vec![
                    a.id as u64,
                    a.home
                        .map_or(0, |p| p.word().package_index().unwrap() as u64 + 1),
                    a.canonical as u64,
                    a.constant as u64,
                    a.defined_constant as u64,
                    a.has_flag(SymbolAttributes::SPECIAL) as u64,
                    a.has_flag(SymbolAttributes::MACRO) as u64,
                ],
            ));
        } else if let Some(condition) = self.object_ref::<LispCondition>(word) {
            shape = Some((
                "condition",
                vec![condition.class as u64, condition.count as u64],
            ));
        } else if self.object_ref::<LispRestart>(word).is_some() {
            shape = Some(("restart", vec![]));
        } else if let Some(environment) = self.object_ref::<LispMacroEnvironment>(word) {
            shape = Some(("macro-environment", vec![environment.id as u64]));
        }
        let shape = shape
            .or_else(|| readtable::image_shape(self, word))
            .or_else(|| class::image_shape(self, word))
            .or_else(|| instance::image_shape(self, word))
            .or_else(|| generic::image_shape(self, word))
            .or_else(|| mop::image_shape(self, word))
            .or_else(|| structure::image_shape(self, word))
            .or_else(|| stream::image_shape(self, word))
            .or_else(|| weak::image_shape(self, word))
            .or_else(|| treelist::image_shape(self, word));
        let (kind, data) = shape?;
        let edges = slots(word)
            .into_iter()
            .map(|slot| unsafe { Self::load_word_slot(slot.0) })
            .collect();
        Some((kind.to_owned(), data, edges))
    }
    pub(crate) fn image_shell(&self, kind: &str, data: &[u64]) -> Result<Word, EvalError> {
        let mut d = Data(data);
        let oom = |error| EvalError::out_of_memory(0, error);
        let word = match kind {
            "symbol" => {
                let id = d.number()?;
                let home = d.number()?;
                let mut a = SymbolAttributes::new(
                    id,
                    home.checked_sub(1)
                        .map(|id| Package::from_word(Word::package(id)).unwrap()),
                    d.flag()?,
                    d.flag()?,
                );
                a.defined_constant = d.flag()?;
                let a = a
                    .with_flag(SymbolAttributes::SPECIAL, d.flag()?)
                    .with_flag(SymbolAttributes::MACRO, d.flag()?);
                d.done()?;
                self.alloc_object(|header| Symbol {
                    header,
                    value: WordSlot::unbound(),
                    function: WordSlot::unbound(),
                    setter: WordSlot::unbound(),
                    name: WordSlot::nil(),
                    variable_doc: WordSlot::nil(),
                    symbol_macro: WordSlot::unbound(),
                    attributes: std::cell::Cell::new(a),
                    function_revision: LocalCell::new(0),
                    tls_index: std::sync::atomic::AtomicU32::new(0),
                })
                .map_err(oom)?
            }
            "restart" => {
                d.done()?;
                self.restart(|| Word::NIL).map_err(oom)?
            }
            "macro-environment" => {
                let id = d.number()?;
                d.done()?;
                self.macro_environment(id).map_err(oom)?
            }
            "condition" => {
                let tag = d.number()?;
                let count = d.number()?;
                d.done()?;
                let class = Class::all()
                    .find(|class| *class as usize == tag)
                    .ok_or_else(invalid)?;
                self.alloc_with_trailing(
                    count,
                    |header| LispCondition {
                        header,
                        class,
                        count,
                    },
                    |_| WordSlot::nil(),
                )
                .map_err(oom)?
            }
            _ => {
                return readtable::image_allocate(self, kind, data)?
                    .or(class::image_allocate(self, kind, data)?)
                    .or(instance::image_allocate(self, kind, data)?)
                    .or(generic::image_allocate(self, kind, data)?)
                    .or(mop::image_allocate(self, kind, data)?)
                    .or(structure::image_allocate(self, kind, data)?)
                    .or(stream::image_allocate(self, kind, data)?)
                    .or(weak::image_allocate(self, kind, data)?)
                    .or(treelist::image_allocate(self, kind, data)?)
                    .ok_or_else(invalid);
            }
        };
        Ok(word)
    }
    pub(crate) fn image_fill(&self, word: Word, values: &[Word]) -> Result<(), EvalError> {
        let target = slots(word);
        if target.len() != values.len() {
            return Err(invalid());
        }
        let owner = word.object_address().expect("image object");
        for (slot, &value) in target.into_iter().zip(values) {
            unsafe {
                self.write_word_slot(owner, slot.0, value);
            }
        }
        Ok(())
    }
}
