use std::fmt::Write;

use crate::condition::Fault;
use crate::heap::Word;
use crate::{EvalError, Root, Runtime};

// The Lisp cache retains at most sixteen strings of this size. The encoding
// does not retain any of the Lisp objects from which a key was derived.
const MAX_KEY_BYTES: usize = 8 * 1024;

pub(super) fn key<'rt>(
    rt: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let [input] = args else {
        return Err(EvalError::new(at, "expected a coverage key form").into());
    };
    let mut text = String::new();
    let mut pending = vec![input.slot.get()];
    // A cons is a prefix followed by two terms. Distinct scalar tags and
    // delimited numbers make the encoding injective over the accepted trees.
    // No Lisp code or GC safepoint runs while traversing the rooted input.
    while let Some(word) = pending.pop() {
        if word == Word::NIL {
            text.push('n');
        } else if let Some((head, tail)) = rt.heap.cons_fields(word) {
            if tail != Word::NIL && rt.heap.cons_fields(tail).is_none() {
                return Ok(vec![rt.nil()]);
            }
            text.push('p');
            pending.push(tail);
            pending.push(head);
        } else if let Some(number) = word.as_fixnum() {
            write!(text, "i{number};").unwrap();
        } else if let Some(character) = word.as_character() {
            write!(text, "c{};", u32::from(character)).unwrap();
        } else if let Some(symbol) = rt.heap.symbol_attributes(word) {
            // Assigned once by this runtime, including for uninterned symbols.
            // Package names and symbol spellings are not identity.
            write!(text, "s{};", symbol.id).unwrap();
        } else {
            return Ok(vec![rt.nil()]);
        }
        if text.len() > MAX_KEY_BYTES {
            return Ok(vec![rt.nil()]);
        }
    }
    // A cache miss is sufficient if the optional key cannot be allocated.
    let key = rt.heap.string(&text).unwrap_or(Word::NIL);
    Ok(vec![rt.root(key)])
}
