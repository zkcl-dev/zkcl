//! Relative diagnostics for complete, independently scoped forms.

use std::sync::Arc;

use rowan::GreenNode;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::navigation::Name;
use crate::navigation::index::{DocumentIndex, closed_list, symbol};
use crate::outline::elements;
use crate::reader_tokens::SymbolName;
use crate::syntax::{SourceReadtable, SyntaxKind, SyntaxNode, TextRange, TextSize};

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
    /// Unqualified spellings the form contains. A cached result can only
    /// consult facts of a name the form mentions, so a change to any other
    /// name's callable facts leaves this result reusable.
    spellings: Box<[String]>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct NamespaceInputs {
    packages: Vec<PackageInputs>,
    declarations: FxHashMap<String, Vec<DeclarationInput>>,
    source_macros: Vec<SourceMacroInputs>,
    source_macro_dependencies: FxHashSet<String>,
    specials: Vec<(Name, SymbolName, SymbolName)>,
    guarded_callables: Vec<Name>,
    initial_readtable: SourceReadtable,
    bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct DeclarationInput {
    name: Name,
    operator: SymbolName,
    evaluates_arguments: bool,
    params: Option<Arc<[String]>>,
}

#[derive(Debug, PartialEq, Eq)]
struct SourceMacroInputs {
    name: Name,
    source: GreenNode,
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
        let mut declarations: FxHashMap<String, Vec<DeclarationInput>> = FxHashMap::default();
        let mut source_macros = Vec::new();
        let mut source_macro_dependencies = FxHashSet::default();
        for declaration in &index.declarations {
            if let Some(definition) = &declaration.macro_definition {
                let root = SyntaxNode::new_root(definition.source.clone());
                source_macro_dependencies.extend(mentioned_spellings(&root).iter().cloned());
                source_macros.push(SourceMacroInputs {
                    name: declaration.name.clone(),
                    source: definition.source.clone(),
                });
            }
            declarations
                .entry(declaration.name.spelling.name.clone())
                .or_default()
                .push(DeclarationInput {
                    name: declaration.name.clone(),
                    operator: declaration.operator.clone(),
                    evaluates_arguments: declaration.evaluates_arguments,
                    params: declaration.params.clone(),
                });
        }
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
            declarations,
            source_macros,
            source_macro_dependencies,
            specials: index.special_proclamations.clone(),
            guarded_callables: index.guarded_callables.clone(),
            initial_readtable: index.initial_readtable(),
            bytes: 0,
        };
        inputs.bytes = inputs
            .declarations
            .iter()
            .map(|(spelling, facts)| {
                size_of::<String>()
                    + spelling.len()
                    + facts.iter().map(DeclarationInput::bytes).sum::<usize>()
            })
            .sum::<usize>()
            + inputs
                .source_macros
                .iter()
                .map(SourceMacroInputs::bytes)
                .sum::<usize>()
            + inputs
                .source_macro_dependencies
                .iter()
                .map(|spelling| size_of::<String>() + spelling.len())
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

    /// Whether the facts every cached form can read are unchanged: package
    /// structure, initial reader syntax, SPECIAL proclamations, guarded
    /// callables and source macro definitions. These are not attributable to
    /// one spelling, so a change in any of them rechecks the whole document.
    fn workspace_facts_match(&self, previous: &Self) -> bool {
        self.packages == previous.packages
            && self.initial_readtable == previous.initial_readtable
            && self.specials == previous.specials
            && self.guarded_callables == previous.guarded_callables
            && self.source_macros == previous.source_macros
    }
}

impl DeclarationInput {
    fn bytes(&self) -> usize {
        size_of::<Self>()
            + name_bytes(&self.name)
            + symbol_bytes(&self.operator)
            + self.params.as_ref().map_or(0, |params| {
                params
                    .iter()
                    .map(|parameter| size_of::<String>() + parameter.len())
                    .sum()
            })
    }
}

impl SourceMacroInputs {
    fn bytes(&self) -> usize {
        size_of::<Self>()
            + name_bytes(&self.name)
            + usize::from(self.source.text_len()).saturating_mul(8)
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
    /// Spellings consulted indirectly through source macro templates in the
    /// workspace captured by the retained form results.
    ///
    /// `None` means the cache has no retained workspace inputs, so callers
    /// must conservatively assume an indirect macro dependency may exist.
    pub fn source_macro_dependencies(&self) -> Option<impl Iterator<Item = &str>> {
        (!self.environment.is_empty()).then(|| {
            self.environment
                .iter()
                .flat_map(|(_, inputs)| inputs.source_macro_dependencies.iter().map(String::as_str))
        })
    }

    pub(super) fn check(
        &mut self,
        document: &LintDocument<'_>,
        emitter: &mut LintEmitter<'_>,
        mut analyze: impl FnMut(&SyntaxNode, &mut LintEmitter<'_>) -> Option<()>,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Option<Vec<crate::outline::Element>>> {
        let documents = &document.workspace.documents;
        let (environment_change, outdated_environment) =
            compare_environment(&self.environment, documents);
        let environment_bytes: usize = outdated_environment
            .as_ref()
            .unwrap_or(&self.environment)
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
        let features_match = self.features.as_deref() == features;
        let config_matches = self.config.as_ref() == Some(emitter.config);
        if !config_matches
            || !features_match
            || matches!(environment_change, EnvironmentChange::Everything)
        {
            self.forms.clear();
        }
        if !features_match {
            self.features = features.map(<[String]>::to_vec);
        }
        if !config_matches {
            self.config = Some(emitter.config.clone());
        }
        // A retained result survives an environment change that only touched
        // callable facts of names the form does not mention.
        let reusable = |entry: &Arc<FormDiagnostics>| match &environment_change {
            EnvironmentChange::Same => true,
            EnvironmentChange::Names(changed) => !entry
                .spellings
                .iter()
                .any(|spelling| changed.contains(spelling.as_str())),
            EnvironmentChange::Everything => false,
        };

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
            let entry = current
                .get(&key)
                .or_else(|| self.forms.get(&key))
                .filter(|entry| reusable(entry));
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
                let spellings = mentioned_spellings(form);
                bytes = bytes.saturating_add(
                    spellings
                        .iter()
                        .map(|spelling| size_of::<String>() + spelling.len())
                        .sum::<usize>(),
                );
                if bytes > remaining {
                    continue;
                }
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
                        spellings,
                    }),
                );
            }
        }
        // Cancellation while rebuilding must leave the retained forms paired
        // with the environment they were checked against.
        if let Some(environment) = outdated_environment {
            self.environment = environment;
        }
        self.forms = current;
        Some(Some(rest))
    }
}

/// What the difference between two revisions of the visible workspace allows
/// retained form results to rely on.
enum EnvironmentChange {
    /// Every visible document contributes the inputs of the last check.
    Same,
    /// Only these unqualified spellings changed their callable facts.
    Names(FxHashSet<String>),
    /// A fact every cached form can read changed. Retained results are void.
    Everything,
}

/// Compare the workspace inputs of this check with the ones the retained
/// results were computed from, and return the current inputs when they differ.
fn compare_environment(
    previous: &[(String, Arc<NamespaceInputs>)],
    documents: &[(&str, &DocumentIndex)],
) -> (EnvironmentChange, Option<Vec<(String, Arc<NamespaceInputs>)>>) {
    let rebuild = || {
        documents
            .iter()
            .map(|&(name, document)| {
                (
                    name.to_owned(),
                    Arc::clone(document.form_lint_inputs()),
                )
            })
            .collect()
    };
    if previous.len() != documents.len()
        || previous
            .iter()
            .zip(documents)
            .any(|((name, _), &(next, _))| name.as_str() != next)
    {
        return (EnvironmentChange::Everything, Some(rebuild()));
    }
    let mut everything = false;
    let mut changed_spellings: Option<FxHashSet<String>> = None;
    let mut source_macro_dependencies: FxHashSet<&str> = FxHashSet::default();
    let mut outdated = false;
    for ((_, recorded), &(_, document)) in previous.iter().zip(documents) {
        let current = document.form_lint_inputs();
        source_macro_dependencies.extend(
            recorded
                .source_macro_dependencies
                .iter()
                .map(String::as_str),
        );
        source_macro_dependencies
            .extend(current.source_macro_dependencies.iter().map(String::as_str));
        // Body-only edits can rebuild the document index with identical
        // namespace facts. Keep the existing proof in that case.
        if Arc::ptr_eq(recorded, current) || recorded == current {
            continue;
        }
        outdated = true;
        if everything {
            continue;
        }
        match declaration_change(recorded, current) {
            Some(changed) => {
                changed_spellings
                    .get_or_insert_with(FxHashSet::default)
                    .extend(changed);
            }
            None => everything = true,
        }
    }
    let change = if everything {
        EnvironmentChange::Everything
    } else {
        match changed_spellings {
            Some(changed)
                if source_macro_dependencies
                    .iter()
                    .any(|spelling| changed.contains(*spelling)) =>
            {
                EnvironmentChange::Everything
            }
            Some(changed) => EnvironmentChange::Names(changed),
            None => EnvironmentChange::Same,
        }
    };
    (change, outdated.then(rebuild))
}

/// Spellings whose callable facts differ between two revisions, or `None` when
/// a fact that every form can read differs.
fn declaration_change(
    previous: &NamespaceInputs,
    current: &NamespaceInputs,
) -> Option<FxHashSet<String>> {
    if !current.workspace_facts_match(previous) {
        return None;
    }
    let mut changed = FxHashSet::default();
    for (spelling, current) in &current.declarations {
        if previous.declarations.get(spelling) != Some(current) {
            changed.insert(spelling.clone());
        }
    }
    for spelling in previous.declarations.keys() {
        if !current.declarations.contains_key(spelling) {
            changed.insert(spelling.clone());
        }
    }
    Some(changed)
}

/// Unqualified uppercase spellings the form contains, sorted and deduplicated.
///
/// Retained diagnostics lie entirely inside their form, so a check can consult
/// a declaration, SPECIAL proclamation or guarded callable only through a name
/// this set contains.
fn mentioned_spellings(form: &SyntaxNode) -> Box<[String]> {
    let mut spellings: Vec<String> = form
        .descendants_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == SyntaxKind::Atom)
        .filter_map(|token| SymbolName::parse(token.text()).map(|symbol| symbol.name))
        .collect();
    spellings.sort_unstable();
    spellings.dedup();
    spellings.into_boxed_slice()
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
    use crate::navigation::index::WorkspaceIndex;

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
    fn signature_edits_recheck_only_forms_that_name_the_changed_function() {
        let original = "(in-package :cl-user)\n\
                        (defun target (x) x)\n\
                        (defun caller (y) (target y))\n\
                        (defun bystander (z) (+ z 1))\n\
                        (defmacro unchanged (form) `(progn ,form))\n";
        let mut workspace = SourceWorkspace::new();
        workspace.set_document("source", original);
        let config = LintConfig::from_preset(LintPreset::Strict);
        let mut cache = FormLintCache::default();
        let check = |workspace: &SourceWorkspace, cache: &mut FormLintCache| {
            let snapshot = workspace.diagnostic_snapshot("source").unwrap();
            let cached = snapshot
                .lint_with_cache(&config, || false, Some(cache))
                .unwrap();
            assert_eq!(Some(&cached), snapshot.lint(&config, || false).as_ref());
            cached
        };
        let reused = |retained: &[Arc<FormDiagnostics>], cache: &FormLintCache| {
            retained
                .iter()
                .filter(|entry| cache.forms.values().any(|kept| Arc::ptr_eq(kept, entry)))
                .count()
        };

        check(&workspace, &mut cache);
        let retained: Vec<_> = cache.forms.values().cloned().collect();
        assert_eq!(retained.len(), 3);

        // Widening the parameter list changes only TARGET's callable facts, so
        // every form but the one that never names it keeps its result.
        let parameter = original.find("(x) x").unwrap() + 1;
        workspace
            .apply_edit("source", parameter..parameter + 1, "x y")
            .unwrap();

        // Cancel after comparing the environments, before rechecking any
        // forms. The next check must still invalidate TARGET's old callers.
        let edited = workspace.snapshot("source").unwrap();
        let root = edited.parsed().root();
        let index = edited.index();
        let visible = WorkspaceIndex::new([("source", index)]);
        let document = LintDocument {
            text: edited.text(),
            root: &root,
            source: index,
            workspace: &visible,
            macro_operands: Default::default(),
        };
        let mut suppressions = super::super::suppressions::SuppressionTable::default();
        let mut emitter = LintEmitter {
            config: &config,
            suppressions: &mut suppressions,
            diagnostics: Vec::new(),
        };
        assert!(
            cache
                .check(&document, &mut emitter, |_, _| unreachable!(), &|| true)
                .is_none()
        );

        check(&workspace, &mut cache);
        assert_eq!(
            reused(&retained, &cache),
            1,
            "only the form that never names TARGET is reusable"
        );

        // A SPECIAL proclamation is a workspace-wide fact: nothing survives.
        let retained: Vec<_> = cache.forms.values().cloned().collect();
        workspace
            .apply_edit("source", 0..0, "(declaim (special z))\n")
            .unwrap();
        check(&workspace, &mut cache);
        assert_eq!(reused(&retained, &cache), 0);
    }

    #[test]
    fn repeated_declarations_do_not_hide_changed_callable_facts() {
        let inputs = |form: &str| {
            let mut workspace = SourceWorkspace::new();
            workspace.set_document("source", form.repeat(2));
            NamespaceInputs::collect(workspace.snapshot("source").unwrap().index())
        };
        let before = inputs("(defun target (x) x)\n");
        let after = inputs("(defun target (x y) (list x y))\n");
        assert_eq!(
            declaration_change(&before, &after),
            Some(["TARGET".into()].into_iter().collect())
        );
    }

    #[test]
    fn unrelated_declaration_additions_and_removals_preserve_forms() {
        let original = "(in-package :cl-user)\n\
                        (defun target (x) x)\n\
                        (defun caller (y) (target y))\n\
                        (defun bystander (z) (+ z 1))\n";
        let declaration = "(defun added (value) value)\n";
        let mut workspace = SourceWorkspace::new();
        workspace.set_document("source", original);
        let config = LintConfig::from_preset(LintPreset::Strict);
        let mut cache = FormLintCache::default();
        let check = |workspace: &SourceWorkspace, cache: &mut FormLintCache| {
            let snapshot = workspace.diagnostic_snapshot("source").unwrap();
            let cached = snapshot
                .lint_with_cache(&config, || false, Some(cache))
                .unwrap();
            assert_eq!(Some(&cached), snapshot.lint(&config, || false).as_ref());
        };
        let retained = |cache: &FormLintCache, name: &str| {
            cache
                .forms
                .values()
                .find(|entry| {
                    entry
                        ._green
                        .to_string()
                        .contains(&format!("(defun {name} "))
                })
                .cloned()
                .unwrap_or_else(|| panic!("missing retained form for {name}"))
        };

        check(&workspace, &mut cache);
        let before: Vec<_> = ["target", "caller", "bystander"]
            .into_iter()
            .map(|name| (name, retained(&cache, name)))
            .collect();

        let insertion = original.len();
        workspace
            .apply_edit("source", insertion..insertion, declaration)
            .unwrap();
        check(&workspace, &mut cache);
        for (name, entry) in &before {
            assert!(
                Arc::ptr_eq(entry, &retained(&cache, name)),
                "adding an unrelated declaration rechecked {name}"
            );
        }

        workspace
            .apply_edit("source", insertion..insertion + declaration.len(), "")
            .unwrap();
        check(&workspace, &mut cache);
        for (name, entry) in &before {
            assert!(
                Arc::ptr_eq(entry, &retained(&cache, name)),
                "removing an unrelated declaration rechecked {name}"
            );
        }
    }

    #[test]
    fn declaration_fact_transitions_and_cross_document_macro_dependencies() {
        let inputs = |text: &str| {
            let mut workspace = SourceWorkspace::new();
            workspace.set_document("source", text);
            NamespaceInputs::collect(workspace.snapshot("source").unwrap().index())
        };
        let absent = inputs("");
        let unique = inputs("(defun target (x) x)\n");
        let ambiguous = inputs("(defun target (x) x)\n(defun target (x y) (list x y))\n");
        let expected = || Some(["TARGET".into()].into_iter().collect());

        assert_eq!(declaration_change(&absent, &unique), expected());
        assert_eq!(declaration_change(&unique, &ambiguous), expected());
        assert_eq!(declaration_change(&ambiguous, &unique), expected());
        assert_eq!(declaration_change(&unique, &absent), expected());

        let mut workspace = SourceWorkspace::new();
        workspace.set_document(
            "macros.lisp",
            "(defmacro checked (form) `(progn (target 1) ,form))\n",
        );
        workspace.set_document("dependency.lisp", "(defun target (x) x)\n");
        workspace.set_document("caller.lisp", "(defun caller (form) (checked form))\n");
        let names = ["macros.lisp", "dependency.lisp", "caller.lisp"];
        let before: Vec<_> = names
            .into_iter()
            .map(|name| {
                let source = workspace.snapshot(name).unwrap();
                let inputs = Arc::clone(source.index().form_lint_inputs());
                (name, source, inputs)
            })
            .collect();
        let recorded: Vec<_> = before
            .iter()
            .map(|(name, _, inputs)| ((*name).to_owned(), Arc::clone(inputs)))
            .collect();

        workspace.set_document("dependency.lisp", "(defun target (x y) (list x y))\n");
        let after: Vec<_> = names
            .into_iter()
            .map(|name| (name, workspace.snapshot(name).unwrap()))
            .collect();
        let documents: Vec<_> = after
            .iter()
            .map(|(name, source)| (*name, source.index()))
            .collect();
        let (change, outdated) = compare_environment(&recorded, &documents);
        assert!(matches!(change, EnvironmentChange::Everything));
        assert!(outdated.is_some());
    }

    #[test]
    fn readtable_changes_recheck_all_cached_forms() {
        let mut workspace = SourceWorkspace::new();
        workspace.set_document("source", "(defun untouched () \"raw\")\n");
        let before = NamespaceInputs::collect(workspace.snapshot("source").unwrap().index());
        assert!(
            workspace.set_initial_readtable("source", crate::syntax::SourceReadtable::RawStrings)
        );
        let after = NamespaceInputs::collect(workspace.snapshot("source").unwrap().index());
        assert_eq!(declaration_change(&before, &after), None);
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
