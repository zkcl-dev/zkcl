use std::collections::HashSet;
use std::path::Path;

pub(super) use zkcl::NamespaceSnapshot as RuntimeSnapshot;
use zkcl::{EvalError, Runtime};

/// Escape a Rust string into a valid Common Lisp string literal.
pub(super) fn lisp_string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

pub(super) fn switch_package(runtime: &Runtime, name: &str) -> Result<String, EvalError> {
    let trimmed = name.trim().trim_start_matches("#:");
    let trimmed = trimmed.strip_prefix(':').unwrap_or(trimmed);
    let quoted = lisp_string_literal(trimmed);
    let code = format!(
        "(CL:LET ((pkg (CL:OR (CL:FIND-PACKAGE {quoted}) (CL:FIND-PACKAGE (CL:STRING-UPCASE {quoted})))))
           (CL:IF pkg
                  (CL:PROGN (CL:SETF CL:*PACKAGE* pkg) (CL:PACKAGE-NAME pkg))
                  (CL:ERROR \"Package ~A not found.\" {quoted})))"
    );
    let values = runtime.eval(&code)?;
    Ok(values
        .first()
        .and_then(|v| runtime.string(v))
        .unwrap_or_else(|| trimmed.to_ascii_uppercase()))
}

pub(super) fn sync_lisp_cwd(runtime: &Runtime, dir: &Path) -> Result<(), EvalError> {
    let mut path_str = dir.to_string_lossy().into_owned();
    if !path_str.ends_with('/') && !path_str.ends_with('\\') {
        path_str.push(std::path::MAIN_SEPARATOR);
    }
    let quoted = lisp_string_literal(&path_str);
    let code = format!("(CL:SETF CL:*DEFAULT-PATHNAME-DEFAULTS* (CL:PARSE-NAMESTRING {quoted}))");
    runtime.eval(&code)?;
    Ok(())
}

pub(super) fn eval_describe(runtime: &Runtime, form_text: &str) -> Result<String, EvalError> {
    let quoted = lisp_string_literal(form_text);
    let code = format!(
        "(CL:WITH-OUTPUT-TO-STRING (out)
           (CL:LET ((CL:*STANDARD-OUTPUT* out)
                    (obj (CL:EVAL (CL:READ-FROM-STRING {quoted}))))
             (CL:DESCRIBE obj out)
             (CL:WHEN (CL:SYMBOLP obj)
               (CL:LET ((pkg (CL:SYMBOL-PACKAGE obj)))
                 (CL:FORMAT out \"  Package: ~A~%\" (CL:IF pkg (CL:PACKAGE-NAME pkg) \"uninterned\")))
               (CL:WHEN (CL:BOUNDP obj)
                 (CL:FORMAT out \"  Value:   ~S~%\" (CL:SYMBOL-VALUE obj)))
               (CL:COND
                 ((CL:SPECIAL-OPERATOR-P obj)
                  (CL:FORMAT out \"  Binding: special operator~%\"))
                 ((CL:MACRO-FUNCTION obj)
                  (CL:FORMAT out \"  Binding: macro ~S~%\" (CL:MACRO-FUNCTION obj)))
                 ((CL:FBOUNDP obj)
                  (CL:FORMAT out \"  Binding: function ~S~%\" (CL:SYMBOL-FUNCTION obj))))
               (CL:DOLIST (doc-type '(CL:FUNCTION CL:VARIABLE CL:TYPE CL:STRUCTURE))
                 (CL:LET ((doc (CL:IGNORE-ERRORS (CL:DOCUMENTATION obj doc-type))))
                   (CL:WHEN doc
                     (CL:FORMAT out \"  Doc (~A): ~A~%\" doc-type doc))))
               (CL:WHEN (CL:SYMBOL-PLIST obj)
                 (CL:FORMAT out \"  Plist:   ~S~%\" (CL:SYMBOL-PLIST obj))))))"
    );
    let values = runtime.eval(&code)?;
    Ok(values
        .first()
        .and_then(|v| runtime.string(v))
        .unwrap_or_default())
}

pub(super) struct AproposEntry {
    pub qualified_name: String,
    pub kind_tags: Vec<&'static str>,
}

pub(super) fn eval_apropos(
    runtime: &Runtime,
    pattern: &str,
    package: Option<&str>,
) -> Result<Vec<AproposEntry>, EvalError> {
    let q_pat = lisp_string_literal(pattern);
    let pkg_expr = match package {
        Some(pkg) => {
            let q_pkg = lisp_string_literal(pkg.trim_start_matches("#:").trim_start_matches(':'));
            format!(
                "(CL:OR (CL:FIND-PACKAGE {q_pkg}) (CL:FIND-PACKAGE (CL:STRING-UPCASE {q_pkg})) (CL:ERROR \"Package ~A not found.\" {q_pkg}))"
            )
        }
        None => "CL:NIL".to_owned(),
    };
    let code = format!(
        "(CL:WITH-OUTPUT-TO-STRING (out)
           (CL:LET* ((pkg {pkg_expr})
                     (syms (CL:IF pkg
                                  (CL:APROPOS-LIST {q_pat} pkg)
                                  (CL:APROPOS-LIST {q_pat})))
                     (seen (CL:MAKE-HASH-TABLE :TEST 'CL:EQ)))
             (CL:DOLIST (s syms)
               (CL:UNLESS (CL:GETHASH s seen)
                 (CL:SETF (CL:GETHASH s seen) CL:T)
                 (CL:LET* ((sp (CL:SYMBOL-PACKAGE s))
                           (pname (CL:IF sp (CL:PACKAGE-NAME sp) \"#\"))
                           (ext (CL:IF sp
                                       (CL:MULTIPLE-VALUE-BIND (found status)
                                           (CL:FIND-SYMBOL (CL:SYMBOL-NAME s) sp)
                                         (CL:DECLARE (CL:IGNORE found))
                                         (CL:EQ status :EXTERNAL))
                                       CL:NIL)))
                   (CL:FORMAT out \"~A~C~A~C~A~C~A~C~A~C~A~C~A~C~A~%\"
                              pname #\\Tab
                              (CL:IF ext \"1\" \"0\") #\\Tab
                              (CL:SYMBOL-NAME s) #\\Tab
                              (CL:IF (CL:SPECIAL-OPERATOR-P s) \"1\" \"0\") #\\Tab
                              (CL:IF (CL:MACRO-FUNCTION s) \"1\" \"0\") #\\Tab
                              (CL:IF (CL:AND (CL:FBOUNDP s) (CL:NOT (CL:MACRO-FUNCTION s)) (CL:NOT (CL:SPECIAL-OPERATOR-P s))) \"1\" \"0\") #\\Tab
                              (CL:IF (CL:BOUNDP s) (CL:IF (CL:CONSTANTP s) \"c\" \"v\") \"0\") #\\Tab
                              (CL:IF (CL:IGNORE-ERRORS (CL:FIND-CLASS s CL:NIL)) \"1\" \"0\")))))))"
    );
    let values = runtime.eval(&code)?;
    let text = values
        .first()
        .and_then(|v| runtime.string(v))
        .unwrap_or_default();

    let mut entries = Vec::new();
    let mut seen_names = HashSet::new();
    for line in text.lines() {
        let mut p = line.split('\t');
        let (Some(pkg), Some(ext), Some(sym)) = (p.next(), p.next(), p.next()) else {
            continue;
        };
        let sep = if pkg == "#" || ext == "1" { ":" } else { "::" };
        let short_pkg = match pkg {
            "COMMON-LISP" => "cl",
            "COMMON-LISP-USER" => "cl-user",
            "KEYWORD" => "",
            other => other,
        };
        let qualified_name = if short_pkg.is_empty() {
            format!(":{}", sym.to_ascii_lowercase())
        } else {
            format!(
                "{}{}{}",
                short_pkg.to_ascii_lowercase(),
                sep,
                sym.to_ascii_lowercase()
            )
        };
        if !seen_names.insert(qualified_name.clone()) {
            continue;
        }
        let mut kind_tags = Vec::new();
        if p.next() == Some("1") {
            kind_tags.push("sp");
        }
        if p.next() == Some("1") {
            kind_tags.push("mac");
        }
        if p.next() == Some("1") {
            kind_tags.push("fn");
        }
        match p.next() {
            Some("c") => kind_tags.push("const"),
            Some("v") => kind_tags.push("var"),
            _ => {}
        }
        if p.next() == Some("1") {
            kind_tags.push("class");
        }
        entries.push(AproposEntry {
            qualified_name,
            kind_tags,
        });
    }
    entries.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
    Ok(entries)
}

pub(super) fn eval_macroexpand(
    runtime: &Runtime,
    form_text: &str,
    once: bool,
) -> Result<(String, bool), EvalError> {
    let quoted = lisp_string_literal(form_text);
    let expander = if once {
        "CL:MACROEXPAND-1"
    } else {
        "CL:MACROEXPAND"
    };
    let code = format!(
        "(CL:LET ((CL:*PRINT-PRETTY* CL:T))
           (CL:MULTIPLE-VALUE-BIND (expanded expanded-p)
               ({expander} (CL:READ-FROM-STRING {quoted}))
             (CL:VALUES (CL:WRITE-TO-STRING expanded :PRETTY CL:T)
                        (CL:IF expanded-p CL:T CL:NIL))))"
    );
    let values = runtime.eval(&code)?;
    let expanded = values
        .first()
        .and_then(|v| runtime.string(v))
        .unwrap_or_default();
    let changed = values.get(1).is_some_and(|v| !v.is_nil());
    Ok((expanded, changed))
}

pub(super) fn eval_disassemble(runtime: &Runtime, target_text: &str) -> Result<String, EvalError> {
    let trimmed = target_text.trim();
    let quoted = lisp_string_literal(trimmed);
    let code = format!(
        "(CL:WITH-OUTPUT-TO-STRING (out)
           (CL:LET* ((CL:*STANDARD-OUTPUT* out)
                     (form (CL:READ-FROM-STRING {quoted}))
                     (target (CL:IF (CL:AND (CL:CONSP form) (CL:EQ (CL:CAR form) 'CL:FUNCTION))
                                    (CL:CADR form)
                                    (CL:IF (CL:AND (CL:CONSP form) (CL:EQ (CL:CAR form) 'CL:QUOTE))
                                           (CL:CADR form)
                                           form))))
             (CL:DISASSEMBLE target)))"
    );
    let values = runtime.eval(&code)?;
    Ok(values
        .first()
        .and_then(|v| runtime.string(v))
        .unwrap_or_default())
}

pub(super) fn eval_runtime_doc(
    runtime: &Runtime,
    sym_text: &str,
) -> Result<Vec<(String, String)>, EvalError> {
    let quoted = lisp_string_literal(
        sym_text
            .trim()
            .trim_start_matches("#'")
            .trim_start_matches('\''),
    );
    let code = format!(
        "(CL:WITH-OUTPUT-TO-STRING (out)
           (CL:LET ((sym (CL:READ-FROM-STRING {quoted})))
             (CL:WHEN (CL:SYMBOLP sym)
               (CL:DOLIST (kind '(CL:FUNCTION CL:VARIABLE CL:TYPE CL:STRUCTURE CL:SETF))
                 (CL:LET ((doc (CL:IGNORE-ERRORS (CL:DOCUMENTATION sym kind))))
                   (CL:WHEN doc
                     (CL:FORMAT out \"~A~C~A~%\" kind #\\Tab (CL:SUBSTITUTE #\\Space #\\Newline doc))))))))"
    );
    let values = runtime.eval(&code)?;
    let text = values
        .first()
        .and_then(|v| runtime.string(v))
        .unwrap_or_default();
    let mut docs = Vec::new();
    for line in text.lines() {
        if let Some((kind, doc)) = line.split_once('\t') {
            docs.push((kind.to_owned(), doc.to_owned()));
        }
    }
    Ok(docs)
}

pub(super) fn eval_runtime_types(
    runtime: &Runtime,
    form_text: &str,
) -> Result<Vec<(String, String)>, EvalError> {
    let quoted = lisp_string_literal(form_text);
    let code = format!(
        "(CL:LET ((vals (CL:MULTIPLE-VALUE-LIST (CL:EVAL (CL:READ-FROM-STRING {quoted})))))
           (CL:WITH-OUTPUT-TO-STRING (out)
             (CL:DOLIST (v vals)
               (CL:FORMAT out \"~S~C~S~%\" v #\\Tab (CL:TYPE-OF v)))))"
    );
    let values = runtime.eval(&code)?;
    let text = values
        .first()
        .and_then(|v| runtime.string(v))
        .unwrap_or_default();
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some((val, ty)) = line.split_once('\t') {
            out.push((val.to_owned(), ty.to_owned()));
        }
    }
    Ok(out)
}

pub(super) fn eval_trace_op(
    runtime: &Runtime,
    trace: bool,
    names: &str,
) -> Result<String, EvalError> {
    let op = if trace { "CL:TRACE" } else { "CL:UNTRACE" };
    let code = format!("({op} {names})");
    let values = runtime.eval(&code)?;
    let mut rendered = Vec::new();
    for v in &values {
        rendered.push(runtime.repr(v)?);
    }
    Ok(rendered.join(" "))
}
