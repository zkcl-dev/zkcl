//! Typed syntax tree wrappers over Rowan `SyntaxNode` and `SyntaxElement`.

use rowan::{Direction, SyntaxElement, SyntaxToken, TokenAtOffset};

use crate::reader_tokens::{Prefix, SymbolName, decode_character_token, string_literal};
use crate::syntax::lexer::lex;
use crate::syntax::{
    LispLanguage, SourceReadtable, SyntaxDiagnosticKind, SyntaxKind, SyntaxNode, TextRange,
    TextSize,
};

pub type Element = SyntaxElement<LispLanguage>;
pub type Token = SyntaxToken<LispLanguage>;

/// A typed view of a Lisp syntax element (non-trivia node or token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AstForm {
    List(AstList),
    Vector(AstVector),
    ReaderForm(AstReaderForm),
    Atom(AstAtom),
    Literal(AstLiteral),
    Dot(Token),
    Unknown(Element),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstList(SyntaxNode);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstVector(SyntaxNode);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstReaderForm(SyntaxNode);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstAtom(Token);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AstLiteral(Token);

/// The feature test and guarded form of a source `#+` or `#-` expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureConditional {
    pub feature: AstForm,
    pub guarded: AstForm,
    /// `true` for `#+`, which reads its guarded form when the test succeeds.
    pub when_present: bool,
}

impl FeatureConditional {
    /// Whether the guarded form is read given the known feature names.
    /// Returns `None` when the features or the test's meaning are unknown.
    pub fn is_active(&self, known_features: Option<&[String]>) -> Option<bool> {
        eval_feature_expr(&self.feature, known_features).map(|enabled| enabled == self.when_present)
    }

    pub(crate) fn from_node(node: &SyntaxNode) -> Option<Self> {
        if node.kind() != SyntaxKind::ReaderForm {
            return None;
        }
        let mut parts = node.children_with_tokens();
        let prefix = parts.next()?.into_token()?;
        if prefix.kind() != SyntaxKind::ReaderConditional {
            return None;
        }
        let mut operands = parts.filter_map(AstForm::cast);
        Some(Self {
            feature: operands.next()?,
            guarded: operands.next()?,
            when_present: prefix.text().ends_with('+'),
        })
    }
}

impl AstForm {
    pub fn cast(element: Element) -> Option<Self> {
        match element {
            Element::Node(node) => match node.kind() {
                SyntaxKind::List => Some(Self::List(AstList(node))),
                SyntaxKind::Vector => Some(Self::Vector(AstVector(node))),
                SyntaxKind::ReaderForm => Some(Self::ReaderForm(AstReaderForm(node))),
                _ => Some(Self::Unknown(Element::Node(node))),
            },
            Element::Token(token) => match token.kind() {
                SyntaxKind::Atom => Some(Self::Atom(AstAtom(token))),
                SyntaxKind::String | SyntaxKind::RawString | SyntaxKind::Character => {
                    Some(Self::Literal(AstLiteral(token)))
                }
                SyntaxKind::Dot => Some(Self::Dot(token)),
                SyntaxKind::OpenParen
                | SyntaxKind::CloseParen
                | SyntaxKind::VectorStart
                | SyntaxKind::Whitespace
                | SyntaxKind::LineComment
                | SyntaxKind::BlockComment => None,
                _ => Some(Self::Unknown(Element::Token(token))),
            },
        }
    }

    pub fn text_range(&self) -> TextRange {
        match self {
            Self::List(list) => list.0.text_range(),
            Self::Vector(vec) => vec.0.text_range(),
            Self::ReaderForm(rf) => rf.0.text_range(),
            Self::Atom(atom) => atom.0.text_range(),
            Self::Literal(lit) => lit.0.text_range(),
            Self::Dot(tok) => tok.text_range(),
            Self::Unknown(el) => el.text_range(),
        }
    }

    pub fn into_element(self) -> Element {
        match self {
            Self::List(list) => Element::Node(list.0),
            Self::Vector(vec) => Element::Node(vec.0),
            Self::ReaderForm(rf) => Element::Node(rf.0),
            Self::Atom(atom) => Element::Token(atom.0),
            Self::Literal(lit) => Element::Token(lit.0),
            Self::Dot(tok) => Element::Token(tok),
            Self::Unknown(el) => el,
        }
    }

    pub fn as_list(&self) -> Option<&AstList> {
        match self {
            Self::List(list) => Some(list),
            _ => None,
        }
    }

    pub fn as_vector(&self) -> Option<&AstVector> {
        match self {
            Self::Vector(vector) => Some(vector),
            _ => None,
        }
    }

    pub fn as_atom(&self) -> Option<&AstAtom> {
        match self {
            Self::Atom(atom) => Some(atom),
            _ => None,
        }
    }

    pub fn as_literal(&self) -> Option<&AstLiteral> {
        match self {
            Self::Literal(literal) => Some(literal),
            _ => None,
        }
    }

    pub fn as_reader_form(&self) -> Option<&AstReaderForm> {
        match self {
            Self::ReaderForm(rf) => Some(rf),
            _ => None,
        }
    }

    pub fn symbol(&self) -> Option<SymbolName> {
        self.as_atom()?.symbol()
    }

    pub fn designator(&self) -> Option<String> {
        match self {
            Self::Atom(atom) => Some(atom.symbol()?.name),
            Self::Literal(lit) => match lit.0.kind() {
                SyntaxKind::String | SyntaxKind::RawString => string_literal(&lit.0),
                SyntaxKind::Character => {
                    lit.character_value().map(|character| character.to_string())
                }
                _ => None,
            },
            _ => None,
        }
    }
}

impl AstList {
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::List).then_some(Self(node))
    }

    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    pub fn is_closed(&self) -> bool {
        closing_paren(&self.0).is_some()
    }

    pub fn is_dotted(&self) -> bool {
        self.dot().is_some()
    }

    /// The opening and closing tokens of a complete list.
    pub fn delimiters(&self) -> Option<(Token, Token)> {
        group_delimiters(&self.0, SyntaxKind::OpenParen)
    }

    /// The first consing dot directly inside the list, if present.
    pub fn dot(&self) -> Option<Token> {
        self.0
            .children_with_tokens()
            .filter_map(Element::into_token)
            .find(|token| token.kind() == SyntaxKind::Dot)
    }

    /// The first form after the consing dot in source order. Reader conditionals
    /// are preserved. Consult parse diagnostics for malformed lists.
    pub fn dotted_tail(&self) -> Option<AstForm> {
        let tail = self
            .dot()?
            .siblings_with_tokens(Direction::Next)
            .skip(1)
            .find_map(AstForm::cast)?;
        (!matches!(tail, AstForm::Dot(_))).then_some(tail)
    }

    pub fn is_proper_closed(&self) -> bool {
        self.is_closed() && !self.is_dotted()
    }

    pub fn forms(&self) -> impl Iterator<Item = AstForm> + '_ {
        self.0.children_with_tokens().filter_map(AstForm::cast)
    }

    pub fn head(&self) -> Option<AstForm> {
        self.forms().next()
    }

    pub fn head_symbol(&self) -> Option<(AstAtom, SymbolName)> {
        let AstForm::Atom(atom) = self.head()? else {
            return None;
        };
        let sym = atom.symbol()?;
        Some((atom, sym))
    }

    pub fn standard_operator(&self) -> Option<String> {
        let (_, sym) = self.head_symbol()?;
        sym.standard_operator().map(str::to_owned)
    }
}

impl AstVector {
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::Vector).then_some(Self(node))
    }

    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    pub fn is_closed(&self) -> bool {
        closing_paren(&self.0).is_some()
    }

    /// The dispatch opener and closing token of a complete vector.
    pub fn delimiters(&self) -> Option<(Token, Token)> {
        group_delimiters(&self.0, SyntaxKind::VectorStart)
    }

    pub fn forms(&self) -> impl Iterator<Item = AstForm> + '_ {
        self.0.children_with_tokens().filter_map(AstForm::cast)
    }
}

impl AstReaderForm {
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        (node.kind() == SyntaxKind::ReaderForm).then_some(Self(node))
    }

    pub fn syntax(&self) -> &SyntaxNode {
        &self.0
    }

    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    pub fn prefix_token(&self) -> Option<Token> {
        self.0.first_token()
    }

    pub fn prefix_kind(&self) -> Option<SyntaxKind> {
        Some(self.prefix_token()?.kind())
    }

    /// Operands after the prefix token.
    pub fn operands(&self) -> impl Iterator<Item = AstForm> + '_ {
        self.0
            .children_with_tokens()
            .skip(1)
            .filter_map(AstForm::cast)
    }

    /// The operands of a feature conditional with both source forms present.
    pub fn feature_conditional(&self) -> Option<FeatureConditional> {
        FeatureConditional::from_node(&self.0)
    }
}

impl AstAtom {
    pub fn token(&self) -> &Token {
        &self.0
    }

    pub fn text(&self) -> &str {
        self.0.text()
    }

    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    pub fn symbol(&self) -> Option<SymbolName> {
        SymbolName::parse(self.0.text())
    }

    pub fn is_keyword(&self) -> bool {
        self.symbol()
            .is_some_and(|s| matches!(s.prefix, Prefix::Keyword))
    }
}

impl AstLiteral {
    pub fn token(&self) -> &Token {
        &self.0
    }

    pub fn text(&self) -> &str {
        self.0.text()
    }

    pub fn text_range(&self) -> TextRange {
        self.0.text_range()
    }

    pub fn string_value(&self) -> Option<String> {
        string_literal(&self.0)
    }

    pub fn character_value(&self) -> Option<char> {
        (self.0.kind() == SyntaxKind::Character).then(|| decode_character_token(self.0.text()))?
    }
}

/// Find the tokens touching a byte offset, including trivia and delimiters.
/// At a token boundary the result contains the left and right tokens in order.
pub fn tokens_at_offset(root: &SyntaxNode, offset: TextSize) -> TokenAtOffset<Token> {
    let range = root.text_range();
    if range.is_empty() || !range.contains_inclusive(offset) {
        return TokenAtOffset::None;
    }
    let token_at_byte = |byte| {
        root.covering_element(TextRange::at(byte, 1.into()))
            .into_token()
            .expect("a byte inside a syntax tree belongs to one token")
    };
    if offset == range.end() {
        return TokenAtOffset::Single(token_at_byte(offset - TextSize::from(1)));
    }
    let right = token_at_byte(offset);
    if offset > range.start() && right.text_range().start() == offset {
        TokenAtOffset::Between(token_at_byte(offset - TextSize::from(1)), right)
    } else {
        TokenAtOffset::Single(right)
    }
}

/// Find the non-trivia token at `offset`, preferring an atom at a boundary.
pub fn token_at_offset(root: &SyntaxNode, offset: TextSize) -> Option<Token> {
    let mut best: Option<Token> = None;
    for token in tokens_at_offset(root, offset) {
        if matches!(token.kind(), SyntaxKind::Atom) {
            return Some(token);
        }
        if !token.kind().is_trivia()
            && !matches!(token.kind(), SyntaxKind::OpenParen | SyntaxKind::CloseParen)
            && best.is_none()
        {
            best = Some(token);
        }
    }
    best
}

/// Evaluate a standard feature expression (`:sym`, `(and ...)`, `(or ...)`, `(not ...)`)
/// against a known set of uppercase feature names. Returns `None` if unknown or malformed.
pub fn eval_feature_expr(form: &AstForm, known_features: Option<&[String]>) -> Option<bool> {
    let features = known_features?;
    match form {
        AstForm::Atom(atom) => {
            let sym = atom.symbol()?;
            // In feature tests, unqualified symbols are read in KEYWORD.
            match sym.prefix {
                Prefix::None | Prefix::Keyword => {
                    Some(features.iter().any(|f| f.eq_ignore_ascii_case(&sym.name)))
                }
                _ => None,
            }
        }
        AstForm::List(list) if list.is_proper_closed() => {
            let mut forms = list.forms();
            let head = forms.next()?.symbol()?;
            let op = match head.prefix {
                Prefix::None | Prefix::Keyword => head.name.as_str(),
                Prefix::Package { ref name, .. } if name == "COMMON-LISP" || name == "CL" => {
                    head.name.as_str()
                }
                _ => return None,
            };
            match op {
                "NOT" => {
                    let arg = forms.next()?;
                    if forms.next().is_some() {
                        return None;
                    }
                    Some(!eval_feature_expr(&arg, known_features)?)
                }
                "AND" => {
                    let mut result = true;
                    for arg in forms {
                        if !eval_feature_expr(&arg, known_features)? {
                            result = false;
                        }
                    }
                    Some(result)
                }
                "OR" => {
                    let mut result = false;
                    for arg in forms {
                        if eval_feature_expr(&arg, known_features)? {
                            result = true;
                        }
                    }
                    Some(result)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Extract a string designator from an AST form.
pub fn form_designator(form: &AstForm) -> Option<String> {
    form.designator()
}

fn closing_paren(node: &SyntaxNode) -> Option<Token> {
    node.last_child_or_token()?
        .into_token()
        .filter(|token| token.kind() == SyntaxKind::CloseParen)
}

fn group_delimiters(node: &SyntaxNode, opener: SyntaxKind) -> Option<(Token, Token)> {
    let first = node.first_child_or_token()?.into_token()?;
    if first.kind() != opener {
        return None;
    }
    Some((first, closing_paren(node)?))
}

/// If the cursor at `offset` is on or immediately adjacent to an opening or closing
/// parenthesis of a closed list/vector, return `(open_range, close_range)`.
pub fn matching_parens_at(root: &SyntaxNode, offset: TextSize) -> Option<(TextRange, TextRange)> {
    let clamped = offset.min(root.text_range().end());
    for token in tokens_at_offset(root, clamped) {
        if !matches!(
            token.kind(),
            SyntaxKind::OpenParen | SyntaxKind::VectorStart | SyntaxKind::CloseParen
        ) {
            continue;
        }
        let Some(parent) = token.parent() else {
            continue;
        };
        let opener = match parent.kind() {
            SyntaxKind::List => SyntaxKind::OpenParen,
            SyntaxKind::Vector => SyntaxKind::VectorStart,
            _ => continue,
        };
        if let Some((first, last)) = group_delimiters(&parent, opener)
            && (first == token || last == token)
        {
            return Some((first.text_range(), last.text_range()));
        }
    }
    None
}

/// Whether an opening character can be paired in standard reader syntax.
/// Incomplete escapes, strings and character literals need the next character
/// as input. Comments and escaped symbols treat paired punctuation literally.
pub fn should_auto_pair_at(buffer: &str, insertion_point: usize, _open_char: char) -> bool {
    buffer
        .get(..insertion_point)
        .is_some_and(|prefix| can_pair_after(prefix, SourceReadtable::Standard))
}

pub(crate) fn can_pair_after(prefix: &str, readtable: SourceReadtable) -> bool {
    lex(prefix, readtable).last().is_none_or(|token| {
        use SyntaxDiagnosticKind::*;
        !matches!(token.kind, SyntaxKind::LineComment | SyntaxKind::RawString)
            && !token.diagnostic.is_some_and(|diagnostic| {
                matches!(
                    diagnostic.kind,
                    UnterminatedString
                        | UnterminatedBlockComment
                        | UnterminatedMultipleEscape
                        | IncompleteEscape
                        | IncompleteCharacterLiteral
                )
            })
    })
}

mod const_preview;

pub use const_preview::eval_const_preview;
#[cfg(feature = "analysis")]
pub(crate) use const_preview::{
    ConstantPreview, PreviewIdentity, eval_const_form_preview_with, eval_const_values_with,
};
