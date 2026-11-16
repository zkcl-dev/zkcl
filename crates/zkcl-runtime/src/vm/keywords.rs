use std::ops::Range;

use crate::condition::{Class, Fault};
use crate::heap::{RootSlots, Word};
use crate::package::Package;
use crate::{EvalError, Runtime};

#[derive(Debug, thiserror::Error)]
pub(super) enum KeywordArgumentError {
    #[error("odd number of keyword arguments")]
    OddCount,
    #[error("unrecognized keyword argument {0}")]
    Unrecognized(String),
    #[error("keyword argument names must be symbols")]
    NonSymbol,
}

impl KeywordArgumentError {
    pub(super) fn at(self, at: usize) -> EvalError {
        EvalError::from_error(at, self)
    }

    pub(super) fn program_error(self, at: usize) -> Fault {
        Fault::plain(Class::ProgramError, self.at(at))
    }
}

pub(super) fn keyword_arguments(
    runtime: &Runtime,
    names: &[Word],
    arguments: Range<usize>,
    argument: impl Fn(usize) -> Word,
    callee_allows_other_keys: bool,
    allow_other_keys_symbol: Word,
) -> Result<Vec<Option<usize>>, KeywordArgumentError> {
    let mut selected = vec![None; names.len()];
    scan_keyword_arguments(
        runtime,
        |index| names[index],
        arguments,
        argument,
        callee_allows_other_keys,
        allow_other_keys_symbol,
        &mut selected,
    )?;
    Ok(selected)
}

fn scan_keyword_arguments(
    runtime: &Runtime,
    key: impl Fn(usize) -> Word,
    arguments: Range<usize>,
    argument: impl Fn(usize) -> Word,
    callee_allows_other_keys: bool,
    allow_other_keys_symbol: Word,
    selected: &mut [Option<usize>],
) -> Result<(), KeywordArgumentError> {
    let tail_len = arguments.len();
    let pairs_end = arguments.start + tail_len / 2 * 2;
    let mut caller_allows_other_keys = None;
    let mut unrecognized = None;
    for index in (arguments.start..pairs_end).step_by(2) {
        let name = argument(index);
        let mut recognized = name == allow_other_keys_symbol;
        if recognized && caller_allows_other_keys.is_none() {
            caller_allows_other_keys = Some(argument(index + 1) != Word::NIL);
        }
        for (slot, matched) in selected.iter_mut().enumerate() {
            if key(slot) == name {
                matched.get_or_insert(index + 1);
                recognized = true;
            }
        }
        if !recognized {
            unrecognized.get_or_insert(name);
        }
    }
    if !callee_allows_other_keys && caller_allows_other_keys != Some(true) {
        if !tail_len.is_multiple_of(2) {
            return Err(KeywordArgumentError::OddCount);
        }
        if let Some(name) = unrecognized {
            return Err(match runtime.heap.symbol_name(name) {
                Some(name) => KeywordArgumentError::Unrecognized(name),
                None => KeywordArgumentError::NonSymbol,
            });
        }
    }
    Ok(())
}

pub(crate) fn named_keyword_arguments<const N: usize>(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    names: [&str; N],
    at: usize,
) -> Result<[Option<usize>; N], Fault> {
    let mut selected = [None; N];
    named_keyword_arguments_into(runtime, frame, args, &names, &mut selected, at)?;
    Ok(selected)
}

pub(super) fn named_keyword_arguments_allowing_other_keys<const N: usize>(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    names: [&str; N],
    at: usize,
) -> Result<[Option<usize>; N], Fault> {
    let mut selected = [None; N];
    select_named_arguments(runtime, frame, args, &names, true, &mut selected, at)?;
    Ok(selected)
}

pub(super) fn named_keyword_arguments_into(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    names: &[&str],
    selected: &mut [Option<usize>],
    at: usize,
) -> Result<(), Fault> {
    select_named_arguments(runtime, frame, args, names, false, selected, at)
}

fn select_named_arguments(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    names: &[&str],
    callee_allows_other_keys: bool,
    selected: &mut [Option<usize>],
    at: usize,
) -> Result<(), Fault> {
    debug_assert_eq!(names.len(), selected.len());
    selected.fill(None);
    if args.is_empty() {
        return Ok(());
    }
    let roots = runtime.root_frame(names.len() + 1);
    for (index, name) in names
        .iter()
        .copied()
        .chain(["ALLOW-OTHER-KEYS"])
        .enumerate()
    {
        let (key, _) = runtime.intern_package(Package::KEYWORD, name, at)?;
        roots.set(index, key);
    }
    scan_keyword_arguments(
        runtime,
        |index| roots.get(index),
        args,
        |index| frame.get(index),
        callee_allows_other_keys,
        roots.get(names.len()),
        selected,
    )
    .map_err(|error| error.program_error(at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn keyword_argument_errors_surface_as_program_errors_with_the_original_cause() {
        let runtime = Runtime::start().unwrap();
        for (arguments, message) in [
            (":x", "odd number of keyword arguments"),
            (":unknown 2", "unrecognized keyword argument UNKNOWN"),
            ("1 2", "keyword argument names must be symbols"),
        ] {
            let form = format!("((lambda (&key x) x) {arguments})");
            let error = runtime
                .eval(&form)
                .err()
                .expect("invalid keyword arguments");
            assert_eq!(
                error.kind(),
                crate::EvalErrorKind::LispCondition(Class::ProgramError)
            );
            let cause = error
                .source()
                .and_then(|source| source.downcast_ref::<KeywordArgumentError>())
                .expect("keyword argument cause");
            assert_eq!(cause.to_string(), message);
        }
    }
}
