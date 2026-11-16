//! Built-in package metadata for `ZK-TREELIST` used by navigation and linting.

use crate::cl_builtins::BuiltinKind;

pub const PACKAGE: &str = "ZK-TREELIST";

#[derive(Debug, Clone, Copy)]
pub struct TreelistCallable {
    pub name: &'static str,
    pub kind: BuiltinKind,
    pub evaluates_arguments: bool,
    pub params: &'static [&'static str],
    pub lambda_list: &'static str,
    pub documentation: &'static str,
}

macro_rules! callables {
    ($($name:literal ($kind:ident, $eval:expr) => [$($param:literal),*] ($display:literal) : $doc:literal;)*) => {
        pub const CALLABLES: &[TreelistCallable] = &[
            $(TreelistCallable {
                name: $name,
                kind: BuiltinKind::$kind,
                evaluates_arguments: $eval,
                params: &[$($param),*],
                lambda_list: $display,
                documentation: $doc,
            }),*
        ];
    };
}

callables! {
    "TREELIST" (Function, true) => ["&rest", "elements"] ("&rest elements") :
        "Create an immutable persistent treelist containing ELEMENTS.";
    "MUTABLE-TREELIST" (Function, true) => ["&rest", "elements"] ("&rest elements") :
        "Create a mutable transient treelist containing ELEMENTS.";
    "MAKE" (Function, true) => ["count", "&key", "initial-element", "mutable"] ("count &key initial-element mutable") :
        "Create a treelist (or mutable-treelist when :MUTABLE is true) of length COUNT filled with INITIAL-ELEMENT.";
    "EMPTY" (Function, true) => [] ("") :
        "Return an empty immutable treelist.";
    "TREELIST-P" (Function, true) => ["object"] ("object") :
        "Return T if OBJECT is an immutable treelist.";
    "MUTABLE-TREELIST-P" (Function, true) => ["object"] ("object") :
        "Return T if OBJECT is a mutable treelist.";
    "EMPTY-P" (Function, true) => ["seq"] ("seq") :
        "Return T if treelist or mutable-treelist SEQ is empty.";
    "LENGTH" (Function, true) => ["seq"] ("seq") :
        "Return the number of elements in treelist or mutable-treelist SEQ in O(1) time.";
    "REF" (Function, true) => ["seq", "index"] ("seq index") :
        "Return the element at 0-based INDEX of treelist or mutable-treelist SEQ.";
    "FIRST" (Function, true) => ["seq"] ("seq") :
        "Return the first element of non-empty treelist or mutable-treelist SEQ.";
    "LAST" (Function, true) => ["seq"] ("seq") :
        "Return the last element of non-empty treelist or mutable-treelist SEQ.";
    "SET" (Function, true) => ["tl", "index", "value"] ("tl index value") :
        "Return a new persistent treelist with INDEX replaced by VALUE.";
    "PUSH-FRONT" (Function, true) => ["tl", "value"] ("tl value") :
        "Return a new persistent treelist with VALUE prepended at the front.";
    "PUSH-BACK" (Function, true) => ["tl", "value"] ("tl value") :
        "Return a new persistent treelist with VALUE appended at the back.";
    "POP-FRONT" (Function, true) => ["tl"] ("tl") :
        "Return two values: the first element of TL and the remaining persistent treelist.";
    "POP-BACK" (Function, true) => ["tl"] ("tl") :
        "Return two values: the last element of TL and the remaining persistent treelist.";
    "INSERT" (Function, true) => ["tl", "index", "value"] ("tl index value") :
        "Return a new persistent treelist with VALUE inserted at INDEX.";
    "DELETE" (Function, true) => ["tl", "index"] ("tl index") :
        "Return a new persistent treelist without the element at INDEX.";
    "APPEND" (Function, true) => ["&rest", "treelists"] ("&rest treelists") :
        "Concatenate persistent treelists into a new treelist.";
    "SUBSEQ" (Function, true) => ["tl", "start", "&optional", "end"] ("tl start &optional end") :
        "Return the persistent sub-treelist in [START, END) sharing tree structure.";
    "SPLIT" (Function, true) => ["tl", "index"] ("tl index") :
        "Split persistent TL at INDEX into (values left right).";
    "SET!" (Function, true) => ["mtl", "index", "value"] ("mtl index value") :
        "Replace the element at INDEX in MUTABLE-TREELIST MTL with VALUE and return VALUE.";
    "PUSH-FRONT!" (Function, true) => ["mtl", "value"] ("mtl value") :
        "Prepend VALUE to MUTABLE-TREELIST MTL in place and return MTL.";
    "PUSH-BACK!" (Function, true) => ["mtl", "value"] ("mtl value") :
        "Append VALUE to MUTABLE-TREELIST MTL in place and return MTL.";
    "POP-FRONT!" (Function, true) => ["mtl"] ("mtl") :
        "Remove and return the first element of MUTABLE-TREELIST MTL.";
    "POP-BACK!" (Function, true) => ["mtl"] ("mtl") :
        "Remove and return the last element of MUTABLE-TREELIST MTL.";
    "INSERT!" (Function, true) => ["mtl", "index", "value"] ("mtl index value") :
        "Insert VALUE at INDEX in MUTABLE-TREELIST MTL in place and return MTL.";
    "DELETE!" (Function, true) => ["mtl", "index"] ("mtl index") :
        "Delete the element at INDEX from MUTABLE-TREELIST MTL in place and return MTL.";
    "APPEND!" (Function, true) => ["mtl", "other"] ("mtl other") :
        "Append treelist or mutable-treelist OTHER onto MTL in place and return MTL.";
    "SPLIT!" (Function, true) => ["mtl", "index"] ("mtl index") :
        "Truncate MTL to [0, INDEX) in place and return [INDEX, LEN) as a new MUTABLE-TREELIST.";
    "CLEAR!" (Function, true) => ["mtl"] ("mtl") :
        "Remove all elements from MUTABLE-TREELIST MTL in place and return MTL.";
    "COPY" (Function, true) => ["mtl"] ("mtl") :
        "Return an independent O(1) copy-on-write MUTABLE-TREELIST handle sharing MTL's state.";
    "THAW" (Function, true) => ["tl"] ("tl") :
        "Create an O(1) transient MUTABLE-TREELIST from persistent TL.";
    "SNAPSHOT" (Function, true) => ["mtl"] ("mtl") :
        "Return an O(1) immutable TREELIST snapshot of MTL without invalidating MTL.";
    "FREEZE" (Function, true) => ["mtl"] ("mtl") :
        "Return an O(1) immutable TREELIST from MTL and clear MTL.";
    "WITH-TRANSIENT" (Macro, false) => ["binding", "&body", "body"] ("(var &optional initial) &body body") :
        "Bind VAR to a fresh transient MUTABLE-TREELIST (thawing INITIAL if supplied), run BODY, and freeze into a TREELIST.";
    "DO-ELEMENTS" (Macro, false) => ["binding", "&body", "body"] ("(var seq &optional result) &body body") :
        "Iterate VAR across every element of treelist or mutable-treelist SEQ and return RESULT.";
    "FROM-SEQUENCE" (Function, true) => ["seq"] ("seq") :
        "Convert any CL:SEQUENCE into a persistent TREELIST.";
    "TO-LIST" (Function, true) => ["seq"] ("seq") :
        "Convert a treelist or mutable-treelist into a proper list.";
    "TO-VECTOR" (Function, true) => ["seq", "&key", "element-type"] ("seq &key (element-type t)") :
        "Convert a treelist or mutable-treelist into a 1D vector.";
    "EQUAL-P" (Function, true) => ["a", "b", "&key", "test"] ("a b &key (test #'eql)") :
        "Return T if treelist sequences A and B have equal length and pairwise satisfy TEST.";
}

#[must_use]
pub fn callable(name: &str) -> Option<&'static TreelistCallable> {
    CALLABLES.iter().find(|entry| entry.name == name)
}

#[must_use]
pub fn type_documentation(name: &str) -> Option<&'static str> {
    match name {
        "TREELIST" => Some("Immutable persistent Sek sequence (subclass of CL:SEQUENCE)."),
        "MUTABLE-TREELIST" => Some("Transient mutable Sek sequence (subclass of CL:SEQUENCE)."),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::{LintConfig, SourceWorkspace};

    #[test]
    fn treelist_macros_and_patterns_lint_cleanly() {
        let mut ws = SourceWorkspace::new();
        ws.set_document(
            "test.lisp",
            r#"(in-package #:cl-user)
(defun build-and-sum (xs)
  (let ((tl (zk-treelist:with-transient (acc (zk:treelist 1 2))
              (zk-treelist:push-back! acc xs)))
        (total 0))
    (zk-treelist:do-elements (item tl total)
      (incf total item))
    (zk:match tl
      ((zk:treelist a (:rest mid) b) (list a (zk-treelist:length mid) b))
      (_ total))))
"#,
        );
        let diags = ws.lint_document("test.lisp", &LintConfig::default());
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }
}
