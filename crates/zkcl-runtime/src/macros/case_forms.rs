use crate::heap::Word;
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime};

use super::{Expansion, call, fresh, invalid, list, places, proper_list, symbol};

enum Match {
    Key,
    Type,
}

enum Miss<'rt, 'env> {
    Nil,
    Error,
    Store(&'env Root<'rt>),
}

pub(super) fn case<'rt>(runtime: &'rt Runtime, operands: &[Root<'rt>]) -> Expansion<'rt> {
    expand(runtime, operands, "CASE", Match::Key, Miss::Nil)
}

pub(super) fn ecase<'rt>(runtime: &'rt Runtime, operands: &[Root<'rt>]) -> Expansion<'rt> {
    expand(runtime, operands, "ECASE", Match::Key, Miss::Error)
}

pub(super) fn ccase<'rt>(
    runtime: &'rt Runtime,
    operands: &[Root<'rt>],
    env: &Root<'rt>,
) -> Expansion<'rt> {
    expand(runtime, operands, "CCASE", Match::Key, Miss::Store(env))
}

pub(super) fn typecase<'rt>(runtime: &'rt Runtime, operands: &[Root<'rt>]) -> Expansion<'rt> {
    expand(runtime, operands, "TYPECASE", Match::Type, Miss::Nil)
}

pub(super) fn etypecase<'rt>(runtime: &'rt Runtime, operands: &[Root<'rt>]) -> Expansion<'rt> {
    expand(runtime, operands, "ETYPECASE", Match::Type, Miss::Error)
}

pub(super) fn ctypecase<'rt>(
    runtime: &'rt Runtime,
    operands: &[Root<'rt>],
    env: &Root<'rt>,
) -> Expansion<'rt> {
    expand(
        runtime,
        operands,
        "CTYPECASE",
        Match::Type,
        Miss::Store(env),
    )
}

fn expand<'rt>(
    runtime: &'rt Runtime,
    operands: &[Root<'rt>],
    operator: &str,
    matcher: Match,
    miss: Miss<'rt, '_>,
) -> Expansion<'rt> {
    let Some((input, clauses)) = operands.split_first() else {
        return Err(invalid(operator, "requires a key and optional clauses"));
    };
    let key = fresh(runtime, "CASE KEY")?;
    let place = match miss {
        Miss::Store(env) => Some(places::expand_place(runtime, input.clone(), env, 0)?),
        _ => None,
    };
    let mut bindings = if let Some(place) = &place {
        place.bindings(runtime)?
    } else {
        Vec::new()
    };
    let reader = place.as_ref().map_or(input, |place| &place.reader);
    bindings.push(list(runtime, [&key, reader].into_iter())?);
    let bindings = list(runtime, bindings.iter())?;

    let otherwise = symbol(runtime, "OTHERWISE")?;
    let truth = runtime.root(Word::T);
    let nil = runtime.nil();
    let mut branches = Vec::with_capacity(clauses.len() + 1);
    let mut accepted = Vec::new();
    for (index, clause) in clauses.iter().enumerate() {
        let clause = proper_list(runtime, clause.clone(), operator)?;
        let Some((selector, body)) = clause.split_first() else {
            return Err(invalid(operator, "each clause requires a selector"));
        };
        let default = matches!(miss, Miss::Nil)
            && (selector.slot.get() == otherwise.slot.get() || selector.slot.get() == Word::T);
        let test = if default && index + 1 == clauses.len() {
            truth.clone()
        } else if default
            && (matches!(matcher, Match::Key) || selector.slot.get() == otherwise.slot.get())
        {
            return Err(invalid(operator, "the otherwise clause must be last"));
        } else {
            match matcher {
                Match::Key => {
                    let keys = if selector.is_nil()
                        || runtime.heap.cons_fields(selector.slot.get()).is_some()
                    {
                        proper_list(runtime, selector.clone(), operator)?
                    } else {
                        vec![selector.clone()]
                    };
                    let test = match keys.as_slice() {
                        [] => nil.clone(),
                        [single] => {
                            let quoted = call(runtime, "QUOTE", std::iter::once(single))?;
                            call(runtime, "EQL", [&key, &quoted].into_iter())?
                        }
                        _ => {
                            let quoted = call(runtime, "QUOTE", std::iter::once(selector))?;
                            call(runtime, "MEMBER", [&key, &quoted].into_iter())?
                        }
                    };
                    accepted.extend(keys);
                    test
                }
                Match::Type => {
                    accepted.push(selector.clone());
                    let quoted = call(runtime, "QUOTE", std::iter::once(selector))?;
                    call(runtime, "TYPEP", [&key, &quoted].into_iter())?
                }
            }
        };
        // A clause without forms returns NIL, as an empty implicit PROGN does.
        branches.push(if body.is_empty() {
            list(runtime, [&test, &nil].into_iter())?
        } else {
            list(runtime, std::iter::once(&test).chain(body))?
        });
    }

    let retry = if matches!(miss, Miss::Nil) {
        None
    } else {
        let failure = type_error(runtime, &key, matcher, &accepted)?;
        if let Some(place) = place {
            let retry = fresh(runtime, "RETRY CASE")?;
            let store = restart_store(runtime, &key, &place)?;
            let failure = call(runtime, "RESTART-CASE", [&failure, &store].into_iter())?;
            let again = call(runtime, "GO", std::iter::once(&retry))?;
            branches.push(list(runtime, [&truth, &failure, &again].into_iter())?);
            Some(retry)
        } else {
            branches.push(list(runtime, [&truth, &failure].into_iter())?);
            None
        }
    };
    let mut body = call(runtime, "COND", branches.iter())?;
    if let Some(retry) = retry {
        let done = fresh(runtime, "CASE RESULT")?;
        let result = call(runtime, "RETURN-FROM", [&done, &body].into_iter())?;
        let loop_body = call(runtime, "TAGBODY", [&retry, &result].into_iter())?;
        body = call(runtime, "BLOCK", [&done, &loop_body].into_iter())?;
    }
    call(runtime, "LET*", [&bindings, &body].into_iter())
}

fn type_error<'rt>(
    runtime: &'rt Runtime,
    datum: &Root<'rt>,
    matcher: Match,
    accepted: &[Root<'rt>],
) -> Expansion<'rt> {
    let class = symbol(runtime, "TYPE-ERROR")?;
    let class = call(runtime, "QUOTE", std::iter::once(&class))?;
    let operator = match matcher {
        Match::Key => "MEMBER",
        Match::Type => "OR",
    };
    let expected = call(runtime, operator, accepted.iter())?;
    let expected = call(runtime, "QUOTE", std::iter::once(&expected))?;
    let datum_key = runtime.root(runtime.intern(&Name::keyword("DATUM"), 0)?);
    let expected_key = runtime.root(runtime.intern(&Name::keyword("EXPECTED-TYPE"), 0)?);
    call(
        runtime,
        "ERROR",
        [&class, &datum_key, datum, &expected_key, &expected].into_iter(),
    )
}

fn restart_store<'rt>(
    runtime: &'rt Runtime,
    key: &Root<'rt>,
    place: &places::Place<'rt>,
) -> Expansion<'rt> {
    let new = fresh(runtime, "NEW CASE KEY")?;
    let args = list(runtime, std::iter::once(&new))?;
    let stores = list(runtime, place.stores.iter())?;
    let write = call(
        runtime,
        "MULTIPLE-VALUE-BIND",
        [&stores, &new, &place.writer].into_iter(),
    )?;
    let update = call(runtime, "SETQ", [key, &new].into_iter())?;
    let store_value = symbol(runtime, "STORE-VALUE")?;
    let interactive = runtime.root(runtime.intern(&Name::keyword("INTERACTIVE"), 0)?);
    let prompt = prompt_for_value(runtime)?;
    list(
        runtime,
        [&store_value, &args, &interactive, &prompt, &write, &update].into_iter(),
    )
}

pub(super) fn prompt_for_value(runtime: &Runtime) -> Expansion<'_> {
    let debug_io = symbol(runtime, "*DEBUG-IO*")?;
    let prompt = runtime
        .heap
        .string("~&Supply a new value: ")
        .map_err(|error| EvalError::out_of_memory(0, error))?;
    let prompt = runtime.root(prompt);
    let display = call(runtime, "FORMAT", [&debug_io, &prompt].into_iter())?;
    let flush = call(runtime, "FINISH-OUTPUT", std::iter::once(&debug_io))?;
    let read = call(runtime, "READ", std::iter::once(&debug_io))?;
    let new = call(runtime, "EVAL", std::iter::once(&read))?;
    let argument_list = call(runtime, "LIST", std::iter::once(&new))?;
    call(
        runtime,
        "LAMBDA",
        [&runtime.nil(), &display, &flush, &argument_list].into_iter(),
    )
}
