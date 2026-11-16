//! Built-in Common Lisp static linter, rule registry, suppression directives,
//! configuration loader, and safe machine-applicable quick-fix engine.

mod checks;
mod context;
pub(crate) mod macro_template;
mod rules;
mod suppressions;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;

use crate::format::TextEdit;
use crate::navigation::index::{DocumentIndex, WorkspaceIndex};
use crate::reader_tokens::{Prefix, SymbolName, canonical_package};
use crate::syntax::{SyntaxDiagnostic, SyntaxKind, SyntaxNode, TextRange, TextSize};

/// Broad classification of a lint rule, modeled after Rust Clippy and MoonBit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LintCategory {
    Correctness,
    Suspicious,
    Security,
    Complexity,
    Perf,
    Style,
    Pedantic,
}

impl LintCategory {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Correctness => "correctness",
            Self::Suspicious => "suspicious",
            Self::Security => "security",
            Self::Complexity => "complexity",
            Self::Perf => "perf",
            Self::Style => "style",
            Self::Pedantic => "pedantic",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "correctness" => Some(Self::Correctness),
            "suspicious" => Some(Self::Suspicious),
            "security" => Some(Self::Security),
            "complexity" => Some(Self::Complexity),
            "perf" => Some(Self::Perf),
            "style" => Some(Self::Style),
            "pedantic" => Some(Self::Pedantic),
            _ => None,
        }
    }
}

impl fmt::Display for LintCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Effective diagnostic severity for a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LintLevel {
    Allow,
    Warn,
    Deny,
}

impl LintLevel {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" | "off" | "none" => Some(Self::Allow),
            "warn" | "warning" => Some(Self::Warn),
            "deny" | "error" => Some(Self::Deny),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Warn => "warning",
            Self::Deny => "error",
        }
    }
}

/// Built-in rule preset controlling which rules are enabled by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum LintPreset {
    /// Conservative rules shared by `zkcl check` and default LSP diagnostics.
    #[default]
    Default,
    /// Full idiomatic Common Lisp engineering style (`zkcl lint`).
    Strict,
    /// Every built-in check, including `pedantic` rules.
    All,
}

impl LintPreset {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Strict => "strict",
            Self::All => "all",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "strict" | "recommended" => Some(Self::Strict),
            "all" | "pedantic" => Some(Self::All),
            _ => None,
        }
    }
}

/// Secondary LSP-aligned classification tags for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LintTag {
    Unnecessary,
    Deprecated,
}

/// Confidence level of an automated source fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FixApplicability {
    /// Guaranteed to preserve Common Lisp semantics when applied automatically.
    MachineApplicable,
    /// Plausible improvement that may alter evaluation count, types, or values.
    MaybeIncorrect,
    /// Partial template requiring manual user completion.
    HasPlaceholders,
}

/// Automated rewrite attached to a [`LintDiagnostic`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintFix {
    pub title: String,
    pub applicability: FixApplicability,
    pub edits: Vec<TextEdit>,
}

/// A structured diagnostic emitted by the built-in linter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintDiagnostic {
    pub rule_id: &'static str,
    pub category: LintCategory,
    pub level: LintLevel,
    pub range: TextRange,
    pub message: String,
    pub help: Option<String>,
    pub related: Vec<LintRelated>,
    pub tags: Vec<LintTag>,
    pub fix: Option<LintFix>,
}

/// A secondary source location explaining a lint diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintRelated {
    /// Workspace document name, or `None` for the primary diagnostic's document.
    pub document: Option<String>,
    pub range: TextRange,
    pub message: String,
}

impl LintDiagnostic {
    /// Capture a presentation-independent report against the exact sources
    /// used by this analysis. Related documents must come from the captured
    /// diagnostic workspace, rather than a later filesystem or editor revision.
    pub fn to_diagnostic<'a>(
        &self,
        source: &'a zkcl_diagnostics::SourceDocument,
        related_source: impl Fn(&str) -> Option<&'a zkcl_diagnostics::SourceDocument>,
    ) -> Option<zkcl_diagnostics::Diagnostic> {
        use zkcl_diagnostics::{
            Applicability, Diagnostic, DiagnosticTag, LabelRole, Note, NoteKind, Severity,
            SourceEdit, Suggestion,
        };
        let severity = match self.level {
            LintLevel::Allow => return None,
            LintLevel::Warn => Severity::Warning,
            LintLevel::Deny => Severity::Error,
        };
        let mut report = Diagnostic::new(severity, self.message.clone());
        report.code = Some(self.rule_id.to_owned());
        report
            .add_label(source, byte_range(self.range), LabelRole::Primary, "")
            .expect("lint primary range belongs to its analyzed source");
        for related in &self.related {
            let document = related
                .document
                .as_deref()
                .map_or(Some(source), &related_source);
            if let Some(document) = document {
                report
                    .add_label(
                        document,
                        byte_range(related.range),
                        LabelRole::Secondary,
                        &related.message,
                    )
                    .expect("lint related range belongs to its captured source");
            } else {
                // Preserve the explanation when an external source is unavailable;
                // its offsets cannot be represented as an exact retained span.
                report.notes.push(Note {
                    kind: NoteKind::Note,
                    message: format!(
                        "{}: {} (source unavailable)",
                        related.document.as_deref().unwrap(),
                        related.message
                    ),
                });
            }
        }
        if let Some(help) = &self.help {
            report.notes.push(Note {
                kind: NoteKind::Help,
                message: help.clone(),
            });
        }
        report.tags = self
            .tags
            .iter()
            .map(|tag| match tag {
                LintTag::Unnecessary => DiagnosticTag::Unnecessary,
                LintTag::Deprecated => DiagnosticTag::Deprecated,
            })
            .collect();
        if let Some(rule) = find_rule(self.rule_id) {
            report.rationale = Some(rule.rationale.to_owned());
            report.evidence = Some(rule.evidence.as_str().to_owned());
        }
        if let Some(fix) = &self.fix {
            let applicability = match fix.applicability {
                FixApplicability::MachineApplicable => Applicability::MachineApplicable,
                FixApplicability::MaybeIncorrect => Applicability::MaybeIncorrect,
                FixApplicability::HasPlaceholders => Applicability::HasPlaceholders,
            };
            let edits = fix
                .edits
                .iter()
                .map(|edit| {
                    SourceEdit::new(
                        source.clone(),
                        byte_range(edit.range),
                        edit.replacement.clone(),
                    )
                    .expect("lint edit range belongs to its analyzed source")
                })
                .collect();
            report.suggestions.push(
                Suggestion::new(&fix.title, applicability, edits)
                    .expect("edits in one lint fix do not conflict"),
            );
        }
        Some(report)
    }
}

impl SyntaxDiagnostic {
    /// Retain the parser's source revision with its precise error range.
    pub fn to_diagnostic(
        &self,
        source: &zkcl_diagnostics::SourceDocument,
    ) -> zkcl_diagnostics::Diagnostic {
        use zkcl_diagnostics::{Diagnostic, LabelRole, Severity};
        let mut report = Diagnostic::new(Severity::Error, self.message());
        report.code = Some("syntax/read".into());
        report
            .add_label(source, byte_range(self.range()), LabelRole::Primary, "")
            .expect("parser diagnostic range belongs to its parsed source");
        report
    }
}

fn byte_range(range: TextRange) -> std::ops::Range<usize> {
    usize::from(range.start())..usize::from(range.end())
}

/// The specification, analysis, or convention behind a lint rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleEvidence {
    LanguageSemantics,
    StandardLibrary,
    ZkclExtension,
    Dataflow,
    CommunityConvention,
    Heuristic,
}

impl RuleEvidence {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LanguageSemantics => "ANSI CL language semantics",
            Self::StandardLibrary => "ANSI CL standard library contract",
            Self::ZkclExtension => "zkcl extension contract",
            Self::Dataflow => "static scope & dataflow analysis",
            Self::CommunityConvention => "Common Lisp community convention",
            Self::Heuristic => "engineering heuristic",
        }
    }
}

/// Custom macro shape mapping (`:lint-as`) for user-defined macros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacroShape {
    Let,
    Defun,
    Defmacro,
    When,
    Unless,
    Dolist,
    Progn,
    Ignore,
}

impl MacroShape {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s
            .trim()
            .trim_start_matches(':')
            .to_ascii_lowercase()
            .as_str()
        {
            "let" => Some(Self::Let),
            "defun" => Some(Self::Defun),
            "defmacro" => Some(Self::Defmacro),
            "when" => Some(Self::When),
            "unless" => Some(Self::Unless),
            "dolist" => Some(Self::Dolist),
            "progn" => Some(Self::Progn),
            "ignore" => Some(Self::Ignore),
            _ => None,
        }
    }
}

/// Documentation and metadata for a single lint rule (`zkcl explain <rule>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleDescriptor {
    pub id: &'static str,
    pub short_name: &'static str,
    pub category: LintCategory,
    pub default_preset: LintPreset,
    pub evidence: RuleEvidence,
    pub summary: &'static str,
    pub rationale: &'static str,
    pub bad_example: &'static str,
    pub good_example: &'static str,
}

/// Errors returned when parsing or updating a [`LintConfig`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LintConfigError {
    #[error("unknown lint rule or category `{0}`")]
    UnknownTarget(String),
    #[error("unknown preset `{0}`")]
    UnknownPreset(String),
    #[error("unknown lint level `{0}`")]
    UnknownLevel(String),
    #[error("unknown macro shape `{0}`")]
    UnknownMacroShape(String),
    #[error("line {line}: {reason}")]
    TomlLine {
        line: usize,
        #[source]
        reason: Box<Self>,
    },
    #[error("expected `key = value` or `[section]`")]
    ExpectedTomlAssignment,
    #[error("expected integer for {0}")]
    ExpectedTomlInteger(&'static str),
    #[error("unknown configuration key `{0}`")]
    UnknownTomlKey(String),
    #[error("syntax error in `.zkcl-lint.lisp`: {}", .0.message())]
    SexpSyntax(#[source] SyntaxDiagnostic),
    #[error("{0} in `.zkcl-lint.lisp`")]
    SexpStructure(&'static str),
    #[error("invalid integer `{value}` for `:{key}`")]
    InvalidSexpInteger { key: &'static str, value: String },
    #[error("missing level for rule `{0}`")]
    MissingRuleLevel(String),
    #[error("expected `({0} :lint-as :let)` in `:macros`")]
    InvalidMacroForm(String),
    #[error("expected shape for macro `{0}`")]
    MissingMacroShape(String),
    #[error("unknown key `:{0}` in `.zkcl-lint.lisp`")]
    UnknownSexpKey(String),
}

impl LintConfigError {
    fn at_line(self, line: usize) -> Self {
        Self::TomlLine {
            line,
            reason: Box::new(self),
        }
    }
}

/// Threshold and rule-level configuration for a lint run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintConfig {
    pub preset: LintPreset,
    pub category_overrides: HashMap<LintCategory, LintLevel>,
    pub rule_overrides: HashMap<&'static str, LintLevel>,
    /// Bare symbol names (normally uppercase), or `HOME-PACKAGE::NAME` keys.
    /// Literal `:` and `\` in either component are backslash-escaped.
    pub macro_shapes: HashMap<String, MacroShape>,
    pub max_complexity: usize,
    pub max_nesting: usize,
}

impl Default for LintConfig {
    fn default() -> Self {
        Self::from_preset(LintPreset::Default)
    }
}

impl LintConfig {
    #[must_use]
    pub fn from_preset(preset: LintPreset) -> Self {
        Self {
            preset,
            category_overrides: HashMap::new(),
            rule_overrides: HashMap::new(),
            macro_shapes: HashMap::new(),
            max_complexity: 15,
            max_nesting: 8,
        }
    }

    /// Set the level of a rule ID, short rule name, category name, or `"all"`.
    pub fn set_target_level(
        &mut self,
        target: &str,
        level: LintLevel,
    ) -> Result<(), LintConfigError> {
        let trimmed = target.trim();
        if trimmed.eq_ignore_ascii_case("all") {
            for rule in rules::RULES {
                self.rule_overrides.insert(rule.id, level);
            }
            return Ok(());
        }
        if let Some(cat) = LintCategory::parse(trimmed) {
            self.category_overrides.insert(cat, level);
            return Ok(());
        }
        if let Some(rule) = rules::find_rule(trimmed) {
            self.rule_overrides.insert(rule.id, level);
            return Ok(());
        }
        Err(LintConfigError::UnknownTarget(trimmed.to_owned()))
    }

    /// Resolve effective [`LintLevel`] for `rule`.
    #[must_use]
    pub fn level_for(&self, rule: &RuleDescriptor) -> LintLevel {
        if let Some(&level) = self.rule_overrides.get(rule.id) {
            return level;
        }
        if let Some(&level) = self.category_overrides.get(&rule.category) {
            return level;
        }
        if self.preset >= rule.default_preset {
            LintLevel::Warn
        } else {
            LintLevel::Allow
        }
    }

    /// Prefer the resolved home-package override to the legacy bare-name override.
    /// If the index cannot resolve a home, only the bare name can match.
    pub(crate) fn macro_shape_for(
        &self,
        workspace: &WorkspaceIndex<'_>,
        head: &SymbolName,
        package: Option<&str>,
    ) -> Option<MacroShape> {
        if self.macro_shapes.is_empty() {
            return None;
        }
        workspace
            .symbol_home_package(head, package)
            .and_then(|home| {
                self.macro_shapes
                    .get(&qualified_macro_key(home, &head.name))
            })
            .or_else(|| {
                self.macro_shapes
                    .get(macro_key_component(&head.name).as_ref())
            })
            .copied()
    }

    /// Parse a `.zkcl-lint.toml` configuration file.
    /// Package-qualified keys in `[macros]` can be quoted, e.g.
    /// `"my-dsl::with-bindings" = "let"`.
    pub fn parse_toml(source: &str) -> Result<Self, LintConfigError> {
        let mut config = Self::from_preset(LintPreset::Strict);
        let mut section = String::new();
        for (line_idx, raw_line) in source.lines().enumerate() {
            let line_no = line_idx + 1;
            let line = strip_toml_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[')
                && let Some(name) = rest.strip_suffix(']')
            {
                section = name.trim().to_ascii_lowercase();
                continue;
            }
            let Some((key, val)) = line.split_once('=') else {
                return Err(LintConfigError::ExpectedTomlAssignment.at_line(line_no));
            };
            let key = unquote_toml(key.trim());
            let val = val.trim();
            match (section.as_str(), key.as_str()) {
                ("" | "lint", "preset") => {
                    let s = unquote_toml(val);
                    config.preset = LintPreset::parse(&s)
                        .ok_or_else(|| LintConfigError::UnknownPreset(s).at_line(line_no))?;
                }
                ("" | "lint" | "thresholds", "max-complexity" | "max_complexity") => {
                    config.max_complexity = val.parse::<usize>().map_err(|_| {
                        LintConfigError::ExpectedTomlInteger("max_complexity").at_line(line_no)
                    })?;
                }
                ("" | "lint" | "thresholds", "max-nesting" | "max_nesting") => {
                    config.max_nesting = val.parse::<usize>().map_err(|_| {
                        LintConfigError::ExpectedTomlInteger("max_nesting").at_line(line_no)
                    })?;
                }
                ("rules" | "lint.rules" | "categories" | "lint.categories", _) => {
                    let s = unquote_toml(val);
                    let level = LintLevel::parse(&s)
                        .ok_or_else(|| LintConfigError::UnknownLevel(s).at_line(line_no))?;
                    config
                        .set_target_level(&key, level)
                        .map_err(|err| err.at_line(line_no))?;
                }
                ("macros" | "lint.macros", _) => {
                    let s = unquote_toml(val);
                    let shape = MacroShape::parse(&s)
                        .ok_or_else(|| LintConfigError::UnknownMacroShape(s).at_line(line_no))?;
                    config
                        .macro_shapes
                        .insert(macro_shape_config_key(&key), shape);
                }
                _ => {
                    return Err(LintConfigError::UnknownTomlKey(key).at_line(line_no));
                }
            }
        }
        Ok(config)
    }

    /// Parse a read-only S-expression configuration (`.zkcl-lint.lisp`) using the CST parser.
    ///
    /// Example:
    /// ```lisp
    /// (:preset :strict
    ///  :rules ((:if-to-when :deny) (:missing-docstring :allow))
    ///  :macros ((:with-db :lint-as :let)
    ///           (my-dsl::with-bindings :lint-as :let)))
    /// ```
    pub fn parse_sexp(source: &str) -> Result<Self, LintConfigError> {
        use crate::navigation::index::{closed_list, symbol};
        use crate::outline::elements;
        use crate::reader_tokens::string_designator;

        let parsed = crate::syntax::parse_source(source);
        if let Some(diagnostic) = parsed.diagnostics().first() {
            return Err(LintConfigError::SexpSyntax(diagnostic.clone()));
        }
        let root = parsed.root();
        let mut top = elements(&root);
        let Some(first) = top.next() else {
            return Ok(Self::from_preset(LintPreset::Strict));
        };
        if top.next().is_some() {
            return Err(LintConfigError::SexpStructure(
                "expected a single top-level plist",
            ));
        }
        let Some(list) = first.as_node().filter(|n| closed_list(n)) else {
            return Err(LintConfigError::SexpStructure("expected a plist `(...)`"));
        };

        let mut config = Self::from_preset(LintPreset::Strict);
        let items: Vec<_> = elements(list).collect();
        if !items.len().is_multiple_of(2) {
            return Err(LintConfigError::SexpStructure(
                "expected even number of key/value entries",
            ));
        }
        for pair in items.as_chunks::<2>().0 {
            let key = string_designator(&pair[0])
                .ok_or(LintConfigError::SexpStructure("expected keyword key"))?;
            match key.as_str() {
                "PRESET" => {
                    let s = string_designator(&pair[1]).ok_or(LintConfigError::SexpStructure(
                        "expected preset name after `:preset`",
                    ))?;
                    config.preset =
                        LintPreset::parse(&s).ok_or(LintConfigError::UnknownPreset(s))?;
                }
                "MAX-COMPLEXITY" => {
                    config.max_complexity = parse_sexp_usize(&pair[1], "max-complexity")?;
                }
                "MAX-NESTING" => {
                    config.max_nesting = parse_sexp_usize(&pair[1], "max-nesting")?;
                }
                "RULES" => {
                    let rules_node = pair[1].as_node().filter(|n| closed_list(n)).ok_or(
                        LintConfigError::SexpStructure("expected list after `:rules`"),
                    )?;
                    for entry in elements(rules_node) {
                        let enode = entry.as_node().filter(|n| closed_list(n)).ok_or(
                            LintConfigError::SexpStructure(
                                "expected `(rule level)` entry in `:rules`",
                            ),
                        )?;
                        let mut ep = elements(enode);
                        let rule = ep.next().and_then(|e| string_designator(&e)).ok_or(
                            LintConfigError::SexpStructure("missing rule name in `:rules`"),
                        )?;
                        let level_str = ep
                            .next()
                            .and_then(|e| string_designator(&e))
                            .ok_or_else(|| LintConfigError::MissingRuleLevel(rule.clone()))?;
                        let level = LintLevel::parse(&level_str)
                            .ok_or(LintConfigError::UnknownLevel(level_str))?;
                        config.set_target_level(&rule, level)?;
                    }
                }
                "MACROS" => {
                    let macros_node = pair[1].as_node().filter(|n| closed_list(n)).ok_or(
                        LintConfigError::SexpStructure("expected list after `:macros`"),
                    )?;
                    for entry in elements(macros_node) {
                        let enode = entry.as_node().filter(|n| closed_list(n)).ok_or(
                            LintConfigError::SexpStructure(
                                "expected `(macro :lint-as shape)` in `:macros`",
                            ),
                        )?;
                        let ep: Vec<_> = elements(enode).collect();
                        let name = ep
                            .first()
                            .and_then(|element| {
                                if let Some(name) = symbol(element) {
                                    Some(macro_shape_symbol_key(&name))
                                } else {
                                    string_designator(element).map(|s| macro_shape_config_key(&s))
                                }
                            })
                            .ok_or(LintConfigError::SexpStructure(
                                "missing macro name in `:macros`",
                            ))?;
                        let shape_el = if ep.len() == 3
                            && ep
                                .get(1)
                                .and_then(symbol)
                                .is_some_and(|s| s.name == "LINT-AS")
                        {
                            &ep[2]
                        } else if ep.len() == 2 {
                            &ep[1]
                        } else {
                            return Err(LintConfigError::InvalidMacroForm(name));
                        };
                        let shape_str = string_designator(shape_el)
                            .ok_or_else(|| LintConfigError::MissingMacroShape(name.clone()))?;
                        let shape = MacroShape::parse(&shape_str)
                            .ok_or(LintConfigError::UnknownMacroShape(shape_str))?;
                        config.macro_shapes.insert(name, shape);
                    }
                }
                other => return Err(LintConfigError::UnknownSexpKey(other.to_owned())),
            }
        }
        Ok(config)
    }
}

fn parse_sexp_usize(
    element: &crate::outline::Element,
    key: &'static str,
) -> Result<usize, LintConfigError> {
    let text =
        element
            .as_token()
            .map(|token| token.text())
            .ok_or(LintConfigError::SexpStructure(match key {
                "max-complexity" => "expected integer after `:max-complexity`",
                _ => "expected integer after `:max-nesting`",
            }))?;
    text.parse::<usize>()
        .map_err(|_| LintConfigError::InvalidSexpInteger {
            key,
            value: text.to_owned(),
        })
}

// Keep the public map's ordinary keys readable, but reserve unescaped "::"
// for the separator, not a colon that came from an escaped Lisp name.
fn macro_key_component(name: &str) -> Cow<'_, str> {
    if !name.contains([':', '\\']) {
        return Cow::Borrowed(name);
    }
    let mut escaped = String::with_capacity(name.len());
    for ch in name.chars() {
        if matches!(ch, ':' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    Cow::Owned(escaped)
}

fn qualified_macro_key(package: &str, name: &str) -> String {
    format!(
        "{}::{}",
        macro_key_component(package),
        macro_key_component(name)
    )
}

fn macro_shape_symbol_key(symbol: &SymbolName) -> String {
    match &symbol.prefix {
        Prefix::Package { name: package, .. } => {
            qualified_macro_key(canonical_package(package), &symbol.name)
        }
        _ => macro_key_component(&symbol.name).into_owned(),
    }
}

fn macro_shape_config_key(text: &str) -> String {
    if let Some(symbol) = SymbolName::parse(text)
        && matches!(symbol.prefix, Prefix::None | Prefix::Package { .. })
    {
        return macro_shape_symbol_key(&symbol);
    }
    macro_key_component(&text.to_ascii_uppercase()).into_owned()
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_quotes = false;
    let mut escape = false;
    for (idx, ch) in line.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_quotes => escape = true,
            '"' => in_quotes = !in_quotes,
            '#' if !in_quotes => return &line[..idx],
            _ => {}
        }
    }
    line
}

fn unquote_toml(s: &str) -> String {
    let trimmed = s.trim();
    if let Some(inner) = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return inner.to_owned();
    }
    if let Some(inner) = trimmed
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return inner.to_owned();
    }
    trimmed.to_owned()
}

/// Return metadata for every built-in lint rule.
#[must_use]
pub fn all_rules() -> &'static [RuleDescriptor] {
    rules::RULES
}

/// Look up a built-in lint rule by full ID (`style/if-to-when`) or short name (`if-to-when`).
#[must_use]
pub fn find_rule(query: &str) -> Option<&'static RuleDescriptor> {
    rules::find_rule(query)
}

/// Apply non-overlapping fixes from `diagnostics` to `text`.
///
/// Edits in a fix are applied together. `include_unsafe` also permits uncertain
/// edits. Fixes with placeholders always require interaction.
/// Returns `(updated_text, applied_fix_count)`.
#[must_use]
pub fn apply_fixes(
    text: &str,
    diagnostics: &[LintDiagnostic],
    include_unsafe: bool,
) -> (String, usize) {
    let (edits, applied) = select_fixes(text, diagnostics, include_unsafe);
    if edits.is_empty() {
        return (text.to_owned(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for edit in edits {
        let start = usize::from(edit.range.start());
        let end = usize::from(edit.range.end());
        out.push_str(&text[cursor..start]);
        out.push_str(&edit.replacement);
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    (out, applied)
}

pub(crate) fn select_fixes<'a>(
    text: &str,
    diagnostics: &'a [LintDiagnostic],
    include_unsafe: bool,
) -> (Vec<&'a TextEdit>, usize) {
    let mut occupied = BTreeMap::<TextSize, &TextEdit>::new();
    let mut applied = 0usize;
    for fix in diagnostics
        .iter()
        .filter_map(|diagnostic| diagnostic.fix.as_ref())
    {
        if !matches!(fix.applicability, FixApplicability::MachineApplicable)
            && !(include_unsafe && fix.applicability == FixApplicability::MaybeIncorrect)
        {
            continue;
        }
        let mut group: Vec<_> = fix.edits.iter().collect();
        group.sort_unstable_by_key(|edit| edit.range.start());
        if group.is_empty()
            || group
                .windows(2)
                .any(|pair| edits_conflict(pair[0].range, pair[1].range))
            || group.iter().any(|edit| {
                let range = edit.range;
                let start = range.start();
                !text.is_char_boundary(usize::from(start))
                    || !text.is_char_boundary(usize::from(range.end()))
                    || occupied
                        .range(..start)
                        .next_back()
                        .is_some_and(|(_, previous)| edits_conflict(previous.range, range))
                    || occupied
                        .range(start..)
                        .next()
                        .is_some_and(|(_, next)| edits_conflict(range, next.range))
            })
        {
            continue;
        }
        occupied.extend(group.into_iter().map(|edit| (edit.range.start(), edit)));
        applied += 1;
    }

    (occupied.into_values().collect(), applied)
}

fn edits_conflict(left: TextRange, right: TextRange) -> bool {
    // The caller orders ranges by start. An insertion at a replacement's
    // boundary would make the outcome depend on the order of the edits.
    left.end() > right.start()
        || (left.end() == right.start() && (left.is_empty() || right.is_empty()))
}

mod reuse;
pub use reuse::FormLintCache;
pub(crate) use reuse::NamespaceInputs;

pub(crate) fn lint_with_workspace(
    text: &str,
    source: &DocumentIndex,
    root: &SyntaxNode,
    workspace: &WorkspaceIndex<'_>,
    config: &LintConfig,
    cancelled: &impl Fn() -> bool,
) -> Option<Vec<LintDiagnostic>> {
    lint_with_form_cache(text, source, root, workspace, config, cancelled, None)
}

pub(crate) fn lint_with_form_cache(
    text: &str,
    source: &DocumentIndex,
    root: &SyntaxNode,
    workspace: &WorkspaceIndex<'_>,
    config: &LintConfig,
    cancelled: &impl Fn() -> bool,
    form_cache: Option<&mut FormLintCache>,
) -> Option<Vec<LintDiagnostic>> {
    if cancelled() {
        return None;
    }
    let mut suppressions = suppressions::SuppressionTable::collect(text, root);
    let mut emitter = context::LintEmitter {
        config,
        suppressions: &mut suppressions,
        diagnostics: Vec::new(),
    };
    let ctx = context::LintDocument {
        text,
        root,
        source,
        workspace,
        macro_operands: Default::default(),
    };

    checks::run_all(&ctx, &mut emitter, cancelled, form_cache)?;
    emitter.emit_unmatched_suppressions();
    emitter
        .diagnostics
        .sort_by_key(|d| (d.range.start(), d.range.end(), d.rule_id));
    emitter
        .diagnostics
        .dedup_by(|a, b| a.range == b.range && a.rule_id == b.rule_id);
    preserve_source_directives(text, root, &mut emitter.diagnostics, cancelled)?;
    Some(emitter.diagnostics)
}

fn preserve_source_directives(
    text: &str,
    root: &SyntaxNode,
    diagnostics: &mut [LintDiagnostic],
    cancelled: &impl Fn() -> bool,
) -> Option<()> {
    if !(text.contains(';') || text.contains('#')) {
        return Some(());
    }
    let has_fixes = diagnostics.iter().any(|diag| diag.fix.is_some());
    if !has_fixes {
        return Some(());
    }
    // Check only subtrees overlapping each fix rather than scanning the whole file CST.
    for diag in diagnostics {
        if cancelled() {
            return None;
        }
        let Some(fix) = &diag.fix else {
            continue;
        };
        let touches_protected = fix.edits.iter().any(|edit| {
            let range = edit.range;
            if range.is_empty() {
                return false;
            }
            let is_protected_kind = |kind: SyntaxKind| {
                matches!(
                    kind,
                    SyntaxKind::LineComment
                        | SyntaxKind::BlockComment
                        | SyntaxKind::ReaderConditional
                        | SyntaxKind::UnknownDispatch
                )
            };
            match root.covering_element(range) {
                rowan::NodeOrToken::Token(tok) => {
                    is_protected_kind(tok.kind())
                        || tok.parent_ancestors().any(|p| is_protected_kind(p.kind()))
                }
                rowan::NodeOrToken::Node(node) => {
                    node.ancestors().any(|p| is_protected_kind(p.kind()))
                        || node.descendants_with_tokens().any(|part| {
                            let pr = part.text_range();
                            pr.start() < range.end()
                                && pr.end() > range.start()
                                && is_protected_kind(part.kind())
                        })
                }
            }
        });
        if touches_protected {
            // A rewrite can drop trivia or a reader boundary used by another target.
            diag.fix = None;
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macro_shapes_parse_qualified_names_and_keep_bare_names() {
        let toml = LintConfig::parse_toml(
            r#"
[macros]
"my-dsl::with-bindings" = "let"
with-bindings = "ignore"
"other:with-buffer" = "let"
"#,
        )
        .unwrap();
        let sexp = LintConfig::parse_sexp(
            r#"(:macros ((my-dsl::with-bindings :lint-as :let)
                         (:with-bindings :ignore)
                         ("other:with-buffer" :let)))"#,
        )
        .unwrap();
        assert_eq!(toml.macro_shapes, sexp.macro_shapes);
        assert_eq!(toml.macro_shapes["MY-DSL::WITH-BINDINGS"], MacroShape::Let);
        assert_eq!(toml.macro_shapes["WITH-BINDINGS"], MacroShape::Ignore);
        assert_eq!(toml.macro_shapes["OTHER::WITH-BUFFER"], MacroShape::Let);
    }

    #[test]
    fn macro_shape_lookup_uses_resolved_home_and_falls_back_to_bare_name() {
        let mut config = LintConfig::parse_sexp(
            "(:macros ((my-dsl::with-bindings :let)
                       (my-dsl::private-macro :ignore)))",
        )
        .unwrap();
        let packages = crate::syntax::parse_source(
            "(defpackage :my-dsl
                (:nicknames :dsl-native)
                (:export :with-bindings))
             (defpackage :client
                (:local-nicknames (:ffi :my-dsl))
                (:import-from :my-dsl :with-bindings))
             (defpackage :other
                (:shadow :with-bindings))",
        );
        let document = DocumentIndex::collect(&packages.root(), None, None, Default::default());
        let workspace = WorkspaceIndex::new([("packages.lisp", &document)]);
        let lookup = |config: &LintConfig, head, package| {
            config.macro_shape_for(&workspace, &SymbolName::parse(head).unwrap(), package)
        };
        for (head, package) in [
            ("my-dsl::with-bindings", None),
            ("dsl-native:with-bindings", None),
            ("ffi:with-bindings", Some("CLIENT")),
            ("with-bindings", Some("MY-DSL")),
            ("with-bindings", Some("CLIENT")),
        ] {
            assert_eq!(lookup(&config, head, package), Some(MacroShape::Let));
        }
        assert_eq!(lookup(&config, "ffi:with-bindings", None), None);
        assert_eq!(lookup(&config, "my-dsl:private-macro", None), None);
        assert_eq!(lookup(&config, "with-bindings", None), None);
        assert_eq!(lookup(&config, "with-bindings", Some("OTHER")), None);
        assert_eq!(lookup(&config, "other::with-bindings", None), None);

        config
            .macro_shapes
            .insert("WITH-BINDINGS".into(), MacroShape::Ignore);
        assert_eq!(
            lookup(&config, "with-bindings", Some("CLIENT")),
            Some(MacroShape::Let)
        );
        assert_eq!(
            lookup(&config, "with-bindings", None),
            Some(MacroShape::Ignore)
        );
        assert_eq!(
            lookup(&config, "other::with-bindings", None),
            Some(MacroShape::Ignore)
        );
    }

    #[test]
    fn escaped_colons_do_not_collide_with_the_package_separator() {
        let config = LintConfig::parse_sexp(
            r#"(:macros ((p::x :let)
                         (|P::X| :ignore)
                         ("|p::X|" :when)
                         (|Q::R|::s :let)
                         (q::|R::S| :progn)))"#,
        )
        .unwrap();
        let toml = LintConfig::parse_toml(
            r#"
[macros]
"p::x" = "let"
"|P::X|" = "ignore"
"|p::X|" = "when"
"|Q::R|::s" = "let"
"q::|R::S|" = "progn"
"#,
        )
        .unwrap();
        assert_eq!(config.macro_shapes, toml.macro_shapes);

        let workspace = WorkspaceIndex::new([]);
        for (head, shape) in [
            ("p::x", MacroShape::Let),
            ("|P::X|", MacroShape::Ignore),
            ("|p::X|", MacroShape::When),
            ("|Q::R|::s", MacroShape::Let),
            ("q::|R::S|", MacroShape::Progn),
        ] {
            let head = SymbolName::parse(head).unwrap();
            assert_eq!(config.macro_shape_for(&workspace, &head, None), Some(shape));
        }
    }

    #[test]
    fn explicit_source_package_can_supply_home_without_defpackage() {
        let config = LintConfig::parse_sexp("(:macros ((my-dsl::with-bindings :let)))").unwrap();
        let parsed = crate::syntax::parse_source("(in-package :my-dsl) (with-bindings)");
        let root = parsed.root();
        let document = DocumentIndex::collect(&root, None, None, Default::default());
        let workspace = WorkspaceIndex::new([("cffi.lisp", &document)]);
        let head = SymbolName::parse("with-bindings").unwrap();
        assert_eq!(
            config.macro_shape_for(
                &workspace,
                &head,
                document.package_at(root.text_range().end())
            ),
            Some(MacroShape::Let)
        );
    }
}
