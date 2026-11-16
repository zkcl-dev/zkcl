use std::collections::HashMap;
use std::sync::Arc;

use crate::ast::tokens_at_offset;
use crate::cl_builtins::{BuiltinKind, builtin_kind, is_external_cl_symbol};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SyntaxKind, SyntaxNode, TextRange, TextSize, parse_source_with_readtable};

use super::index::{DocumentIndex, WorkspaceDeclarations, WorkspaceIndex};
use super::queries::candidate_symbol_tokens;
use super::references::{IndexedDocument, ReferenceTarget};
use super::{Namespace, OccurrenceBinding, SourceLocation};

type RenameEdit = (SourceLocation, String);

pub(crate) fn prepare_rename<'a>(
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    documents: impl IntoIterator<Item = (&'a str, &'a DocumentIndex)>,
) -> Option<(TextRange, String)> {
    let workspace = WorkspaceIndex::new(documents);
    let occurrence = match renamable_target(source, root, offset, &workspace)? {
        OccurrenceBinding::LocalSite {
            occurrence_range, ..
        }
        | OccurrenceBinding::Global {
            occurrence_range, ..
        } => occurrence_range,
        OccurrenceBinding::LocalWithoutSite => return None,
    };
    let token = tokens_at_offset(root, occurrence.start()).right_biased()?;
    let name_start = symbol_name_start(token.text());
    let start = occurrence.start() + TextSize::try_from(name_start).ok()?;
    Some((
        TextRange::new(start, occurrence.end()),
        token.text()[name_start..].to_owned(),
    ))
}

fn renamable_target(
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    workspace: &WorkspaceIndex<'_>,
) -> Option<OccurrenceBinding> {
    let mut target = source.resolve_occurrence_at(root, offset, workspace)?;
    if let OccurrenceBinding::LocalSite {
        namespace: Namespace::Block,
        def_range,
        occurrence_range,
        ..
    } = &target
        && let Some(OccurrenceBinding::Global {
            name,
            is_declaration: true,
            ..
        }) = source.resolve_occurrence_at(root, def_range.start(), workspace)
    {
        target = OccurrenceBinding::Global {
            name,
            occurrence_range: *occurrence_range,
            is_declaration: false,
        };
    }
    match &target {
        OccurrenceBinding::LocalSite {
            namespace: Namespace::ImplicitBlock | Namespace::IntegerTag | Namespace::SpecialValue,
            ..
        } => None,
        OccurrenceBinding::LocalSite {
            namespace: Namespace::Block | Namespace::Tag,
            spelling,
            package,
            ..
        } if spelling.name == "NIL"
            && workspace.symbol_home_package(spelling, package.as_deref())
                == Some("COMMON-LISP") =>
        {
            None
        }
        OccurrenceBinding::LocalSite { .. } => Some(target),
        OccurrenceBinding::Global { name, .. } => {
            let (_, declaration) = workspace.lookup_unique_declaration(name)?;
            (declaration.renamable && workspace.declaration_package(declaration)? != "COMMON-LISP")
                .then_some(target)
        }
        OccurrenceBinding::LocalWithoutSite => None,
    }
}

pub(crate) fn rename<'a>(
    source_doc: &str,
    source: &DocumentIndex,
    root: &SyntaxNode,
    offset: TextSize,
    new_name: &str,
    features: Option<&[String]>,
    documents: impl IntoIterator<Item = IndexedDocument<'a>>,
) -> Option<Vec<RenameEdit>> {
    let new_spelling = SymbolName::parse(new_name)?;
    if new_spelling.prefix != Prefix::None {
        return None;
    }
    let documents: Vec<_> = documents.into_iter().collect();
    let source_by_name: HashMap<_, _> = documents
        .iter()
        .map(|&(name, root, index)| (name, (root, index)))
        .collect();
    let indexed: Vec<_> = documents
        .iter()
        .map(|&(name, _, index)| (name, index))
        .collect();
    let declarations = Arc::new(WorkspaceDeclarations::collect(&indexed, &|| false)?);
    let workspace = WorkspaceIndex::new(indexed).with_declarations(declarations);
    let target = renamable_target(source, root, offset, &workspace)?;
    let (definition_doc, definition_root, definition_index, search_offset, old_spelling, home) =
        match &target {
            OccurrenceBinding::LocalSite { spelling, .. } => {
                (source_doc, root, source, offset, &spelling.name, None)
            }
            OccurrenceBinding::Global { name, .. } => {
                let (document, declaration) = workspace.lookup_unique_declaration(name)?;
                let &(root, index) = source_by_name.get(document)?;
                (
                    document,
                    root,
                    index,
                    declaration.range.start(),
                    &name.spelling.name,
                    workspace.declaration_package(declaration),
                )
            }
            OccurrenceBinding::LocalWithoutSite => return None,
        };
    let mut locations = ReferenceTarget::at(
        definition_doc,
        definition_index,
        definition_root,
        search_offset,
        &workspace,
    )?
    .locations(&workspace, documents.iter().copied(), true);
    let definition = locations.first()?.clone();
    if home.is_some() {
        // A named function body has an implicit BLOCK at the function name.
        // Its references have their own namespace but share the declaration edit.
        locations.extend(
            candidate_symbol_tokens(definition_root).filter_map(|token| {
                if SymbolName::parse(token.text())?.name != *old_spelling {
                    return None;
                }
                let range = token.text_range();
                let target = definition_index.resolve_occurrence_at(
                    definition_root,
                    range.start(),
                    &workspace,
                )?;
                if let OccurrenceBinding::LocalSite {
                    namespace: Namespace::Block,
                    def_range,
                    occurrence_range,
                    ..
                } = target
                    && def_range == definition.range
                    && occurrence_range == range
                {
                    return Some(SourceLocation {
                        document: definition_doc.to_owned(),
                        range,
                    });
                }
                None
            }),
        );
    }
    let mut edits = Vec::with_capacity(locations.len());
    for loc in locations {
        let &(doc_root, doc_index) = source_by_name.get(loc.document.as_str())?;
        let token = tokens_at_offset(doc_root, loc.range.start()).right_biased()?;
        let replacement = replace_symbol_name(token.text(), new_name);
        if replacement != token.text() {
            if loc == definition && *old_spelling != new_spelling.name {
                let spelling = SymbolName::parse(&replacement)?;
                let package = doc_index.package_at(loc.range.start());
                if would_rebind_standard_name(&target, &spelling, package, &workspace) {
                    return None;
                }
            }
            edits.push((loc, replacement));
        }
    }
    let code_edits = edits.len();
    if let Some(home) = home
        && *old_spelling != new_spelling.name
    {
        add_package_edits(
            &mut edits,
            &documents,
            &workspace,
            home,
            old_spelling,
            new_name,
            &new_spelling.name,
        )?;
    }
    if edits.is_empty() {
        return Some(edits);
    }
    validate_rename(
        &documents,
        &workspace,
        &edits,
        code_edits,
        &definition,
        (home.is_some(), [old_spelling, &new_spelling.name]),
        features,
    )?;
    Some(edits)
}

fn would_rebind_standard_name(
    target: &OccurrenceBinding,
    spelling: &SymbolName,
    package: Option<&str>,
    workspace: &WorkspaceIndex<'_>,
) -> bool {
    if !is_external_cl_symbol(&spelling.name) {
        return false;
    }
    let is_standard = match &spelling.prefix {
        Prefix::Package { name, .. } => {
            workspace.resolve_package_prefix(package, name) == Some("COMMON-LISP")
        }
        Prefix::None => {
            workspace.symbol_home_package(spelling, package) == Some("COMMON-LISP")
                || package
                    .and_then(|name| workspace.canonical_global_package(name))
                    .is_some_and(|name| {
                        let uses_common_lisp =
                            workspace
                                .package(name)
                                .map_or(name == "COMMON-LISP-USER", |package| {
                                    package.uses.iter().any(|used| {
                                        workspace.canonical_global_package(used)
                                            == Some("COMMON-LISP")
                                    })
                                });
                        uses_common_lisp && workspace.standard_symbol(spelling, package).is_some()
                    })
        }
        Prefix::Keyword | Prefix::Uninterned => false,
    };
    if !is_standard {
        return false;
    }
    match target {
        OccurrenceBinding::LocalSite {
            namespace: Namespace::Value,
            ..
        } => matches!(
            builtin_kind(&spelling.name),
            Some(BuiltinKind::Variable | BuiltinKind::Constant)
        ),
        OccurrenceBinding::LocalSite {
            namespace: Namespace::Function | Namespace::SetfFunction,
            ..
        } => matches!(
            builtin_kind(&spelling.name),
            Some(BuiltinKind::Function | BuiltinKind::Macro | BuiltinKind::SpecialOperator)
        ),
        OccurrenceBinding::Global { .. } => true,
        _ => false,
    }
}

fn add_package_edits(
    edits: &mut Vec<RenameEdit>,
    documents: &[IndexedDocument<'_>],
    workspace: &WorkspaceIndex<'_>,
    home: &str,
    old_name: &str,
    new_spelling: &str,
    new_name: &str,
) -> Option<()> {
    for &(document, root, index) in documents {
        for package in &index.packages {
            for name in &package.symbol_designators {
                if name.name != old_name {
                    continue;
                }
                let lookup_package = name.import_source.as_deref().unwrap_or(&package.name);
                let package_home =
                    workspace
                        .canonical_global_package(lookup_package)
                        .and_then(|package| {
                            workspace.resolve_symbol_home_in_package(package, old_name, false)
                        });
                if package_home != Some(home) {
                    continue;
                }
                let token = tokens_at_offset(root, name.range.start()).right_biased()?;
                let replacement = match token.kind() {
                    SyntaxKind::String | SyntaxKind::RawString | SyntaxKind::Character => {
                        quote_lisp_string(new_name)
                    }
                    SyntaxKind::Atom
                        if matches!(
                            SymbolName::parse(token.text())?.prefix,
                            Prefix::Package { .. }
                        ) =>
                    {
                        // DEFPACKAGE uses the symbol name as a string designator.
                        format!("#:{new_spelling}")
                    }
                    _ => replace_symbol_name(token.text(), new_spelling),
                };
                edits.push((
                    SourceLocation {
                        document: document.to_owned(),
                        range: name.range,
                    },
                    replacement,
                ));
            }
        }
    }
    Some(())
}

fn quote_lisp_string(name: &str) -> String {
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for c in name.chars() {
        if matches!(c, '"' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

fn replace_symbol_name(original: &str, new_spelling: &str) -> String {
    format!("{}{new_spelling}", &original[..symbol_name_start(original)])
}

fn symbol_name_start(original: &str) -> usize {
    if original.starts_with("#:") {
        return 2;
    }
    let mut escaped = false;
    let mut chars = original.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '\\' => {
                chars.next();
            }
            '|' => escaped = !escaped,
            ':' if !escaped => {
                let internal = chars.next_if(|&(_, next)| next == ':').is_some();
                return index + 1 + usize::from(internal);
            }
            _ => {}
        }
    }
    0
}

struct EditedDocument {
    root: SyntaxNode,
    index: DocumentIndex,
    ranges: Vec<(TextRange, TextRange)>,
}

impl EditedDocument {
    fn new(
        root: &SyntaxNode,
        index: &DocumentIndex,
        mut edits: Vec<&RenameEdit>,
        features: Option<&[String]>,
    ) -> Option<Self> {
        edits.sort_unstable_by_key(|(loc, _)| loc.range.start());
        let original = root.text().to_string();
        let mut revised = String::with_capacity(original.len());
        let mut ranges = Vec::with_capacity(edits.len());
        let mut previous = 0;
        for (loc, replacement) in edits {
            let start = usize::from(loc.range.start());
            revised.push_str(original.get(previous..start)?);
            let revised_start = TextSize::try_from(revised.len()).ok()?;
            revised.push_str(replacement);
            let revised_end = TextSize::try_from(revised.len()).ok()?;
            ranges.push((loc.range, TextRange::new(revised_start, revised_end)));
            previous = usize::from(loc.range.end());
        }
        revised.push_str(original.get(previous..)?);
        TextSize::try_from(revised.len()).ok()?;
        let root = parse_source_with_readtable(&revised, index.initial_readtable).root();
        let index = DocumentIndex::collect(
            &root,
            index.initial_package(),
            features,
            index.initial_readtable,
        );
        Some(Self {
            root,
            index,
            ranges,
        })
    }

    fn offset(&self, before: TextSize) -> TextSize {
        let prior = self
            .ranges
            .partition_point(|(range, _)| range.end() <= before);
        self.ranges[..prior]
            .last()
            .map_or(before, |(old, new)| new.end() + (before - old.end()))
    }
}

fn validate_rename(
    documents: &[IndexedDocument<'_>],
    workspace: &WorkspaceIndex<'_>,
    edits: &[RenameEdit],
    code_edits: usize,
    definition: &SourceLocation,
    (global, spellings): (bool, [&str; 2]),
    features: Option<&[String]>,
) -> Option<()> {
    let mut by_document: HashMap<&str, Vec<&RenameEdit>> = HashMap::new();
    for edit in edits {
        by_document.entry(&edit.0.document).or_default().push(edit);
    }
    let edited: HashMap<_, _> = documents
        .iter()
        .filter_map(|&(name, root, index)| {
            let edits = by_document.remove(name)?;
            Some(EditedDocument::new(root, index, edits, features).map(|document| (name, document)))
        })
        .collect::<Option<_>>()?;
    let revised = WorkspaceIndex::new(documents.iter().map(|&(name, _, index)| {
        (
            name,
            edited.get(name).map_or(index, |document| &document.index),
        )
    }));
    let [old, new] = spellings;
    if global
        && old != new
        && workspace.packages().any(|package| {
            let name = package.name.as_str();
            workspace
                .resolve_symbol_home_in_package(name, new, false)
                .is_some()
                && revised
                    .resolve_symbol_home_in_package(name, new, false)
                    .is_none()
        })
    {
        return None;
    }
    let definition_doc = definition.document.as_str();
    let definition_offset = definition.range.start();
    let expected = (
        definition_doc,
        edited
            .get(definition_doc)
            .map_or(definition_offset, |doc| doc.offset(definition_offset)),
    );
    for (location, _) in &edits[..code_edits] {
        let document = &edited[location.document.as_str()];
        let offset = document.offset(location.range.start());
        let target = document
            .index
            .resolve_occurrence_at(&document.root, offset, &revised);
        if binding_at(location.document.as_str(), target, &revised) != Some(expected) {
            return None;
        }
    }

    // An existing reference to the new name can be captured by the renamed
    // binding, even when all of the explicit rename edits resolve correctly.
    for &(name, root, index) in documents {
        if !global && name != definition_doc {
            continue;
        }
        let changed = edited.get(name);
        let (new_root, new_index) = changed.map_or((root, index), |doc| (&doc.root, &doc.index));
        for token in candidate_symbol_tokens(root) {
            let Some(spelling) = SymbolName::parse(token.text()) else {
                continue;
            };
            if !spellings.contains(&spelling.name.as_str()) {
                continue;
            }
            let range = token.text_range();
            if changed.is_some_and(|doc| {
                doc.ranges
                    .binary_search_by_key(&range.start(), |(r, _)| r.start())
                    .is_ok()
            }) {
                continue;
            }
            let before = range.start();
            let after = changed.map_or(before, |doc| doc.offset(before));
            if matches!(spelling.prefix, Prefix::Package { external: true, .. })
                && workspace
                    .symbol_home_package(&spelling, index.package_at(before))
                    .is_some()
                && revised
                    .symbol_home_package(&spelling, new_index.package_at(after))
                    .is_none()
            {
                return None;
            }
            let old_binding = binding_at(
                name,
                index.resolve_occurrence_at(root, before, workspace),
                workspace,
            );
            let expected_binding = old_binding.map(|(doc, offset)| {
                (
                    doc,
                    edited
                        .get(doc)
                        .map_or(offset, |edited_doc| edited_doc.offset(offset)),
                )
            });
            let new_binding = binding_at(
                name,
                new_index.resolve_occurrence_at(new_root, after, &revised),
                &revised,
            );
            if expected_binding != new_binding {
                return None;
            }
        }
    }
    Some(())
}

fn binding_at<'a>(
    document: &'a str,
    target: Option<OccurrenceBinding>,
    workspace: &WorkspaceIndex<'a>,
) -> Option<(&'a str, TextSize)> {
    match target? {
        OccurrenceBinding::LocalSite { def_range, .. } => Some((document, def_range.start())),
        OccurrenceBinding::Global { name, .. } => {
            let (document, def) = workspace.lookup_unique_declaration(&name)?;
            Some((document, def.range.start()))
        }
        OccurrenceBinding::LocalWithoutSite => None,
    }
}
