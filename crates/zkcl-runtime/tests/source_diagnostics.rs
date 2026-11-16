use std::path::PathBuf;
use zkcl_runtime::{EvalErrorContext, EvalStage, Runtime, SourceDocument};

#[test]
fn fasl_reports_original_macro_call_and_saved_source() {
    let runtime = Runtime::start().unwrap();
    let text = "(defmacro generated-car () '(car 1))\n(generated-car)";
    let document = SourceDocument::file(PathBuf::from("original.lisp"), Some(text.into()))
        .with_uri("file:///saved/original.lisp")
        .with_version(4);
    let compiled = runtime.compile_document_to_bytecode(&document).unwrap();
    let call = text.rfind("(generated-car)").unwrap();
    assert!(compiled.entries.iter().any(|entry| {
        entry.debug.sources.iter().any(|source| {
            source.origin == zkcl_bytecode::SourceOrigin::Macro
                && source.range == Some(call..text.len())
        })
    }));
    let bytes = compiled.encode().unwrap();
    let error = runtime.load_fasl(&bytes).unwrap_err();
    let original = error.source_document().expect("original FASL snapshot");
    assert_eq!(original.text(), Some(text));
    assert_eq!(original.path(), Some(std::path::Path::new("original.lisp")));
    assert_eq!(original.version(), Some(4));
    assert_eq!(error.source_range(), Some(call..text.len()));
    let report = error.to_diagnostic();
    assert_eq!(report.labels[0].span.range(), call..text.len());
    assert!(report.labels[0].message.contains("macro"));
    let provenance = report.labels[0].provenance.as_ref().unwrap();
    assert_eq!(provenance.origin, zkcl_diagnostics::SourceOrigin::Macro);
    assert_eq!(provenance.expansion[0].range(), call..text.len());
    assert_eq!(provenance.expansion[0].snapshot(), original.snapshot_id());
}

#[test]
fn compile_failure_keeps_named_buffer_and_exact_token() {
    let runtime = Runtime::start().unwrap();
    let document = SourceDocument::input("(let ((x 1)) (go missing))").with_uri("untitled:buffer");
    let error = runtime.compile_document_to_bytecode(&document).unwrap_err();
    assert_eq!(error.source_document().unwrap().id(), document.id());
    let start = document.text().unwrap().find("missing").unwrap();
    assert_eq!(error.source_range(), Some(start..start + 7));
    assert!(
        error
            .contexts()
            .contains(&EvalErrorContext::Stage(EvalStage::Compile))
    );
}

#[test]
fn host_file_failure_has_context_without_invented_position() {
    let runtime = Runtime::start().unwrap();
    let path = PathBuf::from("/nonexistent-zkcl-source-diagnostic/input.lisp");
    let error = runtime.load_file(&path).unwrap_err();
    assert_eq!(error.source_range(), None);
    assert!(error.to_diagnostic().labels.is_empty());
    assert!(
        error
            .contexts()
            .contains(&EvalErrorContext::SourceFile(path))
    );
    assert!(!error.to_string().contains("byte 0"));
}

#[test]
fn fasl_rejects_source_ranges_that_split_saved_utf8() {
    let runtime = Runtime::start().unwrap();
    let document = SourceDocument::input(";中\n(error \"failure\")");
    let mut compiled = runtime.compile_document_to_bytecode(&document).unwrap();
    let location = compiled
        .entries
        .iter_mut()
        .flat_map(|entry| &mut entry.debug.sources)
        .next()
        .expect("compiled source operand");
    location.range = Some(2..3);
    assert!(compiled.validate().is_err());
    assert!(compiled.encode().is_err());
}
