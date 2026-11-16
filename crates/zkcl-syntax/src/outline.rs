use rowan::{GreenNodeData, Language, NodeOrToken};

use crate::reader_tokens::SymbolName;
use crate::syntax::{LispLanguage, SyntaxKind, SyntaxNode, TextRange, TextSize, list_has_dot};

/// The declaration syntax recognized by the document outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineKind {
    Function,
    Macro,
    Variable,
    Constant,
    Package,
    System,
    Type,
}

/// A syntactic declaration in one document snapshot.
#[derive(Debug, PartialEq, Eq)]
pub struct OutlineEntry {
    /// Source spelling of the name, including escapes or package markers.
    pub name: String,
    pub kind: OutlineKind,
    /// UTF-8 byte range of the definition, excluding adjacent trivia.
    pub range: TextRange,
    /// UTF-8 byte range of the name within the definition.
    pub selection_range: TextRange,
}

pub(crate) use crate::ast::{Element, Token};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReaderStateEffect {
    None,
    Package,
    ReaderSyntax,
}

pub(crate) fn is_readtable_mutating_operator(name: &str, include_in_readtable: bool) -> bool {
    matches!(
        name,
        "SET-MACRO-CHARACTER"
            | "SET-DISPATCH-MACRO-CHARACTER"
            | "MAKE-DISPATCH-MACRO-CHARACTER"
            | "SET-SYNTAX-FROM-CHAR"
            | "IN-SYNTAX"
    ) || (include_in_readtable && name == "IN-READTABLE")
}

pub(crate) fn is_readtable_place(place: &Element) -> bool {
    match place {
        Element::Token(token) if token.kind() == SyntaxKind::Atom => {
            SymbolName::with_standard_operator(token.text(), |op| {
                matches!(op, "*READTABLE*" | "*READ-BASE*" | "*READ-SUPPRESS*")
            })
            .unwrap_or(false)
        }
        Element::Node(node) if node.kind() == SyntaxKind::List => {
            with_list_operator(node, |op| op == "READTABLE-CASE").unwrap_or(false)
        }
        _ => false,
    }
}

pub(crate) fn reader_state_effect(form: &Element) -> ReaderStateEffect {
    let Some(node) = form.as_node() else {
        return ReaderStateEffect::None;
    };
    if let Some(effect) = with_list_operator(node, |op| {
        if is_readtable_mutating_operator(op, false) {
            return Some(ReaderStateEffect::ReaderSyntax);
        }
        match op {
            "IN-PACKAGE" => Some(ReaderStateEffect::Package),
            "SETQ" | "SETF" => {
                let mut args = elements(node).skip(1);
                let mut effect = ReaderStateEffect::None;
                while let (Some(place), Some(value)) = (args.next(), args.next()) {
                    let place_effect = if is_readtable_place(&place) {
                        ReaderStateEffect::ReaderSyntax
                    } else if matches!(&place, Element::Token(t) if t.kind() == SyntaxKind::Atom && SymbolName::is_standard_operator_ci(t.text(), "*PACKAGE*"))
                    {
                        ReaderStateEffect::Package
                    } else {
                        ReaderStateEffect::None
                    };
                    effect = effect.max(place_effect);
                    if effect == ReaderStateEffect::ReaderSyntax {
                        break;
                    }
                    effect = effect.max(reader_state_effect(&value));
                    if effect == ReaderStateEffect::ReaderSyntax {
                        break;
                    }
                }
                Some(effect)
            }
            "PROGN" | "LOCALLY" | "IF" | "WHEN" | "UNLESS" => {
                Some(combined_reader_state_effect(elements(node).skip(1)))
            }
            "EVAL-WHEN" if eval_when_allows_top_level_outline(node) => {
                Some(combined_reader_state_effect(elements(node).skip(2)))
            }
            _ => None,
        }
    })
    .flatten()
    {
        return effect;
    }
    if node.kind() == SyntaxKind::ReaderForm
        && node
            .first_token()
            .is_some_and(|first| first.kind() == SyntaxKind::ReaderConditional)
    {
        combined_reader_state_effect(elements(node).skip(2))
    } else {
        ReaderStateEffect::None
    }
}

fn combined_reader_state_effect(forms: impl Iterator<Item = Element>) -> ReaderStateEffect {
    let mut effect = ReaderStateEffect::None;
    for form in forms {
        effect = effect.max(reader_state_effect(&form));
        if effect == ReaderStateEffect::ReaderSyntax {
            break;
        }
    }
    effect
}

pub(crate) fn collect_with_features(
    root: &SyntaxNode,
    known_features: Option<&[String]>,
) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    collect_forms(root, known_features, &mut entries);
    entries
}

// A reader macro with an unknown boundary may consume following forms.
// Propagate that boundary through a top-level PROGN.
fn collect_forms(
    container: &SyntaxNode,
    known_features: Option<&[String]>,
    entries: &mut Vec<OutlineEntry>,
) -> bool {
    for form in elements(container) {
        if !collect_one(form, known_features, entries) {
            return false;
        }
    }
    true
}

fn collect_one(
    form: Element,
    known_features: Option<&[String]>,
    entries: &mut Vec<OutlineEntry>,
) -> bool {
    if known_features.is_some()
        && let Some(node) = form.as_node()
        && let Some(conditional) = crate::ast::FeatureConditional::from_node(node)
        && let Some(active) = conditional.is_active(known_features)
    {
        return !active || collect_one(conditional.guarded.into_element(), known_features, entries);
    }
    if let Element::Node(list) = &form
        && list.kind() == SyntaxKind::List
        && !list_has_dot(list)
        && let Some(skip) = with_list_operator(list, |operator| match operator {
            "PROGN" | "LOCALLY" => Some(1),
            "EVAL-WHEN" if eval_when_allows_top_level_outline(list) => Some(2),
            _ => None,
        })
        .flatten()
    {
        for child in elements(list).skip(skip) {
            if !collect_one(child, known_features, entries) {
                return false;
            }
        }
        return true;
    }
    if reader_boundary(&form) || reader_state_effect(&form) == ReaderStateEffect::ReaderSyntax {
        return false;
    }
    if let Element::Node(list) = form
        && let Some(entry) = declaration(&list)
    {
        entries.push(entry);
    }
    true
}

/// Returns `true` if `(eval-when (...) ...)` has a well-formed non-empty situation list
/// containing at least one valid compile/load/execute situation.
pub(crate) fn eval_when_allows_top_level_outline(list: &SyntaxNode) -> bool {
    let Some(situations) = elements(list).nth(1).and_then(Element::into_node) else {
        return false;
    };
    if situations.kind() != SyntaxKind::List
        || list_has_dot(&situations)
        || !situations
            .last_child_or_token()
            .is_some_and(|e| e.kind() == SyntaxKind::CloseParen)
    {
        return false;
    }
    let mut has_active = false;
    for item in elements(&situations) {
        let Some(tok) = item.into_token() else {
            return false;
        };
        if tok.kind() != SyntaxKind::Atom {
            return false;
        }
        let Some(sym) = SymbolName::parse(tok.text()) else {
            return false;
        };
        if is_valid_eval_when_situation(&sym) {
            has_active = true;
        } else {
            return false;
        }
    }
    has_active
}

pub(crate) fn is_valid_eval_when_situation(sym: &SymbolName) -> bool {
    match &sym.prefix {
        crate::reader_tokens::Prefix::Keyword => matches!(
            sym.name.as_str(),
            "COMPILE-TOPLEVEL" | "LOAD-TOPLEVEL" | "EXECUTE"
        ),
        crate::reader_tokens::Prefix::None => {
            matches!(sym.name.as_str(), "COMPILE" | "LOAD" | "EVAL")
        }
        crate::reader_tokens::Prefix::Package { name, .. }
            if crate::reader_tokens::canonical_package(name) == "COMMON-LISP" =>
        {
            matches!(sym.name.as_str(), "COMPILE" | "LOAD" | "EVAL")
        }
        _ => false,
    }
}

pub(crate) fn elements(node: &SyntaxNode) -> FormElements<'_> {
    FormElements {
        parent: node,
        children: node.children_with_tokens(),
    }
}

#[derive(Clone)]
pub(crate) struct FormElements<'a> {
    parent: &'a SyntaxNode,
    children: rowan::SyntaxElementChildren<LispLanguage>,
}

impl Iterator for FormElements<'_> {
    type Item = Element;

    fn next(&mut self) -> Option<Self::Item> {
        self.children.find(|child| is_element_kind(child.kind()))
    }
}

impl FormElements<'_> {
    /// Finds remaining elements at `offset` in source order. At a shared
    /// boundary, both elements are included for inclusive position queries.
    pub(crate) fn at_offset(mut self, offset: TextSize) -> impl Iterator<Item = Element> {
        let first_index = self.next().map_or(usize::MAX, |element| element.index());
        elements_at_offset(self.parent, first_index, offset)
    }
}

/// Find children at `offset` starting with the given child index.
pub(crate) fn elements_at_offset(
    parent: &SyntaxNode,
    first_index: usize,
    offset: TextSize,
) -> impl Iterator<Item = Element> {
    let range = parent.text_range();
    let right = range
        .contains(offset)
        .then(|| parent.child_or_token_at_range(TextRange::at(offset, 1.into())))
        .flatten();
    let left = if range.start() < offset
        && (range.end() == offset
            || right
                .as_ref()
                .is_some_and(|element| element.text_range().start() == offset))
    {
        parent.child_or_token_at_range(TextRange::new(offset - TextSize::from(1), offset))
    } else {
        None
    };
    left.into_iter()
        .chain(right)
        .filter(move |element| element.index() >= first_index && is_element_kind(element.kind()))
}

#[cfg(test)]
#[test]
fn offset_lookup_matches_linear_scan_after_skipping_operands() {
    let text = "(op (left)(right)|é| ; comment\n end)";
    let parsed = crate::syntax::parse_source(text);
    let list = parsed.root().first_child().unwrap();
    for skip in 0..=6 {
        let mut remaining = elements(&list);
        for _ in 0..skip {
            remaining.next();
        }
        for byte in 0..=text.len() as u32 {
            let offset = TextSize::from(byte);
            let expected = remaining
                .clone()
                .filter(|element| element.text_range().contains_inclusive(offset))
                .collect::<Vec<_>>();
            let actual = remaining.clone().at_offset(offset).collect::<Vec<_>>();
            assert_eq!(actual, expected, "byte {byte}, skipped {skip}");
        }
    }
}

pub(crate) fn is_element_kind(kind: SyntaxKind) -> bool {
    !kind.is_trivia() && !matches!(kind, SyntaxKind::OpenParen | SyntaxKind::CloseParen)
}

pub(crate) fn with_list_operator<R>(list: &SyntaxNode, f: impl FnOnce(&str) -> R) -> Option<R> {
    if list.kind() != SyntaxKind::List {
        return None;
    }
    let token = list
        .green()
        .children()
        .find(|child| is_element_kind(LispLanguage::kind_from_raw(child.kind())))?
        .into_token()?;
    if token.kind() != LispLanguage::kind_to_raw(SyntaxKind::Atom) {
        return None;
    }
    SymbolName::with_standard_operator(token.text(), f)
}

pub(crate) fn list_operator(list: &SyntaxNode) -> Option<String> {
    with_list_operator(list, str::to_owned)
}

pub(crate) fn is_nil_element(element: &Element) -> bool {
    match element {
        Element::Token(token) if token.kind() == SyntaxKind::Atom => {
            SymbolName::is_standard_operator_ci(token.text(), "NIL")
        }
        Element::Node(node) => node.kind() == SyntaxKind::List && elements(node).next().is_none(),
        _ => false,
    }
}

/// Returns `true` when `element` is a `DEFMETHOD` qualifier token before the specialized lambda list.
pub(crate) fn is_defmethod_qualifier(element: &Element) -> bool {
    element.as_token().is_some() && !is_nil_element(element)
}

/// Given the operands of `(DEFMETHOD name qualifiers... lambda-list body...)` starting at `name`,
/// returns the index of `lambda-list` (whether a non-token form or `nil`/`()`).
pub(crate) fn defmethod_lambda_list_index(args: &[Element]) -> Option<usize> {
    if args.is_empty() {
        return None;
    }
    (1..args.len()).find(|&idx| !is_defmethod_qualifier(&args[idx]))
}

pub(crate) fn reader_boundary(form: &Element) -> bool {
    has_reader_boundary(form, true)
}

pub(crate) fn unknown_boundary(form: &Element) -> bool {
    has_reader_boundary(form, false)
}

fn has_reader_boundary(form: &Element, include_read_eval: bool) -> bool {
    match form {
        Element::Token(token) => token.kind() == SyntaxKind::UnknownDispatch,
        Element::Node(node) => {
            contains_reader_dispatch(node.green(), include_read_eval)
                || (node.kind() == SyntaxKind::ReaderForm && conditional_operand(node))
        }
    }
}

fn contains_reader_dispatch(node: &GreenNodeData, include_read_eval: bool) -> bool {
    // This check only needs token kinds and spelling. Borrow the parser's
    // bounded green tree directly to avoid allocating positional cursors.
    node.children().any(|child| match child {
        NodeOrToken::Node(node) => contains_reader_dispatch(node, include_read_eval),
        NodeOrToken::Token(token) => {
            let kind = token.kind();
            kind == LispLanguage::kind_to_raw(SyntaxKind::UnknownDispatch)
                || (include_read_eval
                    && kind == LispLanguage::kind_to_raw(SyntaxKind::DispatchPrefix)
                    && token.text().ends_with('.'))
        }
    })
}

fn conditional_operand(reader_form: &SyntaxNode) -> bool {
    reader_form
        .children()
        .filter(|child| child.kind() == SyntaxKind::ReaderForm)
        .any(|child| {
            child
                .first_token()
                .is_some_and(|token| token.kind() == SyntaxKind::ReaderConditional)
                || conditional_operand(&child)
        })
}

pub(crate) fn declaration(list: &SyntaxNode) -> Option<OutlineEntry> {
    if list.kind() != SyntaxKind::List
        || !list
            .last_child_or_token()
            .is_some_and(|element| element.kind() == SyntaxKind::CloseParen)
        || list_has_dot(list)
    {
        return None;
    }
    let mut forms = elements(list);
    let head = forms.next()?.into_token()?;
    if head.kind() != SyntaxKind::Atom {
        return None;
    }
    let head = SymbolName::parse(head.text())?;
    let zk_operator = head.zk_definition();
    let operator = zk_operator
        .or_else(|| head.standard_operator())
        .or_else(|| {
            let is_asdf_or_unqualified = match &head.prefix {
                crate::reader_tokens::Prefix::None => true,
                crate::reader_tokens::Prefix::Package { name, .. } => name == "ASDF",
                _ => false,
            };
            (is_asdf_or_unqualified && head.name == "DEFSYSTEM").then_some("DEFSYSTEM")
        })?;
    let kind = match operator {
        "DEFVARIANT" if zk_operator.is_some() => OutlineKind::Type,
        "DEFPATTERN" if zk_operator.is_some() => OutlineKind::Macro,
        "DEFUN" | "DEFGENERIC" | "DEFMETHOD" => OutlineKind::Function,
        "DEFMACRO" | "DEFINE-COMPILER-MACRO" | "DEFINE-SYMBOL-MACRO" => OutlineKind::Macro,
        "DEFVAR" | "DEFPARAMETER" => OutlineKind::Variable,
        "DEFCONSTANT" => OutlineKind::Constant,
        "DEFPACKAGE" => OutlineKind::Package,
        "DEFSYSTEM" => OutlineKind::System,
        "DEFCLASS" | "DEFINE-CONDITION" | "DEFSTRUCT" | "DEFTYPE" => OutlineKind::Type,
        _ => return None,
    };
    let name = forms.next()?;
    let (name, selection_range) = match name {
        Element::Token(token)
            if name_token(&token)
                || (matches!(kind, OutlineKind::Package | OutlineKind::System)
                    && matches!(token.kind(), SyntaxKind::String | SyntaxKind::RawString)) =>
        {
            (token.text().to_owned(), token.text_range())
        }
        Element::Node(name) if name.kind() == SyntaxKind::List => {
            let mut parts = elements(&name);
            let first = parts.next()?.into_token()?;
            if matches!(operator, "DEFSTRUCT" | "DEFVARIANT") && name_token(&first) {
                (first.text().to_owned(), first.text_range())
            } else if kind == OutlineKind::Function
                && first.kind() == SyntaxKind::Atom
                && SymbolName::is_standard_operator_ci(first.text(), "SETF")
                && parts
                    .next()
                    .and_then(Element::into_token)
                    .is_some_and(|token| name_token(&token))
                && parts.next().is_none()
            {
                (name.text().to_string(), name.text_range())
            } else {
                return None;
            }
        }
        _ => return None,
    };
    Some(OutlineEntry {
        name,
        kind,
        range: list.text_range(),
        selection_range,
    })
}

fn name_token(token: &Token) -> bool {
    // Atom tokens also cover implementation-dependent numeric syntax. This
    // structural query leaves token interpretation to the reader.
    token.kind() == SyntaxKind::Atom
        && (!token.text().starts_with('#') || token.text().starts_with("#:"))
}
