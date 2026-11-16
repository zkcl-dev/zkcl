use crate::outline::{Element, elements};

use super::{
    Namespace, OccurrenceBinding, ScopeAtOffset, ScopeBinding, SymbolName, SyntaxKind, closed_list,
    symbol,
};

impl ScopeAtOffset<'_, '_> {
    pub(super) fn walk_ordinary_lambda_list(
        &self,
        lambda_el: &Element,
        scope: &mut Vec<ScopeBinding>,
        resolve: bool,
        specials: &[SymbolName],
    ) -> Result<Option<OccurrenceBinding>, ()> {
        if lambda_el.as_token().is_some() {
            if !resolve
                && symbol(lambda_el).is_some_and(|s| {
                    self.workspace.standard_symbol(&s, self.package) == Some("NIL")
                })
            {
                return Ok(None);
            }
            return Err(());
        }
        let list = lambda_el.as_node().ok_or(())?;
        if !closed_list(list) {
            return Err(());
        }
        let elems: Vec<_> = elements(list).collect();
        self.walk_ordinary_lambda_tail(&elems, scope, resolve, specials)
    }

    fn walk_ordinary_lambda_tail(
        &self,
        elems: &[Element],
        scope: &mut Vec<ScopeBinding>,
        resolve: bool,
        specials: &[SymbolName],
    ) -> Result<Option<OccurrenceBinding>, ()> {
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum ParameterSection {
            Required,
            Optional,
            Rest,
            Key,
            Aux,
        }
        let mut section = ParameterSection::Required;
        for param in elems {
            if let Some(sym) = symbol(param)
                && sym.name.starts_with('&')
            {
                section = match sym.name.as_str() {
                    "&OPTIONAL" => ParameterSection::Optional,
                    "&REST" | "&BODY" => ParameterSection::Rest,
                    "&KEY" => ParameterSection::Key,
                    "&AUX" => ParameterSection::Aux,
                    "&ALLOW-OTHER-KEYS" => section,
                    _ => return Err(()),
                };
                continue;
            }
            match section {
                ParameterSection::Required | ParameterSection::Rest => {
                    let sym = symbol(param).ok_or(())?;
                    if resolve && self.contains(param.text_range()) {
                        return Ok(Some(self.variable_target(
                            sym,
                            param.text_range(),
                            specials,
                        )));
                    }
                    scope.push(self.variable_binding(sym, param.text_range(), specials));
                }
                ParameterSection::Optional | ParameterSection::Key | ParameterSection::Aux => {
                    if param.as_token().is_some() {
                        let sym = symbol(param).ok_or(())?;
                        if resolve && self.contains(param.text_range()) {
                            return Ok(Some(self.variable_target(
                                sym,
                                param.text_range(),
                                specials,
                            )));
                        }
                        scope.push(self.variable_binding(sym, param.text_range(), specials));
                    } else if let Some(node) = param.as_node() {
                        if !closed_list(node) {
                            return Err(());
                        }
                        let mut parts = elements(node);
                        let first = parts.next().ok_or(())?;
                        let var_el =
                            if section == ParameterSection::Key && first.as_node().is_some() {
                                let kv = first.into_node().ok_or(())?;
                                if !closed_list(&kv) {
                                    return Err(());
                                }
                                let mut kv_parts = elements(&kv);
                                let _kw = kv_parts.next().ok_or(())?;
                                let v = kv_parts.next().ok_or(())?;
                                if kv_parts.next().is_some() {
                                    return Err(());
                                }
                                v
                            } else {
                                first
                            };
                        let sym = symbol(&var_el).ok_or(())?;
                        let init_el = parts.next();
                        let supplied_el = parts.next();
                        if parts.next().is_some() {
                            return Err(());
                        }
                        if resolve && self.contains(var_el.text_range()) {
                            return Ok(Some(self.variable_target(
                                sym,
                                var_el.text_range(),
                                specials,
                            )));
                        }
                        if resolve
                            && let Some(init) = init_el
                            && self.contains(init.text_range())
                        {
                            return Ok(self.expression(init, scope));
                        }
                        if resolve
                            && let Some(ref sup) = supplied_el
                            && self.contains(sup.text_range())
                        {
                            let sup_sym = symbol(sup).ok_or(())?;
                            return Ok(Some(self.variable_target(
                                sup_sym,
                                sup.text_range(),
                                specials,
                            )));
                        }
                        scope.push(self.variable_binding(sym, var_el.text_range(), specials));
                        if let Some(sup) = supplied_el {
                            if section == ParameterSection::Aux {
                                return Err(());
                            }
                            let sup_sym = symbol(&sup).ok_or(())?;
                            scope.push(self.variable_binding(sup_sym, sup.text_range(), specials));
                        }
                    } else {
                        return Err(());
                    }
                }
            }
        }
        Ok(None)
    }

    pub(super) fn walk_destructuring_lambda_list(
        &self,
        lambda_el: &Element,
        scope: &mut Vec<ScopeBinding>,
        resolve: bool,
        specials: &[SymbolName],
    ) -> Result<Option<OccurrenceBinding>, ()> {
        if lambda_el.as_token().is_some() {
            let Some(sym) = symbol(lambda_el) else {
                return Err(());
            };
            if resolve && self.contains(lambda_el.text_range()) {
                return Ok(Some(self.variable_target(
                    sym,
                    lambda_el.text_range(),
                    specials,
                )));
            }
            if resolve || self.workspace.standard_symbol(&sym, self.package) != Some("NIL") {
                scope.push(self.variable_binding(sym, lambda_el.text_range(), specials));
            }
            return Ok(None);
        }
        let list = lambda_el.as_node().ok_or(())?;
        if !resolve
            && (list.kind() != SyntaxKind::List
                || !list
                    .last_child_or_token()
                    .is_some_and(|e| e.kind() == SyntaxKind::CloseParen))
        {
            return Err(());
        }
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum ParameterSection {
            Required,
            Optional,
            Rest,
            Key,
            Aux,
        }
        let mut section = ParameterSection::Required;
        for param in elements(list) {
            if param.kind() == SyntaxKind::Dot {
                section = ParameterSection::Rest;
                continue;
            }
            if let Some(sym) = symbol(&param)
                && sym.name.starts_with('&')
            {
                section = match sym.name.as_str() {
                    "&WHOLE" | "&ENVIRONMENT" | "&REST" | "&BODY" => ParameterSection::Rest,
                    "&OPTIONAL" => ParameterSection::Optional,
                    "&KEY" => ParameterSection::Key,
                    "&AUX" => ParameterSection::Aux,
                    "&ALLOW-OTHER-KEYS" => section,
                    _ => return Err(()),
                };
                continue;
            }
            match section {
                ParameterSection::Required | ParameterSection::Rest => {
                    if resolve && self.contains(param.text_range()) {
                        return self.walk_destructuring_lambda_list(&param, scope, true, specials);
                    }
                    self.walk_destructuring_lambda_list(&param, scope, false, specials)?;
                    if section == ParameterSection::Rest {
                        section = ParameterSection::Required;
                    }
                }
                ParameterSection::Optional | ParameterSection::Key | ParameterSection::Aux => {
                    if param.as_token().is_some() {
                        let sym = symbol(&param).ok_or(())?;
                        if resolve && self.contains(param.text_range()) {
                            return Ok(Some(self.variable_target(
                                sym,
                                param.text_range(),
                                specials,
                            )));
                        }
                        scope.push(self.variable_binding(sym, param.text_range(), specials));
                    } else if let Some(node) = param.as_node() {
                        if !resolve && !closed_list(node) {
                            return Err(());
                        }
                        let mut parts = elements(node);
                        let first = parts.next().ok_or(())?;
                        if resolve {
                            let init = parts.next();
                            let sup = parts.next();
                            if self.contains(first.text_range()) {
                                return self
                                    .walk_destructuring_lambda_list(&first, scope, true, specials);
                            }
                            if let Some(init_el) = init
                                && self.contains(init_el.text_range())
                            {
                                return Ok(self.expression(init_el, scope));
                            }
                            self.walk_destructuring_lambda_list(&first, scope, false, specials)?;
                            if let Some(sup_el) = sup {
                                let sup_sym = symbol(&sup_el).ok_or(())?;
                                if self.contains(sup_el.text_range()) {
                                    return Ok(Some(self.variable_target(
                                        sup_sym,
                                        sup_el.text_range(),
                                        specials,
                                    )));
                                }
                                scope.push(self.variable_binding(
                                    sup_sym,
                                    sup_el.text_range(),
                                    specials,
                                ));
                            }
                        } else {
                            if section == ParameterSection::Key && first.as_node().is_some() {
                                let kv = first.into_node().ok_or(())?;
                                let mut kv_parts = elements(&kv);
                                let _kw = kv_parts.next().ok_or(())?;
                                let pat = kv_parts.next().ok_or(())?;
                                self.walk_destructuring_lambda_list(&pat, scope, false, specials)?;
                            } else {
                                self.walk_destructuring_lambda_list(
                                    &first, scope, false, specials,
                                )?;
                            }
                            let _init = parts.next();
                            if let Some(sup) = parts.next() {
                                let sup_sym = symbol(&sup).ok_or(())?;
                                scope.push(self.variable_binding(
                                    sup_sym,
                                    sup.text_range(),
                                    specials,
                                ));
                            }
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    pub(super) fn walk_specialized_lambda_list(
        &self,
        lambda_el: &Element,
        scope: &mut Vec<ScopeBinding>,
        resolve: bool,
        specials: &[SymbolName],
    ) -> Result<Option<OccurrenceBinding>, ()> {
        if lambda_el.as_token().is_some() {
            if !resolve
                && symbol(lambda_el).is_some_and(|s| {
                    self.workspace.standard_symbol(&s, self.package) == Some("NIL")
                })
            {
                return Ok(None);
            }
            return Err(());
        }
        let list = lambda_el.as_node().ok_or(())?;
        if !closed_list(list) {
            return Err(());
        }
        let elems: Vec<_> = elements(list).collect();
        let mut idx = 0;
        while idx < elems.len() {
            let param = &elems[idx];
            if let Some(sym) = symbol(param)
                && sym.name.starts_with('&')
            {
                break;
            }
            if param.as_token().is_some() {
                let sym = symbol(param).ok_or(())?;
                if resolve && self.contains(param.text_range()) {
                    return Ok(Some(self.variable_target(
                        sym,
                        param.text_range(),
                        specials,
                    )));
                }
                scope.push(self.variable_binding(sym, param.text_range(), specials));
            } else if let Some(node) = param.as_node() {
                if !closed_list(node) {
                    return Err(());
                }
                let mut parts = elements(node);
                let var_el = parts.next().ok_or(())?;
                let spec_el = parts.next().ok_or(())?;
                if parts.next().is_some() {
                    return Err(());
                }
                let sym = symbol(&var_el).ok_or(())?;
                if resolve && self.contains(var_el.text_range()) {
                    return Ok(Some(self.variable_target(
                        sym,
                        var_el.text_range(),
                        specials,
                    )));
                }
                if resolve && self.contains(spec_el.text_range()) {
                    if spec_el.as_token().is_some() {
                        return Ok(self.resolve_symbol(&spec_el, Namespace::Type, scope));
                    }
                    let spec_node = spec_el.into_node().ok_or(())?;
                    if !closed_list(&spec_node) {
                        return Ok(None);
                    }
                    let mut sp_parts = elements(&spec_node);
                    let eql_head = sp_parts.next().ok_or(())?;
                    let eql_sym = symbol(&eql_head).ok_or(())?;
                    if self.workspace.standard_symbol(&eql_sym, self.package) == Some("EQL") {
                        let eql_arg = sp_parts.next().ok_or(())?;
                        if self.contains(eql_arg.text_range()) {
                            return Ok(self.expression(eql_arg, scope));
                        }
                    }
                    return Ok(None);
                }
                scope.push(self.variable_binding(sym, var_el.text_range(), specials));
            } else {
                return Err(());
            }
            idx += 1;
        }
        if idx < elems.len() {
            return self.walk_ordinary_lambda_tail(&elems[idx..], scope, resolve, specials);
        }
        Ok(None)
    }
}
