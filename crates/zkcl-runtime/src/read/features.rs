use super::{Form, FormKind};
use crate::heap::{ListSpine, Word};
use crate::symbol::{Name, Package};
use crate::{EvalError, Runtime};

pub(super) fn special_value(runtime: &Runtime, name: &str, at: usize) -> Result<Word, EvalError> {
    let (symbol, _) = runtime
        .packages
        .borrow()
        .present(Package::COMMON_LISP, name)
        .ok_or_else(|| EvalError::new(at, format!("missing compiler symbol {name}")))?;
    runtime
        .heap
        .symbol_cell(symbol, false)
        .ok_or_else(|| EvalError::read(at, format!("unbound reader variable {name}")))
}

pub(super) fn enabled(runtime: &Runtime, test: &Form) -> Result<bool, EvalError> {
    let features = special_value(runtime, "*FEATURES*", test.at)?;
    // Evaluation compares symbol identities without allocating Lisp objects.
    matches(runtime, test, features)
}

fn matches(runtime: &Runtime, test: &Form, features: Word) -> Result<bool, EvalError> {
    if let FormKind::Name(feature) = &test.kind {
        let feature = runtime
            .find_named_symbol(feature)
            .expect("the reader resolved feature names");
        let mut spine = ListSpine::new(features);
        let improper = || EvalError::read(test.at, "*FEATURES* must be a proper list");
        while let Some((head, _)) = spine.next(&runtime.heap).map_err(|_| improper())? {
            if head == feature {
                return Ok(true);
            }
        }
        if spine.tail() == Word::NIL {
            return Ok(false);
        }
        return Err(improper());
    }
    let FormKind::List(parts, None) = &test.kind else {
        return Err(EvalError::read(
            test.at,
            "expected a feature name or boolean feature expression",
        ));
    };
    let Some((operator, operands)) = parts.split_first() else {
        return Err(EvalError::read(test.at, "expected a feature operator"));
    };
    let operator_name = operator
        .name()
        .filter(|name| {
            matches!(
                name.package(),
                Some(Package::KEYWORD | Package::COMMON_LISP)
            )
        })
        .map(Name::spelling);
    match operator_name {
        Some("NOT") => {
            let [operand] = operands else {
                return Err(EvalError::read(
                    operator.at,
                    "feature NOT expects exactly one operand",
                ));
            };
            Ok(!matches(runtime, operand, features)?)
        }
        Some("AND" | "OR") => {
            let conjunction = operator_name == Some("AND");
            for operand in operands {
                if matches(runtime, operand, features)? != conjunction {
                    return Ok(!conjunction);
                }
            }
            Ok(conjunction)
        }
        _ => Err(EvalError::read(operator.at, "unknown feature operator")),
    }
}
