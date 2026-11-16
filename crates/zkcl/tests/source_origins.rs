use zkcl::{DefinitionKind, Runtime, SourceDocument};

#[test]
fn editor_submission_keeps_revision_and_utf8_base_separate() {
    let runtime = Runtime::start().unwrap();
    let text = "; 雪🙂\n(defun editor-origin (x) (declare (optimize (debug 3))) (car x))";
    let document = SourceDocument::input(text)
        .with_uri("file:///workspace/example.lisp")
        .with_version(17)
        .with_base_offset(1024);
    runtime.load_document(&document).unwrap();
    let info = runtime.find_definitions("editor-origin").unwrap().remove(0);
    let source = info.source.unwrap();
    assert_eq!(source.id(), document.id());
    assert_eq!(source.uri(), Some("file:///workspace/example.lisp"));
    assert_eq!(source.version(), Some(17));
    assert_eq!(source.base_offset(), 1024);
    let start = text.find("(defun").unwrap();
    assert_eq!(info.byte_range, start..text.len());
    assert_eq!(
        source.absolute_range(info.byte_range),
        Some(1024 + start..1024 + text.len())
    );
    let error = runtime
        .eval("(editor-origin 3)")
        .err()
        .expect("the function fails");
    let source = error.stack_frames()[0].source.as_ref().unwrap();
    assert_eq!(source.id(), document.id());
    let car = text.find("(car").unwrap() + 1;
    assert_eq!(error.stack_frames()[0].source_range, Some(car..car + 3));
    assert!(
        document.absolute_range(3..4).is_none(),
        "range splits the UTF-8 comment character"
    );
}

#[test]
fn saved_function_origin_survives_new_definition() {
    let runtime = Runtime::start().unwrap();
    let old = SourceDocument::input("(defun origin-version (x) (+ x 1))")
        .with_uri("untitled:example")
        .with_version(1);
    runtime.load_document(&old).unwrap();
    let saved = runtime.eval("#'origin-version").unwrap().remove(0);
    let new = SourceDocument::input("(defun origin-version (x) (+ x 2))")
        .with_uri("untitled:example")
        .with_version(2);
    runtime.load_document(&new).unwrap();
    assert_eq!(
        runtime.definition_info(&saved).unwrap()[0]
            .source
            .as_ref()
            .unwrap()
            .version(),
        Some(1)
    );
    assert_eq!(
        runtime.find_definitions("origin-version").unwrap()[0]
            .source
            .as_ref()
            .unwrap()
            .version(),
        Some(2)
    );
}

#[test]
fn current_macro_class_variable_and_methods_have_origins() {
    let runtime = Runtime::start().unwrap();
    let document = SourceDocument::input("(defmacro origin-macro (x &optional y) `(list ,x ,y))\n(defparameter *origin-value* 1)\n(defclass origin-class () ())\n(defgeneric origin-generic (x))\n(defmethod origin-generic ((x origin-class)) x)")
        .with_uri("file:///definitions.lisp").with_version(3);
    runtime.load_document(&document).unwrap();
    for (name, kind) in [
        ("origin-macro", DefinitionKind::Macro),
        ("*origin-value*", DefinitionKind::Variable),
        ("origin-class", DefinitionKind::Class),
        ("origin-generic", DefinitionKind::GenericFunction),
    ] {
        let definitions = runtime.find_definitions(name).unwrap();
        let info = definitions.iter().find(|info| info.kind == kind).unwrap();
        assert_eq!(info.source.as_ref().unwrap().id(), document.id(), "{name}");
    }
    assert_eq!(
        runtime.find_definitions("origin-macro").unwrap()[0]
            .lambda_list
            .as_deref(),
        Some("(X &OPTIONAL Y)")
    );
    let generic = runtime.find_definitions("origin-generic").unwrap();
    let method = generic
        .iter()
        .find(|info| info.kind == DefinitionKind::Method)
        .unwrap();
    assert_eq!(method.source.as_ref().unwrap().id(), document.id());
    runtime.eval("(fmakunbound 'origin-macro)").unwrap();
    assert!(runtime.find_definitions("origin-macro").unwrap().is_empty());
}

#[test]
fn compiled_buffers_preserve_origin_and_lambda_expression() {
    let runtime = Runtime::start().unwrap();
    let document = SourceDocument::input("(defun compiled-origin (x) (+ x 1))\n(defparameter *compiled-origin* 7)\n(defclass compiled-origin-class () ())")
        .with_uri("file:///compiled-buffer.lisp").with_version(42).with_base_offset(50);
    let file = runtime.compile_document_to_bytecode(&document).unwrap();
    let bytes = file.encode().unwrap();
    let loaded = Runtime::start().unwrap();
    loaded.load_fasl(&bytes).unwrap();
    for name in [
        "compiled-origin",
        "*compiled-origin*",
        "compiled-origin-class",
    ] {
        let info = loaded.find_definitions(name).unwrap().remove(0);
        let source = info.source.unwrap();
        assert_eq!(source.uri(), document.uri());
        assert_eq!(source.version(), document.version());
        assert_eq!(source.base_offset(), 50);
        assert_eq!(source.text(), document.text());
    }
    let values = loaded.eval("(multiple-value-bind (expression closure-p name) (function-lambda-expression #'compiled-origin) (declare (ignore closure-p)) (values (first expression) (second expression) name))").unwrap();
    let reprs: Vec<_> = values
        .iter()
        .map(|value| loaded.repr(value).unwrap())
        .collect();
    assert_eq!(reprs, ["LAMBDA", "(X)", "COMPILED-ORIGIN"]);
}

#[test]
fn compiled_top_level_progn_keeps_definition_origins_in_source_order() {
    let runtime = Runtime::start().unwrap();
    let text = "(progn
                  (defclass compiled-class () ())
                  (defstruct compiled-structure value)
                  (define-condition compiled-condition (condition) ())
                  (defgeneric compiled-generic (x))
                  (defun compiled-function () 1))";
    let document = SourceDocument::input(text)
        .with_uri("file:///compiled-order.lisp")
        .with_version(8);
    let file = runtime.compile_document_to_bytecode(&document).unwrap();
    let expected = [
        ("COMPILED-CLASS", "(defclass compiled-class () ())"),
        ("COMPILED-STRUCTURE", "(defstruct compiled-structure value)"),
        (
            "COMPILED-CONDITION",
            "(define-condition compiled-condition (condition) ())",
        ),
        ("COMPILED-GENERIC", "(defgeneric compiled-generic (x))"),
        ("COMPILED-FUNCTION", "(defun compiled-function () 1)"),
    ];
    let definitions: Vec<_> = file
        .entries
        .iter()
        .flat_map(|entry| &entry.debug.definitions)
        .filter(|definition| {
            expected
                .iter()
                .any(|(name, _)| definition.name.as_str() == *name)
        })
        .map(|definition| (definition.name.as_str(), definition.range.clone()))
        .collect();
    assert_eq!(
        definitions,
        expected
            .iter()
            .map(|&(name, form)| {
                let start = text.find(form).unwrap();
                (name, Some(start..start + form.len()))
            })
            .collect::<Vec<_>>()
    );
}

#[test]
fn compiled_outer_macro_definitions_keep_the_macro_call_origin() {
    let runtime = Runtime::start().unwrap();
    let text = "(defmacro define-compiled-pair ()
               '(progn
                  (defun compiled-first () 1)
                  (defun compiled-second () 2)))
             (define-compiled-pair)";
    let document = SourceDocument::input(text)
        .with_uri("file:///compiled-macro-order.lisp")
        .with_version(9);
    let file = runtime.compile_document_to_bytecode(&document).unwrap();
    let call = text.find("(define-compiled-pair)").unwrap();
    let definitions: Vec<_> = file
        .entries
        .iter()
        .flat_map(|entry| &entry.debug.definitions)
        .filter(|definition| definition.kind == DefinitionKind::Function)
        .collect();
    assert_eq!(
        definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        ["COMPILED-FIRST", "COMPILED-SECOND"]
    );
    assert!(
        definitions
            .iter()
            .all(|definition| definition.range == Some(call..text.len())),
        "{definitions:?}"
    );
}

#[test]
fn changing_origin_on_a_cloned_snapshot_gets_a_new_identity() {
    let first = SourceDocument::input("x")
        .with_uri("untitled:x")
        .with_version(1);
    let second = first.clone().with_version(2);
    assert_ne!(first.id(), second.id());
    assert_eq!(first.version(), Some(1));
    assert_eq!(second.version(), Some(2));
}
