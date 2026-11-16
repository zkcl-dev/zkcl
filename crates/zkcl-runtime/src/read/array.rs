use std::rc::Rc;

use crate::EvalError;
use crate::form_value;
use crate::heap::{ArrayElement, Word};
use crate::vm::arrays::{self, RANK_LIMIT};
use zkcl_syntax::syntax::SyntaxKind;

use super::{Form, FormKind, ReadMode, Reader, Token, quasiquote};

impl Reader<'_> {
    pub(super) fn vector(
        &mut self,
        opener: &Token<'_>,
        depth: usize,
        backquotes: usize,
    ) -> Result<Form, EvalError> {
        let length = length_argument(opener, opener.text.len() - 1)?;
        let mut elements = Vec::new();
        loop {
            let token = self
                .next_object_token(depth, backquotes, ReadMode::Normal)?
                .ok_or_else(|| self.unexpected_eof("unterminated vector"))?;
            if token.kind == SyntaxKind::CloseParen {
                break;
            }
            if token.is_dot() {
                return Err(token.error("dot is not permitted in a vector"));
            }
            elements.push(self.form(token, depth, backquotes)?);
        }
        let total = vector_size(opener, elements.len(), length)?;
        if backquotes > 0 {
            if total > elements.len() {
                let last = elements.last().expect("explicit length validated").clone();
                elements
                    .try_reserve(total - elements.len())
                    .map_err(|_| EvalError::out_of_memory(opener.at, crate::OutOfMemory))?;
                elements.resize(total, last);
            }
            return Ok(Form::new(
                opener.at,
                quasiquote::vector(opener.at, elements),
            ));
        }
        let values = elements
            .iter()
            .map(|form| form_value::to_value(self.runtime, form))
            .collect::<Result<Vec<_>, _>>()?;
        let word = self
            .runtime
            .heap
            .array(&[total], ArrayElement::General, false, None, |index| {
                values[index.min(values.len() - 1)].slot.get()
            })
            .map_err(|error| EvalError::out_of_memory(opener.at, error))?;
        Ok(Form::new(
            opener.at,
            FormKind::Literal(Rc::new(self.runtime.root_slot(word))),
        ))
    }

    pub(super) fn bit_vector(&mut self, opener: &Token<'_>) -> Result<FormKind, EvalError> {
        let (prefix, digits) = opener.text.split_once('*').expect("bit vector dispatch");
        let length = length_argument(opener, prefix.len())?;
        let total = vector_size(opener, digits.len(), length)?;
        if !digits.bytes().all(|digit| matches!(digit, b'0' | b'1')) {
            return Err(opener.error("expected binary digits after #*"));
        }
        let word = self
            .runtime
            .heap
            .array(&[total], ArrayElement::Bit, false, None, |index| {
                let bit = digits.as_bytes()[index.min(digits.len() - 1)] - b'0';
                Word::fixnum(isize::from(bit)).expect("bit is a fixnum")
            })
            .map_err(|error| EvalError::out_of_memory(opener.at, error))?;
        Ok(FormKind::Literal(Rc::new(self.runtime.root_slot(word))))
    }

    pub(super) fn ranked_array(
        &mut self,
        opener: &Token<'_>,
        depth: usize,
        backquotes: usize,
    ) -> Result<FormKind, EvalError> {
        let rank = length_argument(opener, opener.text.len() - 1)?
            .filter(|&rank| rank < RANK_LIMIT)
            .ok_or_else(|| opener.error("#A requires a rank below ARRAY-RANK-LIMIT"))?;
        let form = self.required_form(depth, backquotes)?;
        let contents = form_value::to_value(self.runtime, &form)?;
        let word = arrays::array_from_contents(self.runtime, contents.slot.get(), rank, opener.at)
            .map_err(|fault| {
                let error = fault.host;
                if error.kind() == crate::EvalErrorKind::OutOfMemory {
                    error
                } else {
                    opener.error(error.message())
                }
            })?;
        Ok(FormKind::Literal(Rc::new(self.runtime.root_slot(word))))
    }
}

fn length_argument(token: &Token<'_>, end: usize) -> Result<Option<usize>, EvalError> {
    let digits = &token.text[1..end];
    if digits.is_empty() {
        return Ok(None);
    }
    digits
        .parse()
        .map(Some)
        .map_err(|_| token.error("array length or rank is too large"))
}

fn vector_size(
    token: &Token<'_>,
    supplied: usize,
    length: Option<usize>,
) -> Result<usize, EvalError> {
    let total = length.unwrap_or(supplied);
    if supplied > total || supplied == 0 && total > 0 || total >= Word::FIXNUM_MAX as usize {
        return Err(token.error("vector elements do not match its length argument"));
    }
    Ok(total)
}
