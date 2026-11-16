use zkcl::syntax::{SyntaxDiagnosticKind, parse_source};
use zkcl::{EvalErrorKind, Runtime};

#[test]
fn runtime_reader_matches_source_token_boundaries() {
    let runtime = Runtime::start().unwrap();
    let source = r#"(values "雪\"λ\\" #| outer #| nested |# |# 'inner#name -42.) #| trailing |#"#;
    assert!(parse_source(source).diagnostics().is_empty());
    let values = runtime.eval(source).unwrap();
    assert_eq!(runtime.string(&values[0]).as_deref(), Some("雪\"λ\\"));
    assert_eq!(
        runtime.symbol_name(&values[1]).as_deref(),
        Some("INNER#NAME")
    );
    assert_eq!(values[2].as_fixnum(), Some(-42));

    let invalid = "(values \"雪\" #| incomplete";
    let parsed = parse_source(invalid);
    let diagnostic = parsed
        .diagnostics()
        .iter()
        .find(|diagnostic| diagnostic.kind() == SyntaxDiagnosticKind::UnterminatedBlockComment)
        .unwrap();
    let start = invalid.find("#|").unwrap();
    let error = runtime.eval(invalid).err().unwrap();
    assert_eq!(error.kind(), EvalErrorKind::Read);
    assert_eq!(error.byte_range(), start..invalid.len());
    assert_eq!(u32::from(diagnostic.range().start()) as usize, start);
    assert_eq!(u32::from(diagnostic.range().end()) as usize, invalid.len());
    assert_eq!(error.message(), diagnostic.message());

    for source in ["#2'list", "#2| comment |# 42"] {
        let error = runtime.eval(source).err().unwrap();
        assert_eq!(error.kind(), EvalErrorKind::Read);
        assert_eq!(error.offset(), 0);
    }
}
