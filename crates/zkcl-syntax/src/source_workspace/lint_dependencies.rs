//! Inputs a document's static diagnostics can observe in other documents.

use rustc_hash::FxHashSet;

use crate::navigation::{Namespace, index::Declaration};
use crate::reader_tokens::{SymbolName, string_literal};
use crate::syntax::SyntaxKind;

use super::SourceSnapshot;
use zkcl_diagnostics::SourcePositionEncoding;

/// Changes to declarations or the package graph visible to source linting.
#[derive(Debug, Default)]
pub struct WorkspaceLintChanges {
    packages: bool,
    symbols: FxHashSet<String>,
}

impl WorkspaceLintChanges {
    /// Compare two revisions of the same document, including additions and removals.
    pub fn between(before: Option<&SourceSnapshot>, after: Option<&SourceSnapshot>) -> Self {
        let revisions = before.zip(after);
        if revisions.is_some_and(|(before, after)| std::ptr::eq(before, after)) {
            return Self::default();
        }
        let indices = [before, after].map(|source| source.map(SourceSnapshot::index));
        let [old_packages, new_packages] =
            indices.map(|idx| idx.map_or(&[][..], |idx| &idx.packages));
        if old_packages.len() != new_packages.len()
            || !old_packages.iter().zip(new_packages).all(|(old, new)| {
                old.name == new.name
                    && old.nicknames == new.nicknames
                    && old.exports == new.exports
                    && old.shadows == new.shadows
                    && old.uses == new.uses
                    && old.local_nicknames == new.local_nicknames
                    && old.imports == new.imports
            })
        {
            return Self {
                packages: true,
                ..Self::default()
            };
        }
        let [old_declarations, new_declarations] =
            indices.map(|idx| idx.map_or(&[][..], |idx| &idx.declarations));
        let [old_specials, new_specials] =
            indices.map(|idx| idx.map_or(&[][..], |idx| &idx.special_proclamations));
        let [old_guarded, new_guarded] =
            indices.map(|idx| idx.map_or(&[][..], |idx| &idx.guarded_callables));
        let same_projected_positions = |range: crate::syntax::TextRange| {
            revisions.is_none_or(|(before, after)| {
                [range.start(), range.end()].into_iter().all(|offset| {
                    let offset = usize::from(offset);
                    if after
                        .source()
                        .preserves_position_from(before.source(), offset)
                    {
                        return true;
                    }
                    [
                        SourcePositionEncoding::Utf8,
                        SourcePositionEncoding::Utf16,
                        SourcePositionEncoding::Utf32,
                    ]
                    .into_iter()
                    .all(|encoding| {
                        before.position_with_encoding(offset, encoding)
                            == after.position_with_encoding(offset, encoding)
                    })
                })
            })
        };
        let same_declaration = |old: &Declaration, new: &Declaration| {
            if old.name != new.name
                || old.operator != new.operator
                || old.evaluates_arguments != new.evaluates_arguments
                || old.params != new.params
                || old.range != new.range
            {
                return false;
            }
            match (
                old.macro_definition.as_deref(),
                new.macro_definition.as_deref(),
            ) {
                (None, None) => {}
                (Some(old), Some(new)) if old.offset == new.offset && old.source == new.source => {}
                _ => return false,
            }
            // These declarations can supply a related location in another document.
            let supplies_related_location = matches!(
                old.name.namespace,
                Namespace::Function | Namespace::SetfFunction
            ) || old.operator.name == "DEFCONSTANT";
            !supplies_related_location || same_projected_positions(old.range)
        };
        let mut changes = Self::default();
        if old_declarations.len() != new_declarations.len()
            || !old_declarations
                .iter()
                .zip(new_declarations)
                .all(|(old, new)| same_declaration(old, new))
        {
            let spellings: FxHashSet<_> = old_declarations
                .iter()
                .chain(new_declarations)
                .map(|decl| decl.name.spelling.name.as_str())
                .collect();
            for spelling in spellings {
                let [mut old, mut new] = indices.map(|idx| {
                    idx.into_iter()
                        .flat_map(|idx| idx.declarations_named(spelling))
                });
                let changed = loop {
                    match (old.next(), new.next()) {
                        (None, None) => break false,
                        (Some(before), Some(after)) if same_declaration(before, after) => {}
                        _ => break true,
                    }
                };
                if changed {
                    changes.symbols.insert(spelling.into());
                }
            }
        }
        if old_specials != new_specials {
            // Special proclamations affect the identity of the matching value
            // bindings. Overlapping declarations are still a sound invalidation.
            changes.symbols.extend(
                old_specials
                    .iter()
                    .chain(new_specials)
                    .map(|(name, _, _)| name.spelling.name.clone()),
            );
        }
        if old_guarded != new_guarded {
            changes.symbols.extend(
                old_guarded
                    .iter()
                    .chain(new_guarded)
                    .map(|name| name.spelling.name.clone()),
            );
        }
        changes
    }

    pub fn is_empty(&self) -> bool {
        !self.packages && self.symbols.is_empty()
    }

    pub fn merge(&mut self, other: Self) {
        self.packages |= other.packages;
        if self.packages {
            self.symbols.clear();
        } else {
            self.symbols.extend(other.symbols);
        }
    }

    /// Whether this document could observe the changes. The source footprint
    /// includes quoted and conditional occurrences and both directions of a
    /// package alias. Name binding itself remains the job of the lint pass.
    pub fn affects(&self, snapshot: &SourceSnapshot) -> bool {
        if self.packages {
            return true;
        }
        if self.symbols.is_empty() {
            return false;
        }
        let spellings = snapshot.lint_spellings.get_or_init(|| {
            let mut spellings = FxHashSet::default();
            let mut collect = |root: crate::syntax::SyntaxNode| {
                for token in root
                    .descendants_with_tokens()
                    .filter_map(|part| part.into_token())
                {
                    match token.kind() {
                        SyntaxKind::Atom => {
                            if let Some(name) = SymbolName::parse(token.text()) {
                                spellings.insert(name.name);
                                if let crate::reader_tokens::Prefix::Package { name, .. } =
                                    name.prefix
                                {
                                    spellings.insert(name);
                                }
                            }
                        }
                        SyntaxKind::String | SyntaxKind::RawString => {
                            if let Some(name) = string_literal(&token) {
                                spellings.insert(name);
                            }
                        }
                        _ => {}
                    }
                }
            };
            collect(snapshot.parsed().root());
            for example in &snapshot.documentation_tests().examples {
                collect(example.parsed().root());
            }
            spellings
        });
        self.symbols.iter().any(|name| spellings.contains(name))
    }
}
