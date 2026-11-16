//! Relative diagnostics for complete, independently scoped forms.

use std::sync::Arc;

use rowan::GreenNode;
use rustc_hash::FxHashMap;

use crate::navigation::Name;
use crate::navigation::index::{DocumentIndex, closed_list, symbol};
use crate::outline::elements;
use crate::reader_tokens::SymbolName;
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize};

use super::context::{LintDocument, LintEmitter};
use super::{LintConfig, LintDiagnostic};

/// Cache for successive checks of one open document. It owns no old source
/// snapshots. Locations and suppression directives are resolved on each check.
#[derive(Default)]
pub struct FormLintCache {
    environment: Vec<(String, Arc<NamespaceInputs>)>,
    features: Option<Vec<String>>,
    config: Option<LintConfig>,
    forms: Forms,
}

type Forms = FxHashMap<FormKey, Arc<FormDiagnostics>>;
const MAX_FORMS: usize = 16_384;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(PartialEq, Eq, Hash)]
struct FormKey {
    identity: usize,
    package: Option<String>,
    line: LineEdges,
    preceding_macros: usize,
}

/// Whole-form separation establishes the outside inputs of lint fixes. The
/// column affects indentation, and the terminators distinguish end of input.
#[derive(PartialEq, Eq, Hash)]
struct LineEdges {
    leading_column: usize,
    previous_whitespace: bool,
    next_whitespace: bool,
}

struct FormDiagnostics {
    // The green node keeps its pointer identity alive until this entry leaves.
    _green: GreenNode,
    diagnostics: Vec<LintDiagnostic>,
    bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NamespaceInputs {
    packages: Vec<PackageInputs>,
    declarations: Vec<DeclarationInputs>,
    specials: Vec<(Name, SymbolName, SymbolName)>,
    guarded_callables: Vec<Name>,
    bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct DeclarationInputs {
    name: Name,
    operator: SymbolName,
    evaluates_arguments: bool,
    params: Option<Arc<[String]>>,
    macro_source: Option<GreenNode>,
}

#[derive(Debug, PartialEq, Eq)]
struct PackageInputs {
    name: String,
    nicknames: Vec<String>,
    local_nicknames: FxHashMap<String, String>,
    exports: Vec<String>,
    imports: Vec<(String, String)>,
    shadows: Vec<String>,
    uses: Vec<String>,
}

impl NamespaceInputs {
    /// The name-resolution inputs of form checks. Source coordinates are
    /// irrelevant to results whose locations all belong to the analyzed form.
    pub(crate) fn collect(index: &DocumentIndex) -> Self {
        let mut inputs = Self {
            packages: index
                .packages
                .iter()
                .map(|package| PackageInputs {
                    name: package.name.clone(),
                    nicknames: package.nicknames.clone(),
                    local_nicknames: package.local_nicknames.clone(),
                    exports: package.exports.clone(),
                    imports: package
                        .imports
                        .iter()
                        .map(|import| (import.source.clone(), import.name.clone()))
                        .collect(),
                    shadows: package.shadows.clone(),
                    uses: package.uses.clone(),
                })
                .collect(),
            declarations: index
                .declarations
                .iter()
                .map(|declaration| DeclarationInputs {
                    name: declaration.name.clone(),
                    operator: declaration.operator.clone(),
                    evaluates_arguments: declaration.evaluates_arguments,
                    params: declaration.params.clone(),
                    macro_source: declaration
                        .macro_definition
                        .as_ref()
                        .map(|definition| definition.source.clone()),
                })
                .collect(),
            specials: index.special_proclamations.clone(),
            guarded_callables: index.guarded_callables.clone(),
            bytes: 0,
        };
        inputs.bytes = inputs
            .declarations
            .iter()
            .map(|decl| {
                size_of::<DeclarationInputs>()
                    + name_bytes(&decl.name)
                    + symbol_bytes(&decl.operator)
                    + decl.params.as_ref().map_or(0, |params| {
                        params
                            .iter()
                            .map(|parameter| size_of::<String>() + parameter.len())
                            .sum()
                    })
                    + decl
                        .macro_source
                        .as_ref()
                        .map_or(0, |green| usize::from(green.text_len()).saturating_mul(8))
            })
            .sum::<usize>()
            + inputs
                .specials
                .iter()
                .map(|(name, decl, spec)| {
                    size_of::<(Name, SymbolName, SymbolName)>()
                        + name_bytes(name)
                        + symbol_bytes(decl)
                        + symbol_bytes(spec)
                })
                .sum::<usize>()
            + inputs
                .guarded_callables
                .iter()
                .map(|name| size_of::<Name>() + name_bytes(name))
                .sum::<usize>()
            + inputs
                .packages
                .iter()
                .map(PackageInputs::bytes)
                .sum::<usize>();
        inputs
    }
}

impl PackageInputs {
    fn bytes(&self) -> usize {
        let names = self
            .nicknames
            .iter()
            .chain(&self.exports)
            .chain(&self.shadows)
            .chain(&self.uses);
        let aliases = self
            .local_nicknames
            .iter()
            .chain(self.imports.iter().map(|(source, name)| (source, name)));
        size_of::<Self>()
            + self.name.len()
            + names
                .map(|name| size_of::<String>() + name.len())
                .sum::<usize>()
            + aliases
                .map(|(left, right)| 2 * size_of::<String>() + left.len() + right.len())
                .sum::<usize>()
    }
}

fn name_bytes(name: &Name) -> usize {
    symbol_bytes(&name.spelling) + name.package.as_ref().map_or(0, String::len)
}

fn symbol_bytes(symbol: &SymbolName) -> usize {
    symbol.name.len()
        + match &symbol.prefix {
            crate::reader_tokens::Prefix::Package { name, .. } => name.len(),
            _ => 0,
        }
}

impl FormLintCache {
    pub(super) fn check(
        &mut self,
        document: &LintDocument<'_>,
        emitter: &mut LintEmitter<'_>,
        mut analyze: impl FnMut(&SyntaxNode, &mut LintEmitter<'_>) -> Option<()>,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Option<Vec<crate::outline::Element>>> {
        let documents = &document.workspace.documents;
        let environment_matches = self.environment.len() == documents.len()
            && self.environment.iter_mut().zip(documents).all(
                |((name, previous), &(next_name, doc))| {
                    if name.as_str() != next_name {
                        return false;
                    }
                    let next = doc.form_lint_inputs();
                    if Arc::ptr_eq(previous, next) {
                        return true;
                    }
                    if previous.as_ref() != next.as_ref() {
                        return false;
                    }
                    // Keep the current revision's proof for later checks.
                    *previous = Arc::clone(next);
                    true
                },
            );
        if !environment_matches {
            self.environment = documents
                .iter()
                .map(|&(name, doc)| (name.to_owned(), Arc::clone(doc.form_lint_inputs())))
                .collect();
        }
        let environment_bytes: usize = self
            .environment
            .iter()
            .map(|(name, input)| {
                size_of::<(String, Arc<NamespaceInputs>)>() + name.len() + input.bytes
            })
            .sum();
        if environment_bytes >= MAX_BYTES {
            *self = Self::default();
            return Some(None);
        }
        let features = document.source.known_features();
        if !environment_matches
            || self.config.as_ref() != Some(emitter.config)
            || self.features.as_deref() != features
        {
            self.forms.clear();
        }
        if self.features.as_deref() != features {
            self.features = features.map(<[String]>::to_vec);
        }
        if self.config.as_ref() != Some(emitter.config) {
            self.config = Some(emitter.config.clone());
        }

        // Bound the number of retained roots as well as their source and
        // diagnostic payload. Only forms from the latest check survive.
        let mut current = Forms::default();
        current.reserve(self.forms.len());
        let mut remaining = MAX_BYTES - environment_bytes;
        let mut rest = Vec::new();
        let mut macros = document
            .source
            .declarations
            .iter()
            .filter_map(|decl| {
                decl.macro_definition
                    .is_some()
                    .then_some(decl.range.start())
            })
            .peekable();
        let mut preceding_macros = 0;
        for element in document.root.children_with_tokens() {
            if cancelled() {
                return None;
            }
            while macros
                .next_if(|&offset| offset < element.text_range().start())
                .is_some()
            {
                preceding_macros += 1;
            }
            let Some((form, key)) = independent_function(document, &element, preceding_macros)
            else {
                rest.push(element);
                continue;
            };
            let start = form.text_range().start();
            let entry = current.get(&key).or_else(|| self.forms.get(&key));
            if let Some(entry) = entry {
                for diagnostic in &entry.diagnostics {
                    let mut diagnostic = diagnostic.clone();
                    project(&mut diagnostic, |range| range + start);
                    emitter.emit_buffered(diagnostic);
                }
                if !current.contains_key(&key)
                    && current.len() < MAX_FORMS
                    && entry.bytes <= remaining
                {
                    remaining -= entry.bytes;
                    current.insert(key, Arc::clone(entry));
                }
                continue;
            }

            let mut suppressions = super::suppressions::SuppressionTable::default();
            let mut buffer = LintEmitter {
                config: emitter.config,
                suppressions: &mut suppressions,
                diagnostics: Vec::new(),
            };
            analyze(form, &mut buffer)?;
            let bounds = form.text_range();
            let mut bytes = size_of::<FormKey>()
                + size_of::<FormDiagnostics>()
                + key.package.as_ref().map_or(0, String::len)
                + usize::from(bounds.len());
            let mut contained = true;
            for diagnostic in &buffer.diagnostics {
                bytes = bytes.saturating_add(diagnostic_bytes(diagnostic));
                contained &= diagnostic.range.start() >= bounds.start()
                    && bounds.contains_range(diagnostic.range)
                    && diagnostic
                        .related
                        .iter()
                        .all(|r| r.document.is_none() && bounds.contains_range(r.range))
                    && diagnostic.fix.as_ref().is_none_or(|fix| {
                        fix.edits
                            .iter()
                            .all(|edit| bounds.contains_range(edit.range))
                    });
                emitter.emit_buffered(diagnostic.clone());
            }
            if contained && current.len() < MAX_FORMS && bytes <= remaining {
                remaining -= bytes;
                for diagnostic in &mut buffer.diagnostics {
                    project(diagnostic, |range| range - start);
                }
                current.insert(
                    key,
                    Arc::new(FormDiagnostics {
                        _green: form.green().to_owned(),
                        diagnostics: buffer.diagnostics,
                        bytes,
                    }),
                );
            }
        }
        self.forms = current;
        Some(Some(rest))
    }
}

fn independent_function<'a>(
    document: &LintDocument<'_>,
    element: &'a crate::outline::Element,
    preceding_macros: usize,
) -> Option<(&'a SyntaxNode, FormKey)> {
    let form = element.as_node()?;
    let range = form.text_range();
    if form.kind() != SyntaxKind::List
        || !closed_list(form)
        || document.source.top_level_form_at(range.start())?.range != range
        || !document.source.package_known_at(range.start())
        || !document
            .source
            .reader_syntax_known_at(range.end() - TextSize::from(1))
    {
        return None;
    }
    let package = document.source.package_at(range.start());
    let operator = elements(form).next().as_ref().and_then(symbol)?;
    if operator.name != "DEFUN"
        || document.workspace.standard_symbol(&operator, package) != Some("DEFUN")
    {
        return None;
    }
    let before = form.first_token()?.prev_token();
    let leading_column = match &before {
        Some(token) if token.kind() == SyntaxKind::Whitespace => {
            token.text().rsplit_once('\n')?.1.chars().take(80).count()
        }
        Some(_) => return None,
        None => 0,
    };
    let after = form.last_token()?.next_token();
    if after
        .as_ref()
        .is_some_and(|token| token.kind() != SyntaxKind::Whitespace || !token.text().contains('\n'))
    {
        return None;
    }
    let green = form.green();
    let key = FormKey {
        identity: std::ptr::from_ref(green) as usize,
        package: package.map(str::to_owned),
        line: LineEdges {
            leading_column,
            previous_whitespace: before.is_some(),
            next_whitespace: after.is_some(),
        },
        preceding_macros,
    };
    Some((form, key))
}

fn project(diagnostic: &mut LintDiagnostic, mut map: impl FnMut(TextRange) -> TextRange) {
    diagnostic.range = map(diagnostic.range);
    for related in &mut diagnostic.related {
        related.range = map(related.range);
    }
    if let Some(fix) = &mut diagnostic.fix {
        for edit in &mut fix.edits {
            edit.range = map(edit.range);
        }
    }
}

fn diagnostic_bytes(diagnostic: &LintDiagnostic) -> usize {
    let mut bytes = size_of::<LintDiagnostic>() + diagnostic.message.len();
    bytes += diagnostic.help.as_ref().map_or(0, String::len);
    bytes += diagnostic.tags.len() * size_of::<super::LintTag>();
    for related in &diagnostic.related {
        bytes += size_of::<super::LintRelated>() + related.message.len();
        bytes += related.document.as_ref().map_or(0, String::len);
    }
    if let Some(fix) = &diagnostic.fix {
        bytes += fix.title.len();
        for edit in &fix.edits {
            bytes += size_of::<crate::TextEdit>() + edit.replacement.len();
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceWorkspace;
    use crate::lint::LintPreset;

    #[test]
    fn lexical_memo_projects_diagnostics_and_rechecks_changed_environment() {
        let mut workspace = SourceWorkspace::new();
        let library = "(in-package :cl-user)\n(defun helper (x) x)\n";
        let original = "; leading comment\n(in-package :cl-user)\n\
                        (defun checked (used ignored)\n  (declare (ignore ignored))\n\
                          (let ((unused 1)) (if ignored used 2)))\n\
                        (defun edited (x) (helper (+ x 7)))\n";
        workspace.set_document("library", library);
        workspace.set_document("source", original);
        let mut cache = FormLintCache::default();
        let config = LintConfig::from_preset(LintPreset::Strict);
        let check = |workspace: &SourceWorkspace, cache: &mut FormLintCache| {
            let snapshot = workspace.diagnostic_snapshot("source").unwrap();
            let cached = snapshot
                .lint_with_cache(&config, || false, Some(cache))
                .unwrap();
            assert_eq!(Some(&cached), snapshot.lint(&config, || false).as_ref());
            cached
        };

        let original_diagnostics = check(&workspace, &mut cache);
        assert!(original_diagnostics.iter().any(|d| d.fix.is_some()));
        assert!(original_diagnostics.iter().any(|d| !d.related.is_empty()));
        assert_eq!(cache.forms.len(), 2);
        let cached_checked = cache
            .forms
            .values()
            .find(|entry| !entry.diagnostics.is_empty())
            .unwrap()
            .clone();
        let comment = original.find("leading").unwrap();
        workspace
            .apply_edit("source", comment..comment, "😀 ")
            .unwrap();
        let shifted = check(&workspace, &mut cache);
        assert!(
            cache
                .forms
                .values()
                .any(|entry| Arc::ptr_eq(entry, &cached_checked))
        );
        assert_eq!(original_diagnostics.len(), shifted.len());
        assert!(
            original_diagnostics
                .iter()
                .zip(&shifted)
                .all(|(old, new)| { old.range + TextSize::from(5) == new.range })
        );

        workspace.set_document("library", format!("{library}(declaim (special unused))"));
        let changed = check(&workspace, &mut cache);
        assert!(
            cache
                .forms
                .values()
                .all(|entry| !Arc::ptr_eq(entry, &cached_checked))
        );
        assert!(changed.iter().all(|d| d.rule_id != "style/unused-variable"));

        let snapshot = workspace.diagnostic_snapshot("source").unwrap();
        assert!(
            snapshot
                .lint_with_cache(&config, || true, Some(&mut cache))
                .is_none()
        );
        check(&workspace, &mut cache);
    }

    #[test]
    fn unchanged_call_rechecks_when_local_macro_moves_before_it() {
        let prefix = "(defpackage :memo (:use :cl) (:local-nicknames (:core :common-lisp)))\n(in-package :memo)\n";
        let caller = "(core:defun caller (items) (with-local-body (mapcar #'print items)))\n";
        let template = "(defmacro with-local-body (&body body) `(progn ,@body :done))\n";
        let mut workspace = SourceWorkspace::new();
        let original = format!("{prefix}{caller}{template}");
        workspace.set_document("source", original.clone());
        let config = LintConfig::default();
        let mut cache = FormLintCache::default();
        let snapshot = workspace.diagnostic_snapshot("source").unwrap();
        let before = snapshot
            .lint_with_cache(&config, || false, Some(&mut cache))
            .unwrap();
        assert!(
            before
                .iter()
                .all(|d| d.rule_id != "perf/mapcar-result-discarded")
        );
        let caller_key = cache.forms.keys().next().unwrap().identity;

        workspace
            .apply_edit("source", prefix.len()..prefix.len(), template)
            .unwrap();
        // Materialize the intermediate revision so the removal retains the
        // caller's actual green node rather than reparsing both changed ranges.
        workspace.snapshot("source").unwrap().parsed();
        let trailing = prefix.len() + template.len() + caller.len();
        workspace
            .apply_edit("source", trailing..trailing + template.len(), "")
            .unwrap();
        let snapshot = workspace.diagnostic_snapshot("source").unwrap();
        let after = snapshot
            .lint_with_cache(&config, || false, Some(&mut cache))
            .unwrap();
        assert_eq!(after, snapshot.lint(&config, || false).unwrap());
        assert!(
            after
                .iter()
                .any(|d| d.rule_id == "perf/mapcar-result-discarded")
        );
        // These declaration orders have equal semantic inputs except for the
        // latest version's macro being textually available at this call site.
        assert!(
            snapshot
                .source()
                .parsed()
                .root()
                .children()
                .any(|node| { std::ptr::from_ref(node.green()) as usize == caller_key })
        );
    }
}
