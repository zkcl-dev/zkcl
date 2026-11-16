use std::rc::Rc;

use rustc_hash::FxHashMap;

use super::{
    BindingLocation, Compiler, Destination, Form, FormKind, Ins, RegisterBinding, core_lower,
};
use crate::core_ir::{Tag, TagLabels};
use crate::vm::{Immediate, JumpTable};
use crate::{EvalError, Runtime};

impl Tag {
    pub(super) fn from_form(runtime: &Runtime, form: &Form) -> Option<Self> {
        match &form.kind {
            FormKind::Name(name) => Some(Self::Symbol(name.key().clone())),
            FormKind::Integer(value) => Some(Self::Integer(*value)),
            FormKind::Literal(root) => {
                let word = root.get();
                if let Some(value) = word.as_fixnum() {
                    Some(Self::Integer(value))
                } else {
                    runtime.heap.integer(word).map(Self::Bignum)
                }
            }
            _ => None,
        }
    }
}

/// Classify the original forms once, before expanding any statement. The first
/// segment precedes the first tag; subsequent segments have labels 0, 1, ...
pub(super) struct TagbodyForms<'a> {
    pub(super) labels: TagLabels,
    pub(super) statements: Vec<&'a [Form]>,
}

impl<'a> TagbodyForms<'a> {
    pub(super) fn parse(runtime: &Runtime, forms: &'a [Form]) -> Result<Self, EvalError> {
        let mut labels = FxHashMap::default();
        let mut statements = Vec::new();
        let mut start = 0;
        for (i, form) in forms.iter().enumerate() {
            if let Some(tag) = Tag::from_form(runtime, form) {
                let label = labels.len();
                if labels.insert(tag, label).is_some() {
                    return Err(EvalError::compile(form.at, "duplicate TAGBODY tag"));
                }
                statements.push(&forms[start..i]);
                start = i + 1;
            } else if !matches!(form.kind, FormKind::List(..))
                && form.object_needing_syntax().is_none()
            {
                return Err(EvalError::compile(
                    form.at,
                    "expected a TAGBODY tag or a compound statement",
                ));
            }
        }
        statements.push(&forms[start..]);
        Ok(Self {
            labels: Rc::new(labels),
            statements,
        })
    }
}

#[derive(Clone)]
pub(super) struct TagScope {
    pub(super) labels: TagLabels,
    pub(super) token: RegisterBinding,
}

impl Compiler<'_> {
    pub(super) fn go_form(&mut self, args: &[Form], at: usize) -> Result<(), EvalError> {
        let arity = || core_lower::application_arity_error("GO", at);
        let [name] = args else {
            return Err(arity());
        };
        let tag = Tag::from_form(self.runtime, name)
            .ok_or_else(|| EvalError::compile(name.at, "expected a symbol or integer tag"))?;
        let (location, label) = self
            .tag(&tag)
            .ok_or_else(|| EvalError::compile(name.at, "no lexically enclosing TAGBODY tag"))?;
        let token = self.reserve(1).start;
        self.read_location(location, token);
        self.emit(Ins::Go { token, label, at });
        Ok(())
    }

    pub(super) fn tag(&mut self, name: &Tag) -> Option<(BindingLocation, usize)> {
        let lookup = |scope: &TagScope| {
            scope
                .labels
                .get(name)
                .map(|&label| (scope.token.clone(), label))
        };
        if let Some((token, label)) = self.tags.iter().rev().find_map(lookup) {
            return Some((BindingLocation::Local(token), label));
        }
        let (token, label) = self.outer_tags.iter().rev().find_map(lookup)?;
        Some((BindingLocation::Capture(self.capture(token)), label))
    }

    pub(super) fn emit_tagbody_entry(
        &mut self,
        label_count: usize,
        at: usize,
    ) -> (usize, RegisterBinding) {
        let token = self.emit_exit_token(at);
        let table = self.code.jump_tables.len();
        self.code.jump_tables.push(JumpTable {
            targets: vec![0; label_count],
        });
        self.emit(Ins::EnterTagbody {
            token: token.reg,
            jumps: table,
            clear: self.used,
        });
        (table, token)
    }

    pub(super) fn tagbody(
        &mut self,
        forms: &[Form],
        dest: Destination,
        at: usize,
    ) -> Result<(), EvalError> {
        let TagbodyForms { labels, statements } = TagbodyForms::parse(self.runtime, forms)?;
        let mark = self.tags.len();
        let jumps = if labels.is_empty() {
            None
        } else {
            let (table, token) = self.emit_tagbody_entry(labels.len(), at);
            self.tags.push(TagScope { labels, token });
            Some(table)
        };
        for (segment, forms) in statements.into_iter().enumerate() {
            if let Some(label) = segment.checked_sub(1) {
                let table = jumps.expect("a labeled segment has a jump table");
                self.code.jump_tables[table].targets[label] = self.code.instructions.len();
            }
            for form in forms {
                let mark = self.used;
                let scratch = self.reserve(1).start;
                self.form(form, Destination::One(scratch))?;
                self.release(mark);
            }
        }
        if jumps.is_some() {
            self.emit(Ins::LeaveTarget);
        }
        self.tags.truncate(mark);
        self.scalar(dest, |out| Ins::Load(out, Immediate::Nil));
        Ok(())
    }
}
