use std::borrow::Cow;
use std::ops::ControlFlow::{self, Break, Continue};
use std::sync::{Arc, OnceLock};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::ast::FeatureConditional;
use crate::cl_builtins::{BuiltinKind, builtin_kind, builtin_signature, is_external_cl_symbol};
use crate::documentation::DocumentationComment;
use crate::lint::macro_template::MacroTemplate;
use crate::outline::{
    Element, ReaderStateEffect, declaration, elements, eval_when_allows_top_level_outline,
    is_element_kind, list_operator, reader_boundary, reader_state_effect,
};
use crate::reader_tokens::{
    Prefix, SymbolName, canonical_package, string_designator, string_literal,
};
use crate::simd_builtins;
use crate::syntax::{SourceReadtable, SyntaxKind, SyntaxNode, TextRange, TextSize, list_has_dot};
use crate::treelist_builtins;

use super::scope::{FunctionBodyScope, ScopeAtOffset};
use super::{Name, Namespace, OccurrenceBinding, ScopeBinding};

mod structure;

#[derive(Debug, Clone)]
pub(crate) struct Declaration {
    pub(crate) name: Name,
    pub(crate) range: TextRange,
    pub(crate) operator: SymbolName,
    pub(crate) evaluates_arguments: bool,
    pub(crate) params: Option<Arc<[String]>>,
    pub(crate) documentation: Option<String>,
    pub(crate) macro_definition: Option<Arc<MacroDefinition>>,
    pub(crate) renamable: bool,
}

#[derive(Debug)]
pub(crate) struct MacroDefinition {
    pub(crate) source: rowan::GreenNode,
    pub(crate) offset: TextSize,
    template: OnceLock<Option<MacroTemplate>>,
}

impl MacroDefinition {
    pub(crate) fn template(&self) -> Option<&MacroTemplate> {
        self.template
            .get_or_init(|| {
                let node = SyntaxNode::new_root(self.source.clone());
                MacroTemplate::infer(elements(&node).skip(2))
                    .map(|template| template.shifted(self.offset))
            })
            .as_ref()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PackageDefinition {
    pub(crate) name: String,
    pub(crate) nicknames: Vec<String>,
    pub(crate) local_nicknames: FxHashMap<String, String>,
    pub(crate) exports: Vec<String>,
    pub(crate) imports: Vec<PackageImport>,
    pub(crate) shadows: Vec<String>,
    pub(crate) uses: Vec<String>,
    pub(crate) symbol_designators: Vec<PackageSymbolDesignator>,
}

#[derive(Debug, Clone)]
pub(crate) struct PackageSymbolDesignator {
    pub(crate) name: String,
    pub(crate) range: TextRange,
    pub(crate) import_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackageImport {
    pub(crate) source: String,
    pub(crate) name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct IndexedTopLevelForm {
    pub(crate) range: TextRange,
    parent: Option<usize>,
}

/// Macro call operands whose value and lexical roles were proven for this snapshot.
pub(crate) type MacroOperands = FxHashMap<TextRange, Vec<Element>>;

#[derive(Debug, Default)]
pub(crate) struct DocumentIndex {
    pub(crate) declarations: Vec<Declaration>,
    pub(crate) documentation: Vec<DocumentationComment>,
    pub(crate) packages: Vec<PackageDefinition>,
    pub(crate) top_level_forms: Vec<IndexedTopLevelForm>,
    pub(crate) special_proclamations: Vec<(Name, SymbolName, SymbolName)>,
    pub(crate) guarded_callables: Vec<Name>,
    form_lint_inputs: OnceLock<Arc<crate::lint::NamespaceInputs>>,
    declarations_by_name: Vec<usize>,
    known_features: Option<Vec<String>>,
    initial_package: Option<String>,
    pub(super) initial_readtable: SourceReadtable,
    reader_boundary: Option<ReaderBoundary>,
    /// Package state after each top-level IN-PACKAGE (or an unknown package
    /// effect), keyed by the byte offset where it takes effect.
    package_changes: Vec<(TextSize, Option<String>)>,
}

#[derive(Debug, Clone, Copy)]
struct ReaderBoundary {
    analysis: TextSize,
    editing: TextSize,
}

impl ReaderBoundary {
    fn at(offset: TextSize) -> Self {
        Self {
            analysis: offset,
            editing: offset,
        }
    }

    fn in_form(form: &Element, analysis: TextSize) -> Self {
        if crate::outline::unknown_boundary(form) {
            return Self::at(analysis);
        }
        // A #. operand has source structure before it is evaluated. Static
        // name analysis skips the whole form, while editing can use the
        // known syntax up to the first possible read-time effect.
        let editing = form
            .as_node()
            .into_iter()
            .flat_map(SyntaxNode::descendants)
            .filter(|node| node.kind() == SyntaxKind::ReaderForm)
            .filter(|node| {
                node.first_token().is_some_and(|token| {
                    token.kind() == SyntaxKind::DispatchPrefix && token.text().ends_with('.')
                })
            })
            .map(|node| node.text_range().end())
            .min()
            .unwrap_or(analysis);
        Self { analysis, editing }
    }
}

impl DocumentIndex {
    pub(crate) fn form_lint_inputs(&self) -> &Arc<crate::lint::NamespaceInputs> {
        self.form_lint_inputs
            .get_or_init(|| Arc::new(crate::lint::NamespaceInputs::collect(self)))
    }

    pub(crate) fn has_workspace_entries(&self) -> bool {
        !(self.declarations.is_empty()
            && self.packages.is_empty()
            && self.special_proclamations.is_empty()
            && self.guarded_callables.is_empty())
    }

    pub(crate) fn collect(
        root: &SyntaxNode,
        initial_package: Option<&str>,
        known_features: Option<&[String]>,
        initial_readtable: SourceReadtable,
    ) -> Self {
        let mut index = Self {
            initial_package: initial_package.map(str::to_owned),
            known_features: known_features.map(<[String]>::to_vec),
            initial_readtable,
            ..Self::default()
        };
        let mut package = index.initial_package.clone();
        for form in elements(root) {
            if let Break(boundary) =
                index.process_top_level(form, &mut package, known_features, None, None)
            {
                index.reader_boundary = Some(boundary);
                break;
            }
        }
        index.declarations_by_name = (0..index.declarations.len()).collect();
        index
            .declarations_by_name
            .sort_unstable_by(|&left, &right| {
                index.declarations[left]
                    .name
                    .spelling
                    .name
                    .cmp(&index.declarations[right].name.spelling.name)
            });
        index
    }

    fn process_top_level(
        &mut self,
        form: Element,
        package: &mut Option<String>,
        known_features: Option<&[String]>,
        parent: Option<usize>,
        documentation_anchor: Option<&Element>,
    ) -> ControlFlow<ReaderBoundary> {
        let range = form.text_range();
        match reader_conditional(&form, known_features) {
            Some(ReaderConditional::Active(guarded)) => {
                return self.process_top_level(
                    guarded,
                    package,
                    known_features,
                    parent,
                    documentation_anchor.or(Some(&form)),
                );
            }
            Some(ReaderConditional::Inactive) => return Continue(()),
            Some(ReaderConditional::Unknown(guarded)) => {
                if reader_boundary(&guarded) {
                    return Break(ReaderBoundary::in_form(&guarded, range.start()));
                }
                let effect = reader_state_effect(&guarded);
                let context = if effect == ReaderStateEffect::Package {
                    None
                } else {
                    package.as_deref().or_else(|| {
                        self.package_changes
                            .is_empty()
                            .then_some("COMMON-LISP-USER")
                    })
                };
                self.record_guarded_callables(guarded, context);
                return match effect {
                    ReaderStateEffect::ReaderSyntax => Break(ReaderBoundary::at(range.end())),
                    ReaderStateEffect::Package => {
                        self.set_package(range.end(), None, package);
                        Continue(())
                    }
                    ReaderStateEffect::None => Continue(()),
                };
            }
            None if reader_boundary(&form) => {
                return Break(ReaderBoundary::in_form(&form, range.start()));
            }
            None => {}
        }

        if let Some(list) = form.as_node()
            && let Some(flow) = crate::outline::with_list_operator(list, |op| match op {
                "IN-PACKAGE" => {
                    let mut arguments = elements(list).skip(1);
                    let next = if closed_list(list) {
                        let name = arguments
                            .next()
                            .and_then(|argument| string_designator(&argument));
                        name.filter(|_| arguments.next().is_none())
                    } else {
                        None
                    };
                    self.set_package(range.end(), next, package);
                    Some(Continue(()))
                }
                wrapper @ ("PROGN" | "LOCALLY" | "EVAL-WHEN") if non_dotted_list(list) => {
                    let wrapper_index = self.top_level_forms.len();
                    self.top_level_forms
                        .push(IndexedTopLevelForm { range, parent });
                    let body_start = if wrapper == "EVAL-WHEN" {
                        if !eval_when_allows_top_level_outline(list) {
                            return Some(Continue(()));
                        }
                        2
                    } else {
                        1
                    };
                    for child in elements(list).skip(body_start) {
                        if let ControlFlow::Break(stop) = self.process_top_level(
                            child,
                            package,
                            known_features,
                            Some(wrapper_index),
                            None,
                        ) {
                            return Some(Break(stop));
                        }
                    }
                    Some(Continue(()))
                }
                _ => None,
            })
            .flatten()
        {
            return flow;
        }

        match reader_state_effect(&form) {
            ReaderStateEffect::ReaderSyntax => return Break(ReaderBoundary::at(range.end())),
            ReaderStateEffect::Package => {
                self.set_package(range.end(), None, package);
                return Continue(());
            }
            ReaderStateEffect::None => {}
        }
        self.top_level_forms
            .push(IndexedTopLevelForm { range, parent });
        if let Some(list) = form.as_node() {
            self.record_declaration(
                list,
                package.as_deref(),
                documentation_anchor.unwrap_or(&form),
            );
        }
        Continue(())
    }

    fn record_guarded_callables(&mut self, form: Element, package: Option<&str>) {
        let mut pending = vec![form];
        while let Some(form) = pending.pop() {
            let Some((form, _)) = possibly_read(form, self.known_features()) else {
                continue;
            };
            let Some(list) = form.as_node().filter(|list| closed_list(list)) else {
                continue;
            };
            let mut forms = elements(list);
            let Some(head) = forms.next().and_then(|head| symbol(&head)) else {
                continue;
            };
            match head.standard_operator() {
                Some("DEFMACRO" | "DEFUN" | "DEFGENERIC") => {
                    if let Some(spelling) = forms.next().and_then(|name| symbol(&name)) {
                        self.guarded_callables.push(Name {
                            namespace: Namespace::Function,
                            spelling,
                            package: package.map(str::to_owned),
                        });
                    }
                }
                Some("DEFSTRUCT") => {
                    if let Some(package) = package
                        && let Some(declarations) =
                            structure::declarations(list, package, &head, self.known_features())
                    {
                        self.guarded_callables
                            .extend(declarations.into_iter().map(|decl| decl.name));
                    }
                }
                Some("PROGN" | "LOCALLY") => pending.extend(forms),
                Some("EVAL-WHEN") if eval_when_allows_top_level_outline(list) => {
                    pending.extend(forms.skip(1))
                }
                _ => {}
            }
        }
    }

    /// Records a package-state change so queries at any offset see the package in
    /// effect there instead of the package at the enclosing form's start.
    fn set_package(&mut self, at: TextSize, next: Option<String>, package: &mut Option<String>) {
        // A missing initial package uses the usual source-analysis default.
        // An explicit transition to None means that the package became unknown.
        if *package != next || (next.is_none() && self.package_changes.is_empty()) {
            *package = next.clone();
            self.package_changes.push((at, next));
        }
    }

    fn record_declaration(&mut self, list: &SyntaxNode, package: Option<&str>, anchor: &Element) {
        let Some(head) = elements(list).next().as_ref().and_then(symbol) else {
            return;
        };
        let Some(operator) = head.zk_definition().or_else(|| head.standard_operator()) else {
            return;
        };
        if operator == "DECLAIM" {
            for spec in elements(list)
                .skip(1)
                .filter_map(Element::into_node)
                .filter(closed_list)
            {
                let mut parts = elements(&spec);
                let Some(special) = parts.next().as_ref().and_then(symbol) else {
                    continue;
                };
                if special.standard_operator() == Some("SPECIAL") {
                    self.special_proclamations.extend(parts.filter_map(|part| {
                        let spelling = symbol(&part)?;
                        Some((
                            Name {
                                namespace: Namespace::Value,
                                spelling,
                                package: package.map(str::to_owned),
                            },
                            head.clone(),
                            special.clone(),
                        ))
                    }));
                }
            }
            return;
        }
        let Some(entry) = declaration(list) else {
            return;
        };
        let comment = DocumentationComment::preceding(list.text_range(), anchor);
        let documentation = comment
            .as_ref()
            .map(|comment| comment.markdown().to_owned());
        self.documentation.extend(comment);
        if operator == "DEFPACKAGE" {
            if let Some(package) = package_definition(list) {
                self.packages.push(package);
            }
            return;
        }
        let base_namespace = match operator {
            "DEFUN" | "DEFMACRO" | "DEFGENERIC" => Namespace::Function,
            "DEFVAR" | "DEFPARAMETER" | "DEFCONSTANT" | "DEFINE-SYMBOL-MACRO" => Namespace::Value,
            "DEFCLASS" | "DEFSTRUCT" | "DEFTYPE" | "DEFINE-CONDITION" | "DEFVARIANT" => {
                Namespace::Type
            }
            "DEFPATTERN" => Namespace::Pattern,
            // A method doesn't identify which definition a generic call invokes.
            _ => return,
        };
        let declared_name = SymbolName::parse(&entry.name)
            .map(|spelling| (base_namespace, spelling, entry.selection_range))
            .or_else(|| {
                if base_namespace == Namespace::Function
                    && let Some(name_el) = elements(list).nth(1)
                    && let Some((sym, range)) = parse_setf_function_name(&name_el)
                {
                    Some((Namespace::SetfFunction, sym, range))
                } else {
                    None
                }
            });
        if let Some((namespace, spelling, decl_range)) = declared_name {
            let package = package.or_else(|| {
                (namespace == Namespace::Function && self.package_changes.is_empty())
                    .then_some("COMMON-LISP-USER")
            });
            let name = Name {
                namespace,
                spelling,
                package: package.map(str::to_owned),
            };
            if name.package().is_some() {
                let evaluates_arguments = matches!(operator, "DEFUN" | "DEFGENERIC");
                let (params, docstring) = extract_lambda_list_and_docstring(list, operator);
                self.declarations.push(Declaration {
                    name,
                    range: decl_range,
                    operator: head.clone(),
                    evaluates_arguments,
                    params,
                    documentation: documentation.or(docstring),
                    macro_definition: (operator == "DEFMACRO").then(|| {
                        Arc::new(MacroDefinition {
                            source: list.green().to_owned(),
                            offset: list.text_range().start(),
                            template: OnceLock::new(),
                        })
                    }),
                    // A structure rename can also change its derived function names.
                    renamable: operator != "DEFSTRUCT",
                });
            }
        }
        if operator == "DEFVARIANT" && package.is_some() {
            let mut conc_prefix: Option<String> = None;
            if let Some(header_list) = elements(list)
                .nth(1)
                .and_then(Element::into_node)
                .filter(closed_list)
            {
                self.record_variant_predicate(&header_list, package, &head);
                conc_prefix = Self::variant_conc_name(&header_list);
            }
            for case in elements(list)
                .skip(2)
                .filter_map(Element::into_node)
                .filter(closed_list)
            {
                let mut fields = elements(&case);
                let Some(head_el) = fields.next() else {
                    continue;
                };
                let Some(spelling) = symbol(&head_el) else {
                    continue;
                };
                let field_els: Vec<Element> = fields.collect();
                let params: Arc<[String]> = field_els
                    .iter()
                    .map(|f| {
                        f.as_node()
                            .filter(|n| closed_list(n))
                            .and_then(|n| elements(n).next())
                            .map_or_else(|| f.to_string(), |slot| slot.to_string())
                    })
                    .collect();
                for ns in [Namespace::Type, Namespace::Pattern, Namespace::Function] {
                    self.declarations.push(Declaration {
                        name: Name {
                            namespace: ns,
                            spelling: spelling.clone(),
                            package: package.map(str::to_owned),
                        },
                        range: head_el.text_range(),
                        operator: head.clone(),
                        evaluates_arguments: ns == Namespace::Function,
                        params: (ns != Namespace::Type).then(|| Arc::clone(&params)),
                        documentation: None,
                        macro_definition: None,
                        renamable: true,
                    });
                }
                let prefix = conc_prefix
                    .clone()
                    .unwrap_or_else(|| format!("{}-", spelling.name));
                for field_el in field_els {
                    let slot_el = field_el
                        .as_node()
                        .filter(|n| closed_list(n))
                        .and_then(|n| elements(n).next())
                        .unwrap_or(field_el);
                    let Some(slot_sym) = symbol(&slot_el) else {
                        continue;
                    };
                    self.declarations.push(Declaration {
                        name: Name {
                            namespace: Namespace::Function,
                            spelling: SymbolName {
                                prefix: Prefix::None,
                                name: format!("{prefix}{}", slot_sym.name),
                            },
                            package: package.map(str::to_owned),
                        },
                        range: slot_el.text_range(),
                        operator: head.clone(),
                        evaluates_arguments: true,
                        params: Some(Arc::from(["INSTANCE".to_owned()])),
                        documentation: None,
                        macro_definition: None,
                        renamable: conc_prefix.is_some(),
                    });
                }
            }
        }
        if matches!(operator, "DEFCLASS" | "DEFINE-CONDITION") && package.is_some() {
            self.record_class_accessors(list, package, &head);
        }
        if operator == "DEFSTRUCT"
            && let Some(package) = package.or_else(|| {
                self.package_changes
                    .is_empty()
                    .then_some("COMMON-LISP-USER")
            })
            && let Some(declarations) =
                structure::declarations(list, package, &head, self.known_features())
        {
            self.declarations.extend(declarations);
        }
    }

    fn record_variant_predicate(
        &mut self,
        header_list: &SyntaxNode,
        package: Option<&str>,
        head: &SymbolName,
    ) {
        let variant_name = elements(header_list).next().and_then(|e| symbol(&e));
        let Some((pred_arg, fallback_range)) =
            Self::variant_header_option(header_list, "PREDICATE")
        else {
            return;
        };
        let (spelling, decl_range, renamable) = match pred_arg.as_ref().and_then(symbol) {
            Some(sym) if sym.prefix == Prefix::None && sym.name == "NIL" => return,
            Some(sym) if !(sym.prefix == Prefix::None && sym.name == "T") => {
                let range = pred_arg
                    .as_ref()
                    .map_or(fallback_range, Element::text_range);
                (sym, range, true)
            }
            _ => {
                let Some(vname) = variant_name.as_ref() else {
                    return;
                };
                (
                    SymbolName {
                        prefix: Prefix::None,
                        name: format!("{}-P", vname.name),
                    },
                    fallback_range,
                    false,
                )
            }
        };
        self.declarations.push(Declaration {
            name: Name {
                namespace: Namespace::Function,
                spelling,
                package: package.map(str::to_owned),
            },
            range: decl_range,
            operator: head.clone(),
            evaluates_arguments: true,
            params: Some(Arc::from(["OBJECT".to_owned()])),
            documentation: None,
            macro_definition: None,
            renamable,
        });
    }

    fn variant_header_option(
        header_list: &SyntaxNode,
        option_name: &str,
    ) -> Option<(Option<Element>, TextRange)> {
        let opts: Vec<Element> = elements(header_list).skip(1).collect();
        let mut i = 0;
        while i < opts.len() {
            if let Some(kw) = symbol(&opts[i]).filter(|s| s.prefix == Prefix::Keyword) {
                if kw.name == option_name {
                    return Some((opts.get(i + 1).cloned(), opts[i].text_range()));
                }
                i += 2;
            } else if let Some(opt_node) = opts[i].as_node().filter(|node| closed_list(node)) {
                let mut oparts = elements(opt_node);
                if oparts
                    .next()
                    .and_then(|e| symbol(&e))
                    .is_some_and(|s| s.prefix == Prefix::Keyword && s.name == option_name)
                {
                    return Some((oparts.next(), opts[i].text_range()));
                }
                i += 1;
            } else {
                i += 1;
            }
        }
        None
    }

    fn variant_conc_name(header_list: &SyntaxNode) -> Option<String> {
        let (arg, _) = Self::variant_header_option(header_list, "CONC-NAME")?;
        let Some(arg) = arg else {
            return Some(String::new());
        };
        if let Some(sym) = symbol(&arg) {
            if sym.prefix == Prefix::None && sym.name == "NIL" {
                return Some(String::new());
            }
            return Some(sym.name);
        }
        arg.into_token().and_then(|t| string_literal(&t))
    }

    fn record_class_accessors(
        &mut self,
        list: &SyntaxNode,
        package: Option<&str>,
        head: &SymbolName,
    ) {
        static READER_PARAMS: OnceLock<Arc<[String]>> = OnceLock::new();
        static WRITER_PARAMS: OnceLock<Arc<[String]>> = OnceLock::new();
        let Some(slots_node) = elements(list).nth(3).and_then(Element::into_node) else {
            return;
        };
        if !closed_list(&slots_node) {
            return;
        }
        let mut record = |namespace, spelling, range, params: &Arc<[String]>| {
            let name = Name {
                namespace,
                spelling,
                package: package.map(str::to_owned),
            };
            if name.package().is_some() {
                self.declarations.push(Declaration {
                    name,
                    range,
                    operator: head.clone(),
                    evaluates_arguments: true,
                    params: Some(Arc::clone(params)),
                    documentation: None,
                    macro_definition: None,
                    renamable: true,
                });
            }
        };
        let reader = READER_PARAMS.get_or_init(|| Arc::from(["instance".to_owned()]));
        let writer = WRITER_PARAMS
            .get_or_init(|| Arc::from(["new-value".to_owned(), "instance".to_owned()]));
        for slot in elements(&slots_node)
            .filter_map(Element::into_node)
            .filter(closed_list)
        {
            let mut options = elements(&slot).skip(1);
            while let (Some(key), Some(value)) = (options.next(), options.next()) {
                let Some(option) = symbol(&key).filter(|key| key.prefix == Prefix::Keyword) else {
                    continue;
                };
                let range = value.text_range();
                match option.name.as_str() {
                    "ACCESSOR" | "READER" => {
                        let Some(name) = symbol(&value) else { continue };
                        record(Namespace::Function, name.clone(), range, reader);
                        if option.name == "ACCESSOR" {
                            record(Namespace::SetfFunction, name, range, writer);
                        }
                    }
                    "WRITER" => {
                        if let Some(name) = symbol(&value) {
                            record(Namespace::Function, name, range, writer);
                        } else if let Some((name, range)) = parse_setf_function_name(&value) {
                            record(Namespace::SetfFunction, name, range, writer);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub(crate) fn top_level_form_at(&self, offset: TextSize) -> Option<&IndexedTopLevelForm> {
        if !self.reader_syntax_known_at(offset) {
            return None;
        }
        let mut form = self.top_level_forms_starting_at_or_before(offset).last()?;
        while !form.range.contains_inclusive(offset) {
            form = &self.top_level_forms[form.parent?];
        }
        Some(form)
    }

    pub(crate) fn top_level_forms_starting_at_or_before(
        &self,
        offset: TextSize,
    ) -> &[IndexedTopLevelForm] {
        let end = self
            .top_level_forms
            .partition_point(|form| form.range.start() <= offset);
        &self.top_level_forms[..end]
    }

    pub(crate) fn declarations_named<'a>(
        &'a self,
        spelling: &str,
    ) -> impl Iterator<Item = &'a Declaration> {
        let start = self.declarations_by_name.partition_point(|&index| {
            self.declarations[index].name.spelling.name.as_str() < spelling
        });
        self.declarations_by_name[start..]
            .iter()
            .map(|&index| &self.declarations[index])
            .take_while(move |decl| decl.name.spelling.name == spelling)
    }

    pub(crate) fn reader_syntax_known_at(&self, offset: TextSize) -> bool {
        self.reader_boundary
            .is_none_or(|boundary| offset < boundary.analysis)
    }

    pub(crate) fn editable_reader_syntax_at(&self, offset: TextSize) -> bool {
        self.reader_boundary
            .is_none_or(|boundary| offset < boundary.editing)
    }

    pub(crate) fn initial_package(&self) -> Option<&str> {
        self.initial_package.as_deref()
    }

    pub(crate) fn known_features(&self) -> Option<&[String]> {
        self.known_features.as_deref()
    }

    /// Returns the package in effect at `offset`, including package changes made by
    /// top-level IN-PACKAGE forms inside PROGN, LOCALLY, or EVAL-WHEN wrappers.
    pub(crate) fn package_at(&self, offset: TextSize) -> Option<&str> {
        self.package_change_at(offset)
            .map(|(_, package)| package.as_deref())
            .unwrap_or(self.initial_package.as_deref())
    }

    pub(crate) fn package_known_at(&self, offset: TextSize) -> bool {
        self.package_change_at(offset)
            .is_none_or(|(_, package)| package.is_some())
    }

    fn package_change_at(&self, offset: TextSize) -> Option<&(TextSize, Option<String>)> {
        let end = self
            .package_changes
            .partition_point(|(at, _)| *at <= offset);
        self.package_changes[..end].last()
    }

    pub(crate) fn resolve_occurrence_at(
        &self,
        root: &SyntaxNode,
        offset: TextSize,
        workspace: &WorkspaceIndex<'_>,
    ) -> Option<OccurrenceBinding> {
        self.resolve_occurrence_with(offset, workspace, |range| find_top_level_form(root, range))
    }

    pub(crate) fn resolve_occurrence_with(
        &self,
        offset: TextSize,
        workspace: &WorkspaceIndex<'_>,
        find_form: impl FnOnce(TextRange) -> Option<Element>,
    ) -> Option<OccurrenceBinding> {
        self.resolve_occurrence_with_operands(offset, workspace, None, find_form)
    }

    pub(crate) fn resolve_occurrence_with_operands(
        &self,
        offset: TextSize,
        workspace: &WorkspaceIndex<'_>,
        macro_operands: Option<&MacroOperands>,
        find_form: impl FnOnce(TextRange) -> Option<Element>,
    ) -> Option<OccurrenceBinding> {
        let form = self.top_level_form_at(offset)?;
        let element = find_form(form.range)?;
        let context = ScopeAtOffset {
            package: self.package_at(offset),
            offset,
            workspace,
            known_features: self.known_features(),
            macro_operands,
        };
        let mut scope = context.top_level_specials(&element);
        context.top_level(element, &mut scope)
    }
}

/// Resolve consecutive source occurrences with the same indexed form and scope.
pub(crate) struct OccurrenceResolver<'a, 'docs> {
    source: &'a DocumentIndex,
    root: &'a SyntaxNode,
    workspace: &'a WorkspaceIndex<'docs>,
    macro_operands: Option<&'a MacroOperands>,
    current: Option<OccurrenceForm<'a>>,
    scratch: Vec<ScopeBinding>,
}

struct OccurrenceForm<'a> {
    element: Element,
    package: Option<&'a str>,
    scope: Vec<ScopeBinding>,
    function_body: Option<FunctionBodyScope>,
}

impl<'a, 'docs> OccurrenceResolver<'a, 'docs> {
    pub(crate) fn new(
        source: &'a DocumentIndex,
        root: &'a SyntaxNode,
        workspace: &'a WorkspaceIndex<'docs>,
        macro_operands: Option<&'a MacroOperands>,
    ) -> Self {
        Self {
            source,
            root,
            workspace,
            macro_operands,
            current: None,
            scratch: Vec::new(),
        }
    }

    pub(crate) fn resolve(&mut self, token: &crate::outline::Token) -> Option<OccurrenceBinding> {
        let offset = token.text_range().start();
        let form = self.source.top_level_form_at(offset)?;
        let package = self.source.package_at(offset);
        let context = ScopeAtOffset {
            package,
            offset,
            workspace: self.workspace,
            known_features: self.source.known_features(),
            macro_operands: self.macro_operands,
        };
        if self.current.as_ref().is_none_or(|current| {
            current.element.text_range() != form.range || current.package != package
        }) {
            let element = find_top_level_form_near(self.root, form.range, token.clone().into())?;
            let scope = context.top_level_specials(&element);
            let function_body = context.prepare_function_body(&element, &scope);
            self.current = Some(OccurrenceForm {
                element,
                package,
                scope,
                function_body,
            });
        }
        let current = self.current.as_ref()?;
        if let Some(body) = &current.function_body
            && body.contains(offset)
        {
            return context.resolve_function_body(&current.element, body, &mut self.scratch);
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(&current.scope);
        context.top_level(current.element.clone(), &mut self.scratch)
    }
}

#[cfg(test)]
#[test]
fn batch_resolution_matches_fresh_lookup_across_top_level_environments() {
    let source = r#"
        (in-package :cl-user)
        (declaim (special *ambient*))
        (defun adjacent(x)x)
        (defun (setf accessor) (value object)
          "A docstring"
          (declare (special value) (ignorable object))
          (return-from accessor value))
        (defun defaults (x &optional (y x yp) &key ((:key z) y))
          (let ((x (if yp y z))) (list x *ambient*)))
        (defun malformed (&bogus parameter) parameter)
        (locally (declare (special hidden))
          (progn
            (defun nested (hidden) (+ hidden *ambient*))
            #+use-feature (defun selected (x) x)))
        (defpackage :shadowing (:use :cl) (:shadow #:defun))
        (in-package :shadowing)
        (defun unresolved (x) x)
        (cl:defun standard (x) x)
        (set-macro-character #\@ #'identity)
        (cl:defun after-unknown-reader (x) x)
    "#;
    let parsed = crate::syntax::parse_source(source);
    let root = parsed.root();
    let features = ["USE-FEATURE".to_owned()];
    for features in [None, Some(&[][..]), Some(features.as_slice())] {
        let index = DocumentIndex::collect(&root, None, features, SourceReadtable::Standard);
        let workspace = WorkspaceIndex::new([("source", &index)]);
        let mut occurrences = OccurrenceResolver::new(&index, &root, &workspace, None);
        for token in super::queries::candidate_symbol_tokens(&root) {
            let offset = token.text_range().start();
            let fresh = index.resolve_occurrence_with(offset, &workspace, |range| {
                find_top_level_form_near(&root, range, token.clone().into())
            });
            assert_eq!(occurrences.resolve(&token), fresh, "{token:?}");
        }
    }
}

pub(crate) fn parse_setf_function_name(element: &Element) -> Option<(SymbolName, TextRange)> {
    let node = element.as_node()?;
    if !closed_list(node) {
        return None;
    }
    let mut parts = elements(node);
    let first = parts.next()?;
    let first_sym = symbol(&first)?;
    if first_sym.standard_operator() != Some("SETF") {
        return None;
    }
    let target = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let sym = symbol(&target)?;
    Some((sym, target.text_range()))
}

pub(crate) fn find_top_level_form(root: &SyntaxNode, target: TextRange) -> Option<Element> {
    find_element_in_node(root, target)
}

/// Reuses the path to an element already visited by an analysis traversal.
pub(crate) fn find_top_level_form_near(
    root: &SyntaxNode,
    target: TextRange,
    near: Element,
) -> Option<Element> {
    near.ancestors()
        .take_while(|node| node != root)
        .filter(|node| node.text_range() == target)
        .last()
        .map(Element::Node)
        .or_else(|| (near.text_range() == target).then_some(near))
        // Inclusive ranges can select a sibling ending where `near` starts.
        .or_else(|| find_top_level_form(root, target))
}

fn find_element_in_node(node: &SyntaxNode, target: TextRange) -> Option<Element> {
    let child = node.child_or_token_at_range(target)?;
    if child.text_range() == target {
        return is_element_kind(child.kind()).then_some(child);
    }
    let child = child.into_node()?;
    matches!(child.kind(), SyntaxKind::ReaderForm | SyntaxKind::List)
        .then(|| find_element_in_node(&child, target))?
}

pub(crate) enum ReaderConditional {
    Active(Element),
    Inactive,
    Unknown(Element),
}

pub(crate) fn reader_conditional(
    form: &Element,
    known_features: Option<&[String]>,
) -> Option<ReaderConditional> {
    let conditional = FeatureConditional::from_node(form.as_node()?)?;
    Some(match conditional.is_active(known_features) {
        Some(true) => ReaderConditional::Active(conditional.guarded.into_element()),
        Some(false) => ReaderConditional::Inactive,
        None => ReaderConditional::Unknown(conditional.guarded.into_element()),
    })
}

pub(crate) fn possibly_read(
    mut element: Element,
    known_features: Option<&[String]>,
) -> Option<(Element, bool)> {
    let mut known = true;
    while let Some(conditional) = reader_conditional(&element, known_features) {
        match conditional {
            ReaderConditional::Active(guarded) => element = guarded,
            ReaderConditional::Inactive => return None,
            ReaderConditional::Unknown(guarded) => {
                known = false;
                element = guarded;
            }
        }
    }
    Some((element, known))
}

pub(crate) fn lambda_list_signature(params: &Element) -> Option<Arc<[String]>> {
    match params {
        Element::Node(list) if closed_list(list) => elements(list)
            .map(|param| (param.kind() != SyntaxKind::ReaderForm).then(|| param.to_string()))
            .collect(),
        Element::Token(token) if token.text().eq_ignore_ascii_case("NIL") => Some(Arc::from([])),
        _ => None,
    }
}

fn extract_lambda_list_and_docstring(
    list: &SyntaxNode,
    operator: &str,
) -> (Option<Arc<[String]>>, Option<String>) {
    let mut parts = elements(list).skip(2);
    match operator {
        "DEFUN" | "DEFMACRO" | "DEFGENERIC" | "DEFTYPE" | "DEFPATTERN" => {
            let Some(params_el) = parts.next() else {
                return (None, None);
            };
            let params = lambda_list_signature(&params_el);
            let doc = find_docstring(&mut parts, operator != "DEFGENERIC");
            (params, doc)
        }
        "DEFVAR" | "DEFPARAMETER" | "DEFCONSTANT" => {
            let _init = parts.next();
            let doc = parts
                .next()
                .and_then(|el| el.into_token())
                .and_then(|t| string_literal(&t));
            (None, doc)
        }
        _ => (None, None),
    }
}

fn find_docstring(
    mut body: impl Iterator<Item = Element>,
    require_following_form: bool,
) -> Option<String> {
    while let Some(el) = body.next() {
        if let Some(node) = el.as_node()
            && crate::outline::with_list_operator(node, |op| op == "DECLARE").unwrap_or(false)
        {
            continue;
        }
        if let Some(tok) = el.as_token()
            && (!require_following_form || body.next().is_some())
        {
            return string_literal(tok);
        }
        break;
    }
    None
}

/// Resolved callable metadata shared by signature help, arity checking, and inlay hints.
pub(crate) struct CallableSignature<'a> {
    pub(crate) name: Cow<'a, str>,
    lambda_list: LambdaList<'a>,
    pub(crate) documentation: Option<&'a str>,
    pub(crate) evaluates_arguments: bool,
    pub(crate) standard_name: Option<&'static str>,
    pub(crate) definition: Option<(Option<&'a str>, TextRange)>,
}

enum LambdaList<'a> {
    Source(&'a [String]),
    Standard(&'static [&'static str]),
}

impl<'a> CallableSignature<'a> {
    pub(crate) fn standard(name: &str) -> Option<Self> {
        let sig = builtin_signature(name)?;
        if !matches!(
            sig.kind,
            BuiltinKind::Function | BuiltinKind::Macro | BuiltinKind::SpecialOperator
        ) {
            return None;
        }
        Some(Self {
            name: Cow::Borrowed(sig.name),
            lambda_list: LambdaList::Standard(sig.params),
            documentation: (!sig.summary.is_empty()).then_some(sig.summary),
            evaluates_arguments: sig.kind == BuiltinKind::Function,
            standard_name: Some(sig.name),
            definition: None,
        })
    }

    pub(crate) fn anonymous(params: &'a [String]) -> Self {
        Self {
            name: Cow::Borrowed("lambda"),
            lambda_list: LambdaList::Source(params),
            documentation: None,
            evaluates_arguments: true,
            standard_name: None,
            definition: None,
        }
    }

    pub(crate) fn is_source(&self) -> bool {
        matches!(self.lambda_list, LambdaList::Source(_))
    }

    pub(crate) fn lambda_list(&self) -> impl Iterator<Item = &str> {
        let (source, standard) = match self.lambda_list {
            LambdaList::Source(params) => (params, &[][..]),
            LambdaList::Standard(params) => (&[][..], params),
        };
        source
            .iter()
            .map(String::as_str)
            .chain(standard.iter().copied())
    }
}

type WorkspaceDeclaration<'a> = (&'a str, &'a Declaration);

pub(crate) struct WorkspaceIndex<'a> {
    pub(crate) documents: Vec<(&'a str, &'a DocumentIndex)>,
    packages_by_name: FxHashMap<&'a str, Option<&'a PackageDefinition>>,
    canonical_packages: FxHashMap<&'a str, Option<&'a str>>,
    declarations_by_spelling: Option<FxHashMap<&'a str, Vec<WorkspaceDeclaration<'a>>>>,
    guarded_by_spelling: FxHashMap<&'a str, Vec<&'a Name>>,
    special_proclamation_names: FxHashSet<&'a str>,
}

enum InheritedExport<'a> {
    None,
    One(&'a str),
    Multiple { has_non_cl: bool },
}

impl<'docs> WorkspaceIndex<'docs> {
    pub(crate) fn new(
        documents: impl IntoIterator<Item = (&'docs str, &'docs DocumentIndex)>,
    ) -> Self {
        let documents: Vec<_> = documents.into_iter().collect();
        let mut special_proclamation_names = FxHashSet::default();
        for (_, document) in &documents {
            for (name, _, _) in &document.special_proclamations {
                special_proclamation_names.insert(name.spelling.name.as_str());
            }
        }
        let mut index = Self {
            documents,
            packages_by_name: FxHashMap::default(),
            canonical_packages: FxHashMap::default(),
            declarations_by_spelling: None,
            guarded_by_spelling: FxHashMap::default(),
            special_proclamation_names,
        };
        for (_, document) in &index.documents {
            for package in &document.packages {
                index
                    .packages_by_name
                    .entry(&package.name)
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(package));
                for name in std::iter::once(&package.name).chain(&package.nicknames) {
                    index
                        .canonical_packages
                        .entry(name)
                        .and_modify(|entry| {
                            if *entry != Some(package.name.as_str()) {
                                *entry = None;
                            }
                        })
                        .or_insert(Some(&package.name));
                }
            }
        }
        index
    }

    pub(crate) fn index_workspace_declarations(
        mut self,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Self> {
        // Full-document checks look up many symbols in the same collection
        // of documents. Other queries can use the per-document name indices.
        for &(_, document) in &self.documents {
            if cancelled() {
                return None;
            }
            for name in &document.guarded_callables {
                self.guarded_by_spelling
                    .entry(&name.spelling.name)
                    .or_default()
                    .push(name);
            }
        }
        if self.documents.len() > 1 {
            let mut declarations_by_spelling = FxHashMap::<_, Vec<_>>::default();
            for &(name, document) in &self.documents {
                if cancelled() {
                    return None;
                }
                for declaration in &document.declarations {
                    declarations_by_spelling
                        .entry(declaration.name.spelling.name.as_str())
                        .or_default()
                        .push((name, declaration));
                }
            }
            self.declarations_by_spelling = Some(declarations_by_spelling);
        }
        Some(self)
    }

    fn declarations_named<'a>(
        &'a self,
        spelling: &'a str,
    ) -> impl Iterator<Item = WorkspaceDeclaration<'docs>> + 'a {
        let (indexed, documents) = match &self.declarations_by_spelling {
            Some(index) => (index.get(spelling), &[][..]),
            None => (None, self.documents.as_slice()),
        };
        indexed
            .into_iter()
            .flatten()
            .copied()
            .chain(documents.iter().flat_map(move |&(name, document)| {
                document
                    .declarations_named(spelling)
                    .map(move |decl| (name, decl))
            }))
    }

    pub(crate) fn packages(&self) -> impl Iterator<Item = &PackageDefinition> {
        self.documents.iter().flat_map(|(_, index)| &index.packages)
    }

    pub(crate) fn can_resolve_package(&self, name: &str) -> bool {
        self.canonical_global_package(name).is_some_and(|name| {
            matches!(
                name,
                "COMMON-LISP" | "COMMON-LISP-USER" | simd_builtins::PACKAGE
            ) || self.package(name).is_some()
        })
    }

    pub(crate) fn canonical_global_package<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        let name = canonical_package(name);
        self.canonical_packages
            .get(name)
            .copied()
            .unwrap_or(Some(name))
    }

    pub(crate) fn resolve_package_prefix<'a>(
        &'a self,
        current: Option<&str>,
        prefix: &'a str,
    ) -> Option<&'a str> {
        if let Some(current) = current {
            let current = self.canonical_global_package(current)?;
            if let Some(info) = self.packages_by_name.get(current) {
                let info = (*info)?;
                if let Some(target) = info.local_nicknames.get(prefix) {
                    return self.canonical_global_package(target);
                }
            }
        } else if self.packages().any(|owner| {
            owner.local_nicknames.get(prefix).is_some_and(|target| {
                self.canonical_global_package(target) != self.canonical_global_package(prefix)
            })
        }) {
            return None;
        }
        self.canonical_global_package(prefix)
    }

    fn resolve_spelling_package<'a>(
        &'a self,
        spelling: &'a SymbolName,
        package: Option<&'a str>,
    ) -> Option<&'a str> {
        match &spelling.prefix {
            Prefix::None => self.canonical_global_package(package?),
            Prefix::Package { name: prefix, .. } => self.resolve_package_prefix(package, prefix),
            Prefix::Keyword | Prefix::Uninterned => None,
        }
    }

    pub(crate) fn package(&self, name: &str) -> Option<&PackageDefinition> {
        self.packages_by_name.get(name).copied().flatten()
    }

    pub(crate) fn may_have_declaration(&self, spelling: &str) -> bool {
        match &self.declarations_by_spelling {
            Some(index) => index.contains_key(spelling),
            None => self
                .documents
                .iter()
                .any(|(_, doc)| doc.declarations_named(spelling).next().is_some()),
        }
    }

    pub(crate) fn is_proclaimed_special(
        &self,
        spelling: &SymbolName,
        package: Option<&str>,
    ) -> bool {
        let may_be_builtin_var = builtin_kind(&spelling.name) == Some(BuiltinKind::Variable);
        let may_be_special_decl = self
            .special_proclamation_names
            .contains(spelling.name.as_str())
            || self.may_have_declaration(&spelling.name);
        if !may_be_builtin_var && !may_be_special_decl {
            return false;
        }
        let Some(home) = self.symbol_home_package(spelling, package) else {
            return false;
        };
        let same_home = |name: &Name| {
            name.spelling.name == spelling.name
                && self.symbol_home_package(&name.spelling, name.package.as_deref()) == Some(home)
        };
        (home == "COMMON-LISP" && may_be_builtin_var)
            || self.declarations_named(&spelling.name).any(|(_, decl)| {
                decl.name.namespace == Namespace::Value
                    && same_home(&decl.name)
                    && matches!(
                        self.standard_symbol(&decl.operator, decl.name.package.as_deref()),
                        Some("DEFVAR" | "DEFPARAMETER")
                    )
            })
            || (self
                .special_proclamation_names
                .contains(spelling.name.as_str())
                && self.documents.iter().any(|(_, index)| {
                    index
                        .special_proclamations
                        .iter()
                        .any(|(name, head, special)| {
                            same_home(name)
                                && self.standard_symbol(head, name.package.as_deref())
                                    == Some("DECLAIM")
                                && self.standard_symbol(special, name.package.as_deref())
                                    == Some("SPECIAL")
                        })
                }))
    }

    fn package_after_imports<'a>(&'a self, package: &'a str, spelling: &str) -> Option<&'a str> {
        let mut current = self.canonical_global_package(package)?;
        let mut visited: Option<Vec<&'a str>> = None;
        loop {
            let Some(info) = self.package(current) else {
                return Some(current);
            };
            if info.imports.is_empty() {
                return Some(current);
            }
            let mut imports = info.imports.iter().filter(|import| import.name == spelling);
            let Some(first) = imports.next() else {
                return Some(current);
            };
            let visited = visited.get_or_insert_with(Vec::new);
            if visited.contains(&current) {
                return None;
            }
            visited.push(current);
            let source = self.canonical_global_package(&first.source)?;
            if imports
                .filter_map(|import| self.canonical_global_package(&import.source))
                .any(|other| other != source)
            {
                return None;
            }
            current = source;
        }
    }

    pub(crate) fn declaration_package<'a>(
        &'a self,
        declaration: &'a Declaration,
    ) -> Option<&'a str> {
        let package = self.resolve_spelling_package(
            &declaration.name.spelling,
            declaration.name.package.as_deref(),
        )?;
        self.package_after_imports(package, &declaration.name.spelling.name)
    }

    fn has_declaration_in_package(&self, package: &str, spelling: &str) -> bool {
        if !self.may_have_declaration(spelling) {
            return false;
        }
        self.declarations_named(spelling).any(|(_, declaration)| {
            self.resolve_spelling_package(
                &declaration.name.spelling,
                declaration.name.package.as_deref(),
            ) == Some(package)
        })
    }

    fn inherited_export_packages<'a>(
        &'a self,
        info: &'a PackageDefinition,
        spelling: &str,
    ) -> Option<InheritedExport<'a>> {
        let mut result = InheritedExport::None;
        for used in &info.uses {
            let used = self.canonical_global_package(used)?;
            if !self.exports_symbol(used, spelling) {
                continue;
            }
            let package = self.package_after_imports(used, spelling)?;
            result = match result {
                InheritedExport::None => InheritedExport::One(package),
                InheritedExport::One(first) if first == package => InheritedExport::One(first),
                InheritedExport::One(first) => InheritedExport::Multiple {
                    has_non_cl: first != "COMMON-LISP" || package != "COMMON-LISP",
                },
                InheritedExport::Multiple { has_non_cl } => InheritedExport::Multiple {
                    has_non_cl: has_non_cl || package != "COMMON-LISP",
                },
            };
        }
        Some(result)
    }

    fn exports_symbol(&self, package: &str, spelling: &str) -> bool {
        if package == "COMMON-LISP" {
            return is_external_cl_symbol(spelling);
        }
        match self.packages_by_name.get(package) {
            Some(info) => info.is_some_and(|info| info.exports.iter().any(|name| name == spelling)),
            None => match package {
                "ZK-FFI" => crate::foreign::signature(spelling).is_some(),
                simd_builtins::PACKAGE => {
                    simd_builtins::function(spelling).is_some()
                        || simd_builtins::type_documentation(spelling).is_some()
                }
                treelist_builtins::PACKAGE => {
                    treelist_builtins::callable(spelling).is_some()
                        || treelist_builtins::type_documentation(spelling).is_some()
                }
                _ => false,
            },
        }
    }

    fn allows_declaration_lookup(&self, package: &str, spelling: &str, external: bool) -> bool {
        let info = match self.packages_by_name.get(package) {
            None => return !external || self.exports_symbol(package, spelling),
            Some(Some(info)) => info,
            Some(None) => return false,
        };
        if external && !info.exports.iter().any(|s| s == spelling) {
            return false;
        }
        // An inherited name coinciding with a declaration from another
        // package is ambiguous without a complete load and package graph.
        info.shadows.iter().any(|s| s == spelling)
            || !info.uses.iter().any(|used| {
                self.canonical_global_package(used)
                    .and_then(|used| self.package(used))
                    .is_some_and(|used| used.exports.iter().any(|s| s == spelling))
            })
    }

    pub(crate) fn standard_symbol<'a>(
        &self,
        spelling: &'a SymbolName,
        package: Option<&str>,
    ) -> Option<&'a str> {
        if !is_external_cl_symbol(&spelling.name) {
            return None;
        }
        match &spelling.prefix {
            Prefix::Package { name, external } => {
                let target_pkg = self.resolve_package_prefix(package, name)?;
                (self.resolve_symbol_home_in_package(target_pkg, &spelling.name, *external)
                    == Some("COMMON-LISP"))
                .then_some(&spelling.name)
            }
            Prefix::None => {
                let Some(package) = package else {
                    return Some(&spelling.name);
                };
                let canon = self.canonical_global_package(package)?;
                match self.packages_by_name.get(canon) {
                    Some(_) => (self.resolve_symbol_home_in_package(canon, &spelling.name, false)
                        == Some("COMMON-LISP"))
                    .then_some(&spelling.name),
                    None => match canon {
                        "COMMON-LISP" => Some(&spelling.name),
                        "COMMON-LISP-USER" => (!self
                            .has_declaration_in_package(canon, &spelling.name))
                        .then_some(&spelling.name),
                        simd_builtins::PACKAGE | treelist_builtins::PACKAGE => None,
                        _ => Some(&spelling.name),
                    },
                }
            }
            Prefix::Keyword | Prefix::Uninterned => None,
        }
    }

    /// Resolve a target symbol's canonical home package and check accessibility.
    /// Supports direct declarations, imported identity, and inherited external
    /// symbols. Multiple inheritance paths are accepted only when they reach the
    /// same symbol identity. An explicit single colon requires an export from
    /// the named package, while an unqualified name or double colon can inherit.
    pub(crate) fn resolve_symbol_home_in_package<'a>(
        &'a self,
        package: &'a str,
        spelling: &str,
        external: bool,
    ) -> Option<&'a str> {
        let info = match self.packages_by_name.get(package) {
            None => {
                if package == "COMMON-LISP-USER"
                    && !external
                    && is_external_cl_symbol(spelling)
                    && !self.has_declaration_in_package(package, spelling)
                {
                    return Some("COMMON-LISP");
                }
                return (!external || self.exports_symbol(package, spelling)).then_some(package);
            }
            Some(Some(info)) => info,
            Some(None) => return None,
        };
        if external && !info.exports.iter().any(|export| export == spelling) {
            return None;
        }
        // Fast path for the common case: unqualified external CL symbol in a package
        // that uses only COMMON-LISP and has no workspace declarations/shadows/imports for it.
        if !external
            && is_external_cl_symbol(spelling)
            && info.uses.len() == 1
            && canonical_package(&info.uses[0]) == "COMMON-LISP"
            && self
                .canonical_packages
                .get("COMMON-LISP")
                .copied()
                .unwrap_or(Some("COMMON-LISP"))
                == Some("COMMON-LISP")
            && !self.may_have_declaration(spelling)
            && info.shadows.iter().all(|shadow| shadow != spelling)
            && (info.imports.is_empty() || info.imports.iter().all(|imp| imp.name != spelling))
        {
            return Some("COMMON-LISP");
        }
        let imported_home = self.package_after_imports(package, spelling)?;
        if imported_home != package {
            return Some(imported_home);
        }
        if info.shadows.iter().any(|shadow| shadow == spelling) {
            return Some(package);
        }
        let has_local = self.has_declaration_in_package(package, spelling);
        let inherited = self.inherited_export_packages(info, spelling)?;
        if has_local {
            if matches!(inherited, InheritedExport::One(p) if p != "COMMON-LISP")
                || matches!(inherited, InheritedExport::Multiple { has_non_cl: true })
            {
                return None;
            }
            return Some(package);
        }
        match inherited {
            InheritedExport::None => Some(package),
            InheritedExport::One(first) => Some(first),
            InheritedExport::Multiple { .. } => None,
        }
    }

    pub(crate) fn symbol_home_package<'a>(
        &'a self,
        spelling: &'a SymbolName,
        package: Option<&'a str>,
    ) -> Option<&'a str> {
        let (package, external) = match &spelling.prefix {
            Prefix::None => (self.canonical_global_package(package?)?, false),
            Prefix::Package { name, external } => {
                (self.resolve_package_prefix(package, name)?, *external)
            }
            Prefix::Keyword => return Some("KEYWORD"),
            Prefix::Uninterned => return None,
        };
        self.resolve_symbol_home_in_package(package, &spelling.name, external)
    }

    pub(crate) fn declaration_matches(&self, package: &str, declaration: &Declaration) -> bool {
        if self.declaration_package(declaration) != Some(package) {
            return false;
        }
        let external = matches!(
            declaration.name.spelling.prefix,
            Prefix::Package { external: true, .. }
        );
        self.allows_declaration_lookup(package, &declaration.name.spelling.name, external)
            && (declaration.operator.zk_definition().is_some()
                || self
                    .standard_symbol(&declaration.operator, declaration.name.package.as_deref())
                    .is_some())
    }

    pub(crate) fn lookup_unique_declaration(
        &self,
        name: &Name,
    ) -> Option<WorkspaceDeclaration<'docs>> {
        self.lookup_unique_declaration_parts(
            name.namespace,
            &name.spelling,
            name.package.as_deref(),
        )
    }

    pub(crate) fn lookup_unique_declaration_parts(
        &self,
        namespace: Namespace,
        spelling: &SymbolName,
        package: Option<&str>,
    ) -> Option<WorkspaceDeclaration<'docs>> {
        let mut declarations = self.matching_declarations(namespace, spelling, package);
        let first = declarations.next()?;
        declarations.next().is_none().then_some(first)
    }

    fn matching_declarations<'a>(
        &'a self,
        namespace: Namespace,
        spelling: &'a SymbolName,
        package: Option<&'a str>,
    ) -> impl Iterator<Item = WorkspaceDeclaration<'docs>> + 'a {
        let mut declarations = self
            .declarations_named(&spelling.name)
            .filter(move |(_, declaration)| declaration.name.namespace == namespace)
            .peekable();
        let home = declarations.peek().and_then(|_| {
            let raw_pkg = self.resolve_spelling_package(spelling, package)?;
            let external = matches!(spelling.prefix, Prefix::Package { external: true, .. });
            let resolved =
                self.resolve_symbol_home_in_package(raw_pkg, &spelling.name, external)?;
            self.allows_declaration_lookup(
                resolved,
                &spelling.name,
                external && resolved == raw_pkg,
            )
            .then_some(resolved)
        });
        declarations.filter(move |(_, declaration)| {
            home.is_some_and(|home| self.declaration_matches(home, declaration))
        })
    }

    pub(crate) fn may_define_callable(&self, spelling: &SymbolName, package: &str) -> bool {
        self.matching_declarations(Namespace::Function, spelling, Some(package))
            .next()
            .is_some()
            || (self
                .guarded_by_spelling
                .contains_key(spelling.name.as_str())
                && self
                    .symbol_home_package(spelling, Some(package))
                    .is_some_and(|home| self.has_guarded_callable(&spelling.name, home)))
    }

    fn has_guarded_callable(&self, spelling: &str, home: &str) -> bool {
        self.guarded_by_spelling
            .get(spelling)
            .is_some_and(|guarded| {
                guarded.iter().any(|name| {
                    self.symbol_home_package(&name.spelling, name.package.as_deref())
                        .is_none_or(|candidate| candidate == home)
                })
            })
    }

    pub(crate) fn lookup_macro_template(
        &self,
        spelling: &SymbolName,
        package: Option<&str>,
        source: &DocumentIndex,
        at: TextSize,
    ) -> Option<(Option<&'docs str>, &'docs Declaration, &'docs MacroTemplate)> {
        if !self
            .declarations_named(&spelling.name)
            .any(|(_, decl)| decl.macro_definition.is_some())
            || !source.package_known_at(at)
            || package.is_some_and(|package| !self.can_resolve_package(package))
        {
            return None;
        }
        let (document, declaration) = self.lookup_unique_declaration_parts(
            Namespace::Function,
            spelling,
            Some(package.unwrap_or("COMMON-LISP-USER")),
        )?;
        let definition = declaration.macro_definition.as_deref()?;
        if self
            .guarded_by_spelling
            .contains_key(spelling.name.as_str())
            && self.has_guarded_callable(&spelling.name, self.declaration_package(declaration)?)
        {
            return None;
        }
        let local = source
            .declarations_named(&spelling.name)
            .any(|candidate| std::ptr::eq(candidate, declaration));
        if local && declaration.range.start() >= at {
            return None;
        }
        Some((
            (!local).then_some(document),
            declaration,
            definition.template()?,
        ))
    }

    /// Resolve a called operator symbol via lexical FLET/LABELS binding first,
    /// then workspace declarations and built-ins.
    pub(crate) fn resolve_callable<'a>(
        &'a self,
        head_sym: &SymbolName,
        package: Option<&str>,
        local_fn: Option<&'a ScopeBinding>,
    ) -> Option<CallableSignature<'a>> {
        self.resolve_callable_with_standard(head_sym, package, local_fn, None)
    }

    /// Resolve a called operator symbol when the caller may have already
    /// resolved a canonical COMMON-LISP standard operator name.
    pub(crate) fn resolve_callable_with_standard<'a>(
        &'a self,
        head_sym: &SymbolName,
        package: Option<&str>,
        local_fn: Option<&'a ScopeBinding>,
        known_standard: Option<&'a str>,
    ) -> Option<CallableSignature<'a>> {
        if let Some(binding) = local_fn {
            return binding.params.as_ref().map(|params| CallableSignature {
                name: Cow::Borrowed(&binding.spelling.name),
                lambda_list: LambdaList::Source(params),
                documentation: None,
                evaluates_arguments: binding.evaluates_arguments,
                standard_name: None,
                definition: binding.def_range.map(|range| (None, range)),
            });
        }

        if let Some((document, decl)) =
            self.lookup_unique_declaration_parts(Namespace::Function, head_sym, package)
            && let Some(params) = &decl.params
        {
            return Some(CallableSignature {
                name: Cow::Borrowed(&decl.name.spelling.name),
                lambda_list: LambdaList::Source(params),
                documentation: decl.documentation.as_deref(),
                evaluates_arguments: decl.evaluates_arguments,
                standard_name: None,
                definition: Some((Some(document), decl.range)),
            });
        }

        let std_name = if let Some(std_name) = known_standard {
            std_name
        } else {
            if let Some((name, params)) = crate::foreign::signature(&head_sym.name)
                && self.symbol_home_package(head_sym, package) == Some("ZK-FFI")
            {
                return Some(CallableSignature {
                    name: Cow::Borrowed(name),
                    lambda_list: LambdaList::Standard(params),
                    documentation: None,
                    evaluates_arguments: head_sym.name != "WITH-FOREIGN-MEMORY",
                    standard_name: None,
                    definition: None,
                });
            }
            if let Some(function) = simd_builtins::function(&head_sym.name)
                && self.symbol_home_package(head_sym, package) == Some(simd_builtins::PACKAGE)
            {
                return Some(CallableSignature {
                    name: Cow::Borrowed(function.name),
                    lambda_list: LambdaList::Standard(function.params),
                    documentation: Some(function.documentation),
                    evaluates_arguments: true,
                    standard_name: None,
                    definition: None,
                });
            }
            if let Some(callable) = treelist_builtins::callable(&head_sym.name)
                && (self.symbol_home_package(head_sym, package) == Some(treelist_builtins::PACKAGE)
                    || matches!(
                        head_sym.name.as_str(),
                        "TREELIST" | "MUTABLE-TREELIST" | "TREELIST-P" | "MUTABLE-TREELIST-P"
                    ) && self.symbol_home_package(head_sym, package) == Some("ZK"))
            {
                return Some(CallableSignature {
                    name: Cow::Borrowed(callable.name),
                    lambda_list: LambdaList::Standard(callable.params),
                    documentation: Some(callable.documentation),
                    evaluates_arguments: callable.evaluates_arguments,
                    standard_name: None,
                    definition: None,
                });
            }
            self.standard_symbol(head_sym, package)?
        };
        CallableSignature::standard(std_name)
    }

    pub(crate) fn is_symbol_macro(&self, spelling: &SymbolName, package: Option<&str>) -> bool {
        self.declarations_named(&spelling.name).any(|(_, decl)| {
            decl.operator.name == "DEFINE-SYMBOL-MACRO"
                && self
                    .symbol_home_package(spelling, package)
                    .is_some_and(|home| self.declaration_matches(home, decl))
        })
    }
}

pub(crate) fn symbol(element: &Element) -> Option<SymbolName> {
    let token = element.as_token()?;
    (token.kind() == SyntaxKind::Atom).then(|| SymbolName::parse(token.text()))?
}

pub(crate) fn operator(list: &SyntaxNode) -> Option<String> {
    list_operator(list)
}

pub(crate) fn closed_list(list: &SyntaxNode) -> bool {
    non_dotted_list(list)
        && list
            .green()
            .children()
            .next_back()
            .is_some_and(|element| element.kind().0 == SyntaxKind::CloseParen as u16)
}

pub(crate) fn non_dotted_list(list: &SyntaxNode) -> bool {
    list.kind() == SyntaxKind::List && !list_has_dot(list)
}

fn package_definition(list: &SyntaxNode) -> Option<PackageDefinition> {
    let mut forms = elements(list).skip(1);
    let name = string_designator(&forms.next()?)?;
    let mut package = PackageDefinition {
        name,
        nicknames: Vec::new(),
        local_nicknames: FxHashMap::default(),
        exports: Vec::new(),
        imports: Vec::new(),
        shadows: Vec::new(),
        uses: Vec::new(),
        symbol_designators: Vec::new(),
    };
    for option in forms {
        let list = option.into_node()?;
        if !closed_list(&list) {
            return None;
        }
        let mut arguments = elements(&list);
        let head = symbol(&arguments.next()?)?;
        if head.prefix != Prefix::Keyword {
            return None;
        }
        let target = match head.name.as_str() {
            "NICKNAMES" => &mut package.nicknames,
            "LOCAL-NICKNAMES" => {
                for argument in arguments {
                    let pair = argument.into_node()?;
                    if !closed_list(&pair) || list_has_dot(&pair) {
                        return None;
                    }
                    let mut fields = elements(&pair);
                    let alias = string_designator(&fields.next()?)?;
                    let target = string_designator(&fields.next()?)?;
                    if fields.next().is_some() {
                        return None;
                    }
                    if let Some(previous) = package.local_nicknames.insert(alias, target.clone())
                        && previous != target
                    {
                        return None;
                    }
                }
                continue;
            }
            "EXPORT" => &mut package.exports,
            "SHADOW" => &mut package.shadows,
            "IMPORT-FROM" | "SHADOWING-IMPORT-FROM" => {
                let source = string_designator(&arguments.next()?)?;
                for argument in arguments {
                    let name = string_designator(&argument)?;
                    package.symbol_designators.push(PackageSymbolDesignator {
                        name: name.clone(),
                        range: argument.text_range(),
                        import_source: Some(source.clone()),
                    });
                    package.imports.push(PackageImport {
                        source: source.clone(),
                        name,
                    });
                }
                continue;
            }
            "INTERN" => {
                for argument in arguments {
                    package.symbol_designators.push(PackageSymbolDesignator {
                        name: string_designator(&argument)?,
                        range: argument.text_range(),
                        import_source: None,
                    });
                }
                continue;
            }
            "USE" => &mut package.uses,
            _ => continue,
        };
        for argument in arguments {
            let name = string_designator(&argument)?;
            if matches!(head.name.as_str(), "EXPORT" | "SHADOW") {
                package.symbol_designators.push(PackageSymbolDesignator {
                    name: name.clone(),
                    range: argument.text_range(),
                    import_source: None,
                });
            }
            target.push(name);
        }
    }
    Some(package)
}
