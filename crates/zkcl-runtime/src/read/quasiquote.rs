use super::{Form, FormKind};
use crate::EvalError;
use crate::symbol::Name;

// A deeper backquote is expanded while reading. These reader-private markers
// let an unquoted expression within that expansion reach an outer backquote.
const INSERT: &str = "\0insert";
const SPLICE: &str = "\0splice";
const VECTOR: &str = "\0vector-template";
pub(crate) const MARKERS: [&str; 3] = [INSERT, SPLICE, VECTOR];

pub(super) fn comma(at: usize, splice: bool, operand: Form) -> FormKind {
    call(at, if splice { SPLICE } else { INSERT }, vec![operand]).kind
}

pub(super) fn vector(at: usize, items: Vec<Form>) -> FormKind {
    call(at, VECTOR, vec![Form::new(at, FormKind::List(items, None))]).kind
}

pub(super) fn expand(template: Form) -> Result<Form, EvalError> {
    let at = template.at;
    match marker(&template) {
        Some(INSERT) => return Ok(operand(template)),
        Some(SPLICE) => return Err(EvalError::read(at, "splicing comma must be a list element")),
        Some(VECTOR) => {
            let items = expand(operand(template))?;
            let function = call(at, "FUNCTION", vec![symbol(at, "VECTOR")]);
            return Ok(call(at, "APPLY", vec![function, items]));
        }
        _ => {}
    }
    if !has_comma(&template) {
        return Ok(call(at, "QUOTE", vec![template]));
    }

    let FormKind::List(items, tail) = template.kind else {
        unreachable!("comma markers occur within lists")
    };
    let mut parts = Vec::new();
    let mut ordinary = Vec::new();
    for item in items {
        if marker(&item) == Some(SPLICE) {
            if !ordinary.is_empty() {
                parts.push(call(at, "LIST", std::mem::take(&mut ordinary)));
            }
            parts.push(operand(item));
        } else {
            ordinary.push(expand(item)?);
        }
    }
    if parts.is_empty() && tail.is_none() {
        return Ok(call(at, "LIST", ordinary));
    }
    if !ordinary.is_empty() {
        parts.push(call(at, "LIST", ordinary));
    }
    // APPEND treats its final argument as the tail. The explicit NIL ensures
    // that a splice in the last element position is still a list argument.
    parts.push(match tail {
        Some(tail) => expand(*tail)?,
        None => symbol(at, "NIL"),
    });
    Ok(call(at, "APPEND", parts))
}

fn has_comma(form: &Form) -> bool {
    marker(form).is_some()
        || matches!(&form.kind, FormKind::List(items, tail)
            if items.iter().any(has_comma) || tail.as_deref().is_some_and(has_comma))
}

fn marker(form: &Form) -> Option<&str> {
    let [head, _] = form.list()? else { return None };
    match head.name()?.operator() {
        marker @ (INSERT | SPLICE | VECTOR) => Some(marker),
        _ => None,
    }
}

fn operand(form: Form) -> Form {
    let FormKind::List(mut items, None) = form.kind else {
        unreachable!("a reader marker has one operand")
    };
    items.pop().unwrap()
}

fn symbol(at: usize, name: &str) -> Form {
    Form::new(at, FormKind::Name(Name::common_lisp(name)))
}

fn call(at: usize, operator: &str, arguments: Vec<Form>) -> Form {
    let mut items = Vec::with_capacity(arguments.len() + 1);
    items.push(symbol(at, operator));
    items.extend(arguments);
    Form::new(at, FormKind::List(items, None))
}
