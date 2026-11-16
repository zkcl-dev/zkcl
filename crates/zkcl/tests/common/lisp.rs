use std::path::Path;

use zkcl::{EvalError, Root, Runtime};

#[track_caller]
pub fn eval<'rt>(runtime: &'rt Runtime, source: &str) -> Vec<Root<'rt>> {
    match runtime.eval(source) {
        Ok(values) => values,
        Err(error) => panic!("evaluation failed for {source}: {error}"),
    }
}

#[track_caller]
pub fn eval_error(runtime: &Runtime, source: &str) -> EvalError {
    match runtime.eval(source) {
        Ok(_) => panic!("expected evaluation to fail for {source}"),
        Err(error) => error,
    }
}

#[track_caller]
pub fn eval_fixnums(runtime: &Runtime, source: &str) -> Vec<isize> {
    let values = eval(runtime, source);
    let mut fixnums = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let Some(fixnum) = value.as_fixnum() else {
            panic!("expected a fixnum at value index {index}: {source}");
        };
        fixnums.push(fixnum);
    }
    fixnums
}

#[track_caller]
pub fn eval_reprs(runtime: &Runtime, source: &str) -> Vec<String> {
    let values = eval(runtime, source);
    let mut representations = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        match runtime.repr(value) {
            Ok(repr) => representations.push(repr),
            Err(error) => panic!("printing value at index {index} from {source} failed: {error}"),
        }
    }
    representations
}

#[track_caller]
pub fn assert_truthy(runtime: &Runtime, source: &str) {
    let values = eval(runtime, source);
    assert!(
        values.first().is_some_and(|value| !value.is_nil()),
        "expected a non-NIL primary value: {source}"
    );
}

pub fn lisp_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[track_caller]
pub fn lisp_path(path: &Path) -> String {
    lisp_string(path.to_str().expect("a UTF-8 test path"))
}
