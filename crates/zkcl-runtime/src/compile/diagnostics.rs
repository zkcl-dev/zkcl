use std::error::Error;
use std::rc::Rc;

use crate::condition::{self, Class, Fault};
use crate::heap::RootSlot;
use crate::macros::{RESIDUAL_MACRO_ERROR, compiled_native};
use crate::read::{Form, FormKind, FormOrigin, FormSource};
use crate::symbol::Name;
use crate::{EvalError, EvalErrorKind, Root, Runtime, form_value};
use crate::{EvalErrorContext, EvalStage, SourceDocument};
use std::ops::Range;
use std::path::PathBuf;

const HOST_DIAGNOSTICS: &[(&str, EvalErrorKind)] = &[
    ("read", EvalErrorKind::Read),
    ("compile", EvalErrorKind::Compile),
    ("macro-expansion", EvalErrorKind::MacroExpansion),
    ("host-callback", EvalErrorKind::HostCallback),
    ("out-of-memory", EvalErrorKind::OutOfMemory),
    ("cancelled", EvalErrorKind::Cancelled),
    ("runtime", EvalErrorKind::Runtime),
];

/// Leave a failed expansion at its original execution point. The diagnostic
/// accompanies the condition object because a PROGRAM-ERROR need not carry a
/// Lisp-readable message, and reporting a Lisp condition can run user code.
pub(crate) fn residual_macro_error(
    runtime: &Runtime,
    condition: &RootSlot,
    diagnostic: &EvalError,
    at: usize,
) -> Result<RootSlot, EvalError> {
    let node = |kind| Form::new(at, kind);
    let signal_expansion_error =
        compiled_native(RESIDUAL_MACRO_ERROR).expect("registered expansion failure");
    let mut nodes = vec![
        node(FormKind::Name(Name::common_lisp("FUNCALL"))),
        node(FormKind::Literal(Rc::new(runtime.root_slot(
            signal_expansion_error,
        )))),
        node(FormKind::Literal(Rc::new(runtime.root_slot(condition.get())))),
    ];
    let mut current = Some(diagnostic);
    while let Some(error) = current {
        let position = |offset| {
            isize::try_from(offset)
                .map(FormKind::Integer)
                .map_err(|_| EvalError::compile(at, "diagnostic position exceeds the fixnum range"))
        };
        let range = error.byte_range();
        let kind = error.kind();
        let category = if let EvalErrorKind::LispCondition(class) = kind {
            class.name()
        } else {
            HOST_DIAGNOSTICS
                .iter()
                .find_map(|&(label, mapped)| (mapped == kind).then_some(label))
                .expect("host diagnostic category")
        };
        let metadata = node(FormKind::List(
            vec![
                node(FormKind::Name(Name::common_lisp("QUOTE"))),
                encode_metadata(error, at)?,
            ],
            None,
        ));
        nodes.extend([
            node(FormKind::String(error.message().into())),
            node(position(range.start)?),
            node(position(range.end)?),
            node(FormKind::String(category.into())),
            metadata,
        ]);
        current = error
            .source()
            .and_then(|cause| cause.downcast_ref::<EvalError>());
    }
    let form = node(FormKind::List(nodes, None));
    Ok(form_value::to_value(runtime, &form)?.slot)
}

pub(crate) fn raise_residual_macro_error<'rt>(
    runtime: &'rt Runtime,
    args: &[Root<'rt>],
    at: usize,
) -> Result<Vec<Root<'rt>>, Fault> {
    let malformed = || EvalError::macro_expansion(at, "invalid compiled expansion diagnostic");
    let [condition, diagnostic @ ..] = args else {
        return Err(malformed().into());
    };
    let (descriptions, remainder) = diagnostic.as_chunks::<5>();
    if !condition::is_condition(runtime, condition.slot.get()) || !remainder.is_empty() {
        return Err(malformed().into());
    }
    let mut diagnostic = None;
    for [message, start, end, category, metadata] in descriptions.iter().rev() {
        let message = runtime.string(message).ok_or_else(malformed)?;
        let position = |value: &Root<'_>| {
            value
                .as_fixnum()
                .and_then(|position| usize::try_from(position).ok())
                .ok_or_else(malformed)
        };
        let range = position(start)?..position(end)?;
        if range.start > range.end {
            return Err(malformed().into());
        }
        let category = runtime.string(category).ok_or_else(malformed)?;
        let kind = Class::named(&category)
            .map(EvalErrorKind::LispCondition)
            .or_else(|| {
                HOST_DIAGNOSTICS
                    .iter()
                    .find_map(|&(label, kind)| (label == category).then_some(kind))
            })
            .ok_or_else(malformed)?;
        let metadata = form_value::from_value(runtime, metadata, at).map_err(|_| malformed())?;
        let error = EvalError::from_compiled_diagnostic(kind, range, message, diagnostic);
        diagnostic = Some(decode_metadata(runtime, error, &metadata).ok_or_else(malformed)?);
    }
    Err(Fault::existing_condition(
        condition,
        diagnostic.ok_or_else(malformed)?,
    ))
}

// This is data in the literal graph, never executable report text. The current
// format deliberately has no old-payload branch: source files are recompiled.
fn encode_metadata(error: &EvalError, at: usize) -> Result<Form, EvalError> {
    let list = |items| Form::new(at, FormKind::List(items, None));
    let text = |value: Option<&str>| {
        Form::new(
            at,
            value.map_or_else(
                || FormKind::Name(Name::common_lisp("NIL")),
                |text| FormKind::String(text.into()),
            ),
        )
    };
    let number = |value: usize| {
        isize::try_from(value)
            .map(|value| Form::new(at, FormKind::Integer(value)))
            .map_err(|_| EvalError::compile(at, "diagnostic metadata exceeds the fixnum range"))
    };
    let range = |value: &Range<usize>| {
        Ok::<_, EvalError>(list(vec![number(value.start)?, number(value.end)?]))
    };
    let document = if let Some(document) = error.source_document() {
        list(vec![
            text(document.path().and_then(|path| path.to_str())),
            text(document.text()),
            text(document.uri()),
            text(document.version().map(|value| value.to_string()).as_deref()),
            number(document.base_offset())?,
        ])
    } else {
        list(Vec::new())
    };
    let provenance = if let Some(source) = error.provenance() {
        list(vec![
            number(source.at)?,
            source
                .range
                .as_ref()
                .map(range)
                .transpose()?
                .unwrap_or_else(|| list(Vec::new())),
            text(Some(match source.origin {
                FormOrigin::Original => "original",
                FormOrigin::Reader => "reader",
                FormOrigin::Macro => "macro",
                FormOrigin::Compiler => "compiler",
            })),
            list(
                source
                    .expansion
                    .iter()
                    .map(range)
                    .collect::<Result<_, _>>()?,
            ),
        ])
    } else {
        list(Vec::new())
    };
    let mut contexts = Vec::new();
    for context in error.contexts() {
        let (tag, value) = match context {
            EvalErrorContext::SourceFile(path) => {
                let Some(path) = path.to_str() else {
                    continue;
                };
                ("source", text(Some(path)))
            }
            EvalErrorContext::Artifact(path) => {
                let Some(path) = path.to_str() else {
                    continue;
                };
                ("artifact", text(Some(path)))
            }
            EvalErrorContext::EmbeddedModule(module) => ("embedded", text(Some(module))),
            EvalErrorContext::Stage(stage) => (
                "stage",
                text(Some(match stage {
                    EvalStage::Read => "read",
                    EvalStage::Compile => "compile",
                    EvalStage::Load => "load",
                    EvalStage::Evaluate => "evaluate",
                })),
            ),
            EvalErrorContext::ReaderInput { range: value } => ("reader", range(value)?),
            EvalErrorContext::MacroExpansion { range: value } => (
                "macro",
                value
                    .as_ref()
                    .map(range)
                    .transpose()?
                    .unwrap_or_else(|| list(Vec::new())),
            ),
        };
        contexts.push(list(vec![text(Some(tag)), value]));
    }
    Ok(list(vec![
        number(usize::from(error.source_range().is_some()))?,
        document,
        provenance,
        list(contexts),
    ]))
}

fn decode_metadata(runtime: &Runtime, error: EvalError, metadata: &Form) -> Option<EvalError> {
    // Converting the constant's Lisp object graph preserves strings as literals.
    let text = |form: &Form| match &form.kind {
        FormKind::Literal(root) => runtime.heap.string_contents(root.get()),
        _ => None,
    };
    fn number(form: &Form) -> Option<usize> {
        if let FormKind::Integer(value) = form.kind {
            usize::try_from(value).ok()
        } else {
            None
        }
    }
    fn range(form: &Form) -> Option<Range<usize>> {
        let [start, end] = form.list()? else {
            return None;
        };
        let value = number(start)?..number(end)?;
        (value.start <= value.end).then_some(value)
    }
    let [known, document, provenance, contexts] = metadata.list()? else {
        return None;
    };
    let known = match number(known)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let document = match document.list()? {
        [] => None,
        [path, source, uri, version, base] => {
            let mut document = SourceDocument::new(text(path).map(PathBuf::from), text(source))
                .with_base_offset(number(base)?);
            if let Some(uri) = text(uri) {
                document = document.with_uri(uri);
            }
            if let Some(version) = text(version) {
                document = document.with_version(version.parse().ok()?);
            }
            Some(document)
        }
        _ => return None,
    };
    let provenance = match provenance.list()? {
        [] => None,
        [at, position, origin, expansion] => Some(FormSource {
            at: number(at)?,
            range: if position.list()?.is_empty() {
                None
            } else {
                Some(range(position)?)
            },
            origin: match text(origin)?.as_str() {
                "original" => FormOrigin::Original,
                "reader" => FormOrigin::Reader,
                "macro" => FormOrigin::Macro,
                "compiler" => FormOrigin::Compiler,
                _ => return None,
            },
            expansion: expansion
                .list()?
                .iter()
                .map(range)
                .collect::<Option<Vec<_>>>()?,
        }),
        _ => return None,
    };
    // Validate decoded source ranges against their own saved snapshot once.
    if let Some(document) = &document {
        if known {
            document.span(error.byte_range()).ok()?;
        }
        if let Some(source) = &provenance {
            if let Some(range) = &source.range {
                document.span(range.clone()).ok()?;
            }
            for range in &source.expansion {
                document.span(range.clone()).ok()?;
            }
        }
    }
    let contexts = contexts
        .list()?
        .iter()
        .map(|form| {
            let [tag, value] = form.list()? else {
                return None;
            };
            Some(match text(tag)?.as_str() {
                "source" => EvalErrorContext::SourceFile(PathBuf::from(text(value)?)),
                "artifact" => EvalErrorContext::Artifact(PathBuf::from(text(value)?)),
                "embedded" => EvalErrorContext::EmbeddedModule(text(value)?),
                "stage" => EvalErrorContext::Stage(match text(value)?.as_str() {
                    "read" => EvalStage::Read,
                    "compile" => EvalStage::Compile,
                    "load" => EvalStage::Load,
                    "evaluate" => EvalStage::Evaluate,
                    _ => return None,
                }),
                "reader" => EvalErrorContext::ReaderInput {
                    range: range(value)?,
                },
                "macro" => EvalErrorContext::MacroExpansion {
                    range: if value.list()?.is_empty() {
                        None
                    } else {
                        Some(range(value)?)
                    },
                },
                _ => return None,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(error.restore_compiled_metadata(document, known, provenance, contexts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_diagnostic_keeps_snapshot_and_structured_context() {
        let runtime = Runtime::start().unwrap();
        runtime.initialize_packages().unwrap();
        let document = SourceDocument::input("中文 (bad)")
            .with_uri("untitled:macro")
            .with_version(7);
        let error = EvalError::compile(7, "bad generated form")
            .in_document(&document)
            .with_context(EvalErrorContext::Stage(EvalStage::Compile));
        let encoded = encode_metadata(&error, 0).unwrap();
        let value = form_value::to_value(&runtime, &encoded).unwrap();
        let metadata = form_value::from_value(&runtime, &value, 0).unwrap();
        let restored =
            decode_metadata(&runtime, EvalError::compile(7, error.message()), &metadata).unwrap();
        assert_eq!(restored.source_document().unwrap().text(), document.text());
        assert_eq!(restored.source_document().unwrap().uri(), document.uri());
        assert_eq!(restored.source_document().unwrap().version(), Some(7));
        assert_eq!(restored.contexts(), error.contexts());
        assert_eq!(restored.source_range(), Some(7..7));
    }
}
