//! Macro lambda-list patterns with disjoint, source-addressable operands.

use crate::navigation::index::{DocumentIndex, WorkspaceIndex, closed_list, possibly_read, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName, is_number_literal_token};
use crate::syntax::SyntaxKind;

use super::super::context::is_nil_syntax;
use super::{Builder, MAX_DEPTH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Parameters {
    positional: Vec<Parameter>,
    required: usize,
    rest: Option<usize>,
    keywords: Option<KeywordParameters>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct KeywordParameters {
    entries: Vec<(String, usize)>,
    allow_other: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Parameter {
    Name(usize),
    Destructure(Parameters),
}

pub(super) enum Actual {
    Missing,
    Form(Element),
    Rest(Vec<Element>),
}

impl Parameters {
    pub(super) fn bind(
        &self,
        source: &DocumentIndex,
        workspace: &WorkspaceIndex<'_>,
        args: &[Element],
        actual: &mut [Actual],
    ) -> Option<()> {
        if args.len() < self.required {
            return None;
        }
        for (parameter, form) in self.positional.iter().zip(args) {
            match parameter {
                Parameter::Name(slot) => actual[*slot] = Actual::Form(form.clone()),
                Parameter::Destructure(pattern) => pattern.bind(
                    source,
                    workspace,
                    &read_list(source, workspace, form)?,
                    actual,
                )?,
            }
        }
        let tail = args.get(self.positional.len()..).unwrap_or_default();
        if let Some(rest) = self.rest {
            actual[rest] = Actual::Rest(tail.to_vec());
        } else if let Some(keywords) = &self.keywords {
            keywords.bind(source, workspace, tail, actual)?;
        } else if !tail.is_empty() {
            return None;
        }
        Some(())
    }
}

impl KeywordParameters {
    fn bind(
        &self,
        source: &DocumentIndex,
        workspace: &WorkspaceIndex<'_>,
        tail: &[Element],
        actual: &mut [Actual],
    ) -> Option<()> {
        let (pairs, extra) = tail.as_chunks::<2>();
        if !extra.is_empty() {
            return None;
        }
        let mut allow_other = None;
        let mut unknown = false;
        for [key, value] in pairs {
            let name = symbol(key).filter(|name| name.prefix == Prefix::Keyword)?;
            if allow_other.is_none() && name.name == "ALLOW-OTHER-KEYS" {
                allow_other = Some(!is_nil_syntax(
                    value,
                    source.package_at(value.text_range().start()),
                    workspace,
                ));
            }
            if let Some((_, slot)) = self.entries.iter().find(|(key, _)| *key == name.name) {
                if matches!(actual[*slot], Actual::Missing) {
                    actual[*slot] = Actual::Form(value.clone());
                }
            } else if name.name != "ALLOW-OTHER-KEYS" {
                unknown = true;
            }
        }
        (!unknown || self.allow_other || allow_other == Some(true)).then_some(())
    }
}

pub(super) fn read_list(
    source: &DocumentIndex,
    workspace: &WorkspaceIndex<'_>,
    form: &Element,
) -> Option<Vec<Element>> {
    if form.as_token().is_some()
        && is_nil_syntax(
            form,
            source.package_at(form.text_range().start()),
            workspace,
        )
    {
        return Some(Vec::new());
    }
    let node = form.as_node().filter(|node| closed_list(node))?;
    elements(node)
        .filter_map(|element| possibly_read(element, source.known_features()))
        .map(|(element, known)| known.then_some(element))
        .collect()
}

impl Builder {
    fn add_parameter(&mut self, element: &Element) -> Option<usize> {
        let name = variable(element)?;
        if self.auxiliary.contains(&name) || self.fresh.contains(&name) {
            return None;
        }
        let slot = self.parameters_by_name.len();
        self.parameters_by_name
            .insert(name, slot)
            .is_none()
            .then_some(slot)
    }

    fn add_auxiliary(&mut self, element: &Element) -> Option<()> {
        let name = variable(element)?;
        (!self.parameters_by_name.contains_key(&name)
            && !self.fresh.contains(&name)
            && self.auxiliary.insert(name))
        .then_some(())
    }

    pub(super) fn parameters(&mut self, syntax: &Element, depth: usize) -> Option<Parameters> {
        if depth == MAX_DEPTH {
            return None;
        }
        let mut result = Parameters {
            positional: Vec::new(),
            required: 0,
            rest: None,
            keywords: None,
        };
        if let Some(name) = symbol(syntax).filter(|name| name.name == "NIL") {
            self.require(name, false);
            return Some(result);
        }
        let mut elements = elements(syntax.as_node().filter(|node| closed_list(node))?);
        let mut optional = false;
        let mut aux = false;
        let mut seen_parameter = false;
        let mut environment = false;
        while let Some(element) = elements.next() {
            if let Some(marker) = symbol(&element).filter(|name| name.name.starts_with('&')) {
                match marker.name.as_str() {
                    "&WHOLE" if !seen_parameter => self.add_auxiliary(&elements.next()?)?,
                    "&ENVIRONMENT" if depth == 0 && !environment && !aux => {
                        self.add_auxiliary(&elements.next()?)?;
                        environment = true;
                    }
                    "&OPTIONAL"
                        if !optional
                            && !aux
                            && result.keywords.is_none()
                            && result.rest.is_none() =>
                    {
                        optional = true
                    }
                    "&REST" | "&BODY"
                        if !aux && result.keywords.is_none() && result.rest.is_none() =>
                    {
                        result.rest = Some(self.add_parameter(&elements.next()?)?);
                    }
                    // A REST parameter and keyword slots can alias the same
                    // input forms. This template requires disjoint projections.
                    "&KEY" if !aux && result.keywords.is_none() && result.rest.is_none() => {
                        result.keywords = Some(KeywordParameters::default())
                    }
                    "&ALLOW-OTHER-KEYS"
                        if !aux
                            && result
                                .keywords
                                .as_ref()
                                .is_some_and(|keys| !keys.allow_other) =>
                    {
                        result.keywords.as_mut()?.allow_other = true
                    }
                    "&AUX" if !aux => aux = true,
                    _ => return None,
                }
                self.require(marker, false);
                seen_parameter = true;
                continue;
            }
            seen_parameter = true;
            if aux {
                self.add_aux_binding(&element)?;
                continue;
            }
            if result.rest.is_some() {
                return None;
            }
            if let Some(keywords) = &mut result.keywords {
                if keywords.allow_other {
                    return None;
                }
                let (key, slot) = self.optional_slot(&element, true)?;
                let key = key?;
                if keywords.entries.iter().any(|(name, _)| *name == key) {
                    return None;
                }
                keywords.entries.push((key, slot));
                continue;
            }
            let parameter = if optional {
                Parameter::Name(self.optional_slot(&element, false)?.1)
            } else if element.as_node().is_some() {
                Parameter::Destructure(self.parameters(&element, depth + 1)?)
            } else {
                Parameter::Name(self.add_parameter(&element)?)
            };
            result.positional.push(parameter);
            result.required += usize::from(!optional);
        }
        Some(result)
    }

    fn optional_slot(
        &mut self,
        element: &Element,
        keyword: bool,
    ) -> Option<(Option<String>, usize)> {
        let fields;
        let binding = if let Some(node) = element.as_node().filter(|node| closed_list(node)) {
            fields = elements(node).collect::<Vec<_>>();
            match fields.as_slice() {
                [name] => name,
                [name, default, supplied @ ..]
                    if supplied.len() <= 1 && self.literal_default(default) =>
                {
                    if let Some(supplied) = supplied.first() {
                        self.add_auxiliary(supplied)?;
                    }
                    name
                }
                _ => return None,
            }
        } else {
            element
        };
        if keyword && let Some(node) = binding.as_node().filter(|node| closed_list(node)) {
            let [key, variable] =
                <[Element; 2]>::try_from(elements(node).collect::<Vec<_>>()).ok()?;
            let key = symbol(&key).filter(|name| name.prefix == Prefix::Keyword)?;
            return Some((Some(key.name), self.add_parameter(&variable)?));
        }
        let slot = self.add_parameter(binding)?;
        Some((
            keyword.then(|| variable(binding).expect("validated parameter").name),
            slot,
        ))
    }

    fn add_aux_binding(&mut self, element: &Element) -> Option<()> {
        if let Some(node) = element.as_node().filter(|node| closed_list(node)) {
            let fields: Vec<_> = elements(node).collect();
            match fields.as_slice() {
                [var] => self.add_auxiliary(var),
                [var, init] => {
                    if self.literal_default(init) {
                        self.add_auxiliary(var)
                    } else {
                        let name = variable(var)?;
                        if self.parameters_by_name.contains_key(&name)
                            || self.auxiliary.contains(&name)
                            || self.fresh.contains(&name)
                        {
                            return None;
                        }
                        self.bind_fresh(name, init)
                    }
                }
                _ => None,
            }
        } else {
            self.add_auxiliary(element)
        }
    }

    fn literal_default(&mut self, default: &Element) -> bool {
        if let Some(name) = symbol(default) {
            if matches!(name.name.as_str(), "NIL" | "T") && name.prefix != Prefix::Keyword {
                self.require(name, false);
                return true;
            }
            if name.prefix == Prefix::Keyword {
                return true;
            }
        }
        default.as_token().is_some_and(|token| {
            matches!(
                token.kind(),
                SyntaxKind::String | SyntaxKind::RawString | SyntaxKind::Character
            ) || is_number_literal_token(token.text())
        }) || default
            .as_node()
            .is_some_and(|node| closed_list(node) && elements(node).next().is_none())
    }
}

pub(super) fn variable(element: &Element) -> Option<SymbolName> {
    let name = symbol(element)?;
    (name.prefix == Prefix::None
        && !matches!(name.name.as_str(), "NIL" | "T")
        && !name.name.starts_with('&')
        && !element
            .as_token()
            .is_some_and(|token| is_number_literal_token(token.text())))
    .then_some(name)
}
