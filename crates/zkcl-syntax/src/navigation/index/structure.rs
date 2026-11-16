//! Source-visible functions introduced by a literal DEFSTRUCT definition.

use std::sync::{Arc, OnceLock};

use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName, string_designator, string_literal};
use crate::syntax::{SyntaxNode, TextRange};

use super::{Declaration, closed_list, lambda_list_signature, possibly_read, symbol};
use crate::navigation::{Name, Namespace};

pub(super) fn declarations(
    form: &SyntaxNode,
    package: &str,
    operator: &SymbolName,
    features: Option<&[String]>,
) -> Option<Vec<Declaration>> {
    static UNARY: OnceLock<Arc<[String]>> = OnceLock::new();
    static SETTER: OnceLock<Arc<[String]>> = OnceLock::new();
    let unary = UNARY.get_or_init(|| Arc::from(["structure".into()]));
    let setter = SETTER.get_or_init(|| Arc::from(["new-value".into(), "structure".into()]));

    let fields = read_fields(form, features)?;
    let [_, spec, slots @ ..] = fields.as_slice() else {
        return None;
    };
    let options;
    let (name_site, opts) = if let Some(node) = spec.as_node() {
        options = read_fields(node, features)?;
        options.split_first()?
    } else {
        (spec, &[][..])
    };
    let structure = symbol(name_site)?.name;
    let range = name_site.text_range();
    let implicit = |name| FunctionName {
        spelling: SymbolName {
            name,
            prefix: Prefix::None,
        },
        range,
        explicit: false,
    };
    let mut constructor_options = Vec::new();
    let mut predicate = Some(implicit(format!("{structure}-P")));
    let mut copier = Some(implicit(format!("COPY-{structure}")));
    let mut prefix = Some(format!("{structure}-"));
    let mut typed = false;
    let mut named = false;
    let mut inherited = false;
    for opt in opts {
        let fields;
        let (key, args) = if let Some(node) = opt.as_node() {
            fields = read_fields(node, features)?;
            fields.split_first()?
        } else {
            (opt, &[][..])
        };
        let key = symbol(key).filter(|key| key.prefix == Prefix::Keyword)?;
        match (key.name.as_str(), args) {
            ("CONSTRUCTOR", []) => {
                constructor_options.push((Some(implicit(format!("MAKE-{structure}"))), None))
            }
            ("CONSTRUCTOR", [name]) => constructor_options.push((function_name(name)?, None)),
            ("CONSTRUCTOR", [name, args]) => {
                let name = function_name(name)??;
                constructor_options.push((Some(name), Some(lambda_list_signature(args)?)));
            }
            ("PREDICATE", [name]) => predicate = function_name(name)?,
            ("COPIER", [name]) => copier = function_name(name)?,
            ("CONC-NAME", []) => prefix = None,
            ("CONC-NAME", [value]) => {
                prefix = if nil_syntax(value) {
                    None
                } else {
                    Some(string_designator(value)?)
                };
            }
            ("TYPE", [_]) => typed = true,
            ("NAMED", []) => named = true,
            ("INCLUDE", [_, ..]) => inherited = true,
            ("PREDICATE" | "COPIER", [])
            | ("PRINT-FUNCTION" | "PRINT-OBJECT" | "INITIAL-OFFSET", _) => {}
            _ => return None,
        }
    }

    let mut output = Vec::new();
    let mut record = |name: FunctionName, namespace, params: &Arc<[String]>| {
        let name_info = Name {
            namespace,
            spelling: name.spelling,
            package: Some(package.to_owned()),
        };
        if name_info.package().is_some() {
            output.push(Declaration {
                name: name_info,
                range: name.range,
                operator: operator.clone(),
                evaluates_arguments: true,
                params: Some(Arc::clone(params)),
                documentation: None,
                macro_definition: None,
                renamable: name.explicit,
            });
        }
    };
    if (!typed || named)
        && let Some(predicate) = predicate
    {
        record(predicate, Namespace::Function, unary);
    }
    if let Some(copier) = copier {
        record(copier, Namespace::Function, unary);
    }
    let mut keyword_parameters = vec!["&key".to_owned()];
    for slot in slots.iter().filter(|slot| {
        !slot
            .as_token()
            .is_some_and(|token| string_literal(token).is_some())
    }) {
        let fields;
        let (site, options) = if let Some(node) = slot.as_node() {
            fields = read_fields(node, features)?;
            let (site, rest) = fields.split_first()?;
            (site, rest.get(1..).unwrap_or_default())
        } else {
            (slot, &[][..])
        };
        let mut spelling = symbol(site)?;
        keyword_parameters.push(site.to_string());
        if let Some(prefix) = &prefix {
            spelling = SymbolName {
                name: format!("{prefix}{}", spelling.name),
                prefix: Prefix::None,
            };
        }
        let (options, extra) = options.as_chunks::<2>();
        if !extra.is_empty() {
            return None;
        }
        let read_only = options.iter().any(|[key, value]| {
            symbol(key).is_some_and(|key| key.prefix == Prefix::Keyword && key.name == "READ-ONLY")
                && !nil_syntax(value)
        });
        let accessor = FunctionName {
            spelling,
            range: site.text_range(),
            explicit: false,
        };
        record(accessor.clone(), Namespace::Function, unary);
        if !read_only {
            record(accessor, Namespace::SetfFunction, setter);
        }
    }
    if inherited {
        keyword_parameters.push("&allow-other-keys".into());
    }
    let keyword_parameters: Arc<[String]> = keyword_parameters.into();
    if constructor_options.is_empty() {
        constructor_options.push((Some(implicit(format!("MAKE-{structure}"))), None));
    }
    for (name, params) in constructor_options {
        if let Some(name) = name {
            record(
                name,
                Namespace::Function,
                params.as_ref().unwrap_or(&keyword_parameters),
            );
        }
    }
    Some(output)
}

#[derive(Clone)]
struct FunctionName {
    spelling: SymbolName,
    range: TextRange,
    explicit: bool,
}

fn function_name(site: &Element) -> Option<Option<FunctionName>> {
    if nil_syntax(site) {
        return Some(None);
    }
    Some(Some(FunctionName {
        spelling: symbol(site)?,
        range: site.text_range(),
        explicit: true,
    }))
}

fn nil_syntax(site: &Element) -> bool {
    symbol(site).is_some_and(|sym| sym.standard_operator() == Some("NIL"))
        || site
            .as_node()
            .is_some_and(|node| closed_list(node) && elements(node).next().is_none())
}

fn read_fields(node: &SyntaxNode, features: Option<&[String]>) -> Option<Vec<Element>> {
    closed_list(node).then_some(())?;
    elements(node)
        .filter_map(|element| possibly_read(element, features))
        .map(|(element, known)| known.then_some(element))
        .collect()
}
