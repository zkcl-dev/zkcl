use crate::outline::{Element, elements};

use super::{
    Namespace, OccurrenceBinding, ScopeAtOffset, ScopeBinding, SymbolName, SyntaxKind, SyntaxNode,
    TextRange, closed_list, symbol,
};

impl ScopeAtOffset<'_, '_> {
    pub(super) fn has_standard_head(&self, list: &SyntaxNode, name: &str) -> bool {
        elements(list)
            .next()
            .as_ref()
            .and_then(symbol)
            .is_some_and(|head| self.workspace.standard_symbol(&head, self.package) == Some(name))
    }

    pub(super) fn special_declarations(
        &self,
        body: impl IntoIterator<Item = Element>,
        docstring_allowed: bool,
    ) -> Vec<SymbolName> {
        let mut specials = Vec::new();
        let mut docstring_allowed = docstring_allowed;
        for form in body {
            if docstring_allowed
                && matches!(form.kind(), SyntaxKind::String | SyntaxKind::RawString)
            {
                docstring_allowed = false;
                continue;
            }
            let Some(list) = form
                .as_node()
                .filter(|list| closed_list(list) && self.has_standard_head(list, "DECLARE"))
            else {
                break;
            };
            for spec in elements(list)
                .skip(1)
                .filter_map(Element::into_node)
                .filter(closed_list)
            {
                if self.has_standard_head(&spec, "SPECIAL") {
                    specials.extend(
                        elements(&spec)
                            .skip(1)
                            .filter_map(|variable| symbol(&variable)),
                    );
                }
            }
        }
        specials
    }

    fn variable_namespace(&self, spelling: &SymbolName, specials: &[SymbolName]) -> Namespace {
        if specials
            .iter()
            .any(|special| self.symbols_match(special, self.package, spelling))
            || self.workspace.is_proclaimed_special(spelling, self.package)
        {
            Namespace::SpecialValue
        } else {
            Namespace::Value
        }
    }

    pub(super) fn variable_target(
        &self,
        spelling: SymbolName,
        range: TextRange,
        specials: &[SymbolName],
    ) -> OccurrenceBinding {
        self.local_target(
            self.variable_namespace(&spelling, specials),
            spelling,
            range,
            None,
        )
    }

    pub(super) fn variable_binding(
        &self,
        spelling: SymbolName,
        range: TextRange,
        specials: &[SymbolName],
    ) -> ScopeBinding {
        self.scope_binding(
            self.variable_namespace(&spelling, specials),
            spelling,
            range,
        )
    }

    pub(super) fn add_free_specials(
        &self,
        scope: &mut Vec<ScopeBinding>,
        binding_start: usize,
        specials: &[SymbolName],
    ) {
        for special in specials {
            if scope[binding_start..].iter().any(|binding| {
                binding.namespace == Namespace::SpecialValue && self.same_symbol(binding, special)
            }) {
                continue;
            }
            scope.push(ScopeBinding {
                namespace: Namespace::SpecialValue,
                spelling: special.clone(),
                package: self.package.map(str::to_owned),
                def_range: None,
                params: None,
                evaluates_arguments: true,
            });
        }
    }

    pub(crate) fn top_level_specials(&self, element: &Element) -> Vec<ScopeBinding> {
        let mut body_scopes = Vec::new();
        if let Some(parent) = element.parent() {
            for node in parent
                .ancestors()
                .take_while(|node| node.kind() != SyntaxKind::SourceFile)
            {
                if self.has_standard_head(&node, "LOCALLY") {
                    body_scopes.push(self.special_declarations(elements(&node).skip(1), false));
                }
            }
        }
        let mut scope = Vec::new();
        for specials in body_scopes.iter().rev() {
            let start = scope.len();
            self.add_free_specials(&mut scope, start, specials);
        }
        scope
    }
}
