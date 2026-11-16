//! Operand roles proven by the returned template of a source-defined macro.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::cl_builtins::{BuiltinKind, builtin_kind};
use crate::navigation::index::{DocumentIndex, WorkspaceIndex, closed_list, symbol};
use crate::outline::{Element, elements};
use crate::reader_tokens::{Prefix, SymbolName};
use crate::syntax::{SyntaxKind, TextRange, TextSize};

use super::context::is_nil_syntax;

mod parameters;

use parameters::{Actual, Parameters, read_list, variable};

const MAX_PARTS: usize = 512;
const MAX_DEPTH: usize = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MacroTemplate {
    parameters: Parameters,
    slots: usize,
    requirements: Vec<Requirement>,
    projections: Vec<Projection>,
    blocks: Vec<TemplateBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Requirement {
    name: SymbolName,
    in_expansion: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OperandRole {
    Inherited,
    Value,
    Test,
    Discarded,
    Place,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TemplateBody {
    Arguments,
    Forms,
    Declared,
    Documented,
    Tagbody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EvalContext {
    pub(super) usage: OperandRole,
    pub(super) top_level: bool,
    pub(super) binds_values: bool,
    pub(super) deferred: bool,
    pub(super) in_loop: bool,
    pub(super) nil_block: bool,
    pub(super) open_guard: bool,
    block: Option<usize>,
}

impl EvalContext {
    fn nested(self, usage: OperandRole) -> Self {
        Self {
            usage,
            top_level: false,
            ..self
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Projection {
    slot: usize,
    range: TextRange,
    context: EvalContext,
    body: Option<TemplateBody>,
    spliced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TemplateBlock {
    parent: Option<usize>,
    name: BlockName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BlockName {
    Literal(SymbolName),
    Operand(usize),
}

pub(super) struct Operand {
    pub(super) forms: Vec<Element>,
    pub(super) context: EvalContext,
    pub(super) body: Option<TemplateBody>,
    pub(super) origin: TextRange,
    /// The second field says whether the name comes from the template.
    pub(super) blocks: Vec<(SymbolName, bool)>,
}

impl MacroTemplate {
    /// Analyze the expansion-producing fragment once per source snapshot. No
    /// Lisp code is evaluated, even when the macro itself runs at compile time.
    pub(crate) fn infer(definition_tail: impl IntoIterator<Item = Element>) -> Option<Self> {
        let mut fields = definition_tail.into_iter();
        let parameters = fields.next()?;
        let body: Vec<_> = fields.take(MAX_PARTS + 1).collect();
        let mut remaining = MAX_PARTS;
        for field in std::iter::once(&parameters).chain(&body) {
            if let Some(node) = field.as_node() {
                for part in node.descendants_with_tokens() {
                    remaining = remaining.checked_sub(1)?;
                    if matches!(
                        part.kind(),
                        SyntaxKind::Error
                            | SyntaxKind::UnknownDispatch
                            | SyntaxKind::ReaderConditional
                    ) || part.into_token().is_some_and(|token| {
                        token.kind() == SyntaxKind::DispatchPrefix && token.text().starts_with("#.")
                    }) {
                        return None;
                    }
                }
            } else {
                remaining = remaining.checked_sub(1)?;
            }
        }
        let mut builder = Builder::default();
        let parameters = builder.parameters(&parameters, 0)?;
        let template = builder.returned_template(&body)?;
        builder.collect_holes(&template)?;
        if builder.holes.is_empty() {
            return None;
        }
        builder.form(
            &template,
            EvalContext {
                usage: OperandRole::Inherited,
                top_level: true,
                binds_values: false,
                deferred: false,
                in_loop: false,
                nil_block: false,
                open_guard: false,
                block: None,
            },
            None,
            0,
        );
        // A caller operand that is also inspected as data, or expanded twice,
        // cannot be rewritten using the context of just one insertion.
        builder
            .projections
            .retain(|projection| builder.counts[projection.slot] == 1);
        if builder.projections.is_empty() {
            return None;
        }
        Some(Self {
            parameters,
            slots: builder.parameters_by_name.len(),
            requirements: builder.requirements,
            projections: builder.projections,
            blocks: builder.blocks,
        })
    }

    pub(crate) fn shifted(mut self, offset: TextSize) -> Self {
        if offset != TextSize::from(0) {
            for projection in &mut self.projections {
                projection.range += offset;
            }
        }
        self
    }

    pub(super) fn supports_definition(
        &self,
        workspace: &WorkspaceIndex<'_>,
        package: Option<&str>,
        shadowed: impl Fn(&SymbolName) -> bool,
    ) -> bool {
        package.is_none_or(|package| workspace.can_resolve_package(package))
            && self.requirements.iter().all(|requirement| {
                workspace
                    .standard_symbol(&requirement.name, package)
                    .is_some()
                    && (requirement.in_expansion || !shadowed(&requirement.name))
            })
    }

    pub(super) fn supports_expansion(&self, shadowed: impl Fn(&str) -> bool) -> bool {
        self.requirements
            .iter()
            .all(|requirement| !requirement.in_expansion || !shadowed(&requirement.name.name))
    }

    pub(super) fn operands(
        &self,
        source: &DocumentIndex,
        workspace: &WorkspaceIndex<'_>,
        args: &[Element],
    ) -> Option<Vec<Operand>> {
        let mut actuals: Vec<_> = (0..self.slots).map(|_| Actual::Missing).collect();
        self.parameters
            .bind(source, workspace, args, &mut actuals)?;
        let mut operands = Vec::with_capacity(self.projections.len());
        for projection in &self.projections {
            let mut blocks = Vec::new();
            let mut enclosing = projection.context.block;
            while let Some(index) = enclosing {
                let block = &self.blocks[index];
                match &block.name {
                    BlockName::Literal(name) => blocks.push((name.clone(), true)),
                    BlockName::Operand(slot) => match &actuals[*slot] {
                        Actual::Form(form) => {
                            let name = if is_nil_syntax(
                                form,
                                source.package_at(form.text_range().start()),
                                workspace,
                            ) {
                                SymbolName {
                                    prefix: Prefix::Package {
                                        name: "COMMON-LISP".into(),
                                        external: true,
                                    },
                                    name: "NIL".into(),
                                }
                            } else {
                                symbol(form)?
                            };
                            blocks.push((name, false));
                        }
                        Actual::Missing | Actual::Rest(_) => return None,
                    },
                }
                enclosing = block.parent;
            }
            let actual = std::mem::replace(&mut actuals[projection.slot], Actual::Missing);
            let forms = match (projection.spliced, actual) {
                (_, Actual::Missing) => continue,
                (false, Actual::Form(form)) => vec![form],
                (true, Actual::Rest(forms)) => forms,
                (true, Actual::Form(form)) => read_list(source, workspace, &form)?,
                (false, Actual::Rest(_)) => return None,
            };
            if !forms.is_empty() {
                operands.push(Operand {
                    forms,
                    context: projection.context,
                    body: projection.body,
                    origin: projection.range,
                    blocks,
                });
            }
        }
        Some(operands)
    }
}

#[derive(Default)]
struct Builder {
    parameters_by_name: FxHashMap<SymbolName, usize>,
    operand_aliases: FxHashMap<SymbolName, usize>,
    auxiliary: FxHashSet<SymbolName>,
    fresh: Vec<SymbolName>,
    requirements: Vec<Requirement>,
    holes: FxHashMap<TextRange, (usize, bool)>,
    counts: Vec<usize>,
    projections: Vec<Projection>,
    blocks: Vec<TemplateBlock>,
}

impl Builder {
    fn require(&mut self, name: SymbolName, in_expansion: bool) {
        let requirement = Requirement { name, in_expansion };
        if !self.requirements.contains(&requirement) {
            self.requirements.push(requirement);
        }
    }

    fn skip_ignore_declarations<'a>(&mut self, mut body: &'a [Element]) -> Option<&'a [Element]> {
        loop {
            let Some(first) = body.first() else {
                return Some(body);
            };
            if let Some((head, clauses)) = list(first)
                && head.name == "DECLARE"
            {
                self.require(head, false);
                for clause in clauses {
                    let (head, _) = list(&clause)?;
                    if !matches!(head.name.as_str(), "IGNORE" | "IGNORABLE") {
                        return None;
                    }
                    self.require(head, false);
                }
                body = &body[1..];
            } else {
                return Some(body);
            }
        }
    }

    fn returned_template(&mut self, body: &[Element]) -> Option<Element> {
        let mut body = body;
        if let [doc, rest @ ..] = body
            && matches!(doc.kind(), SyntaxKind::String | SyntaxKind::RawString)
        {
            body = rest;
        }
        body = self.skip_ignore_declarations(body)?;
        let [result] = body else { return None };
        let mut result = result.clone();
        for _ in 0..MAX_DEPTH {
            if let Some(form) = reader_operand(&result, SyntaxKind::Backquote) {
                return Some(form);
            }
            // The common hygienic wrapper binds fresh symbols and has a
            // single result. Arbitrary expansion-time computation is opaque.
            let (head, args) = list(&result)?;
            if matches!(head.name.as_str(), "PROGN" | "LOCALLY")
                && let [next] = args.as_slice()
            {
                self.require(head, false);
                result = next.clone();
                continue;
            }
            let (bindings, rest) = args.split_first()?;
            if !matches!(head.name.as_str(), "LET" | "LET*") {
                return None;
            }
            let rest = self.skip_ignore_declarations(rest)?;
            let [next] = rest else {
                return None;
            };
            let sequential = head.name == "LET*";
            self.require(head, false);
            let mut pending_aliases = Vec::new();
            for binding in elements(bindings.as_node().filter(|node| closed_list(node))?) {
                let (var, init) = let_binding(&binding)?;
                let name = variable(&var)?;
                let init = init?;
                if self.parameters_by_name.contains_key(&name)
                    || self.auxiliary.contains(&name)
                    || self.fresh.contains(&name)
                    || self.operand_aliases.contains_key(&name)
                    || pending_aliases.iter().any(|(pending, _)| *pending == name)
                {
                    return None;
                }
                if let Some(slot) = symbol(&init).and_then(|init| self.operand_slot(&init)) {
                    if sequential {
                        self.operand_aliases.insert(name, slot);
                    } else {
                        pending_aliases.push((name, slot));
                    }
                } else {
                    self.bind_fresh(name, &init)?;
                }
            }
            self.operand_aliases.extend(pending_aliases);
            result = next.clone();
        }
        None
    }

    fn operand_slot(&self, name: &SymbolName) -> Option<usize> {
        self.parameters_by_name
            .get(name)
            .or_else(|| self.operand_aliases.get(name))
            .copied()
    }

    fn bind_fresh(&mut self, name: SymbolName, initializer: &Element) -> Option<()> {
        let (op, args) = list(initializer)?;
        let string_argument = matches!(args.as_slice(), [arg] if matches!(arg.kind(), SyntaxKind::String | SyntaxKind::RawString));
        if !(matches!(op.name.as_str(), "GENSYM" | "MAKE-SYMBOL") && string_argument
            || op.name == "GENSYM" && args.is_empty())
        {
            return None;
        }
        self.require(op, false);
        self.fresh.push(name);
        Some(())
    }

    fn collect_holes(&mut self, template: &Element) -> Option<()> {
        self.counts.resize(self.parameters_by_name.len(), 0);
        let mut pending = vec![template.clone()];
        while let Some(element) = pending.pop() {
            let Some(node) = element.as_node() else {
                continue;
            };
            if node.kind() == SyntaxKind::ReaderForm {
                let prefix = node.first_token()?.kind();
                if matches!(
                    prefix,
                    SyntaxKind::Backquote | SyntaxKind::UnquoteDestructive
                ) {
                    return None;
                }
                if matches!(prefix, SyntaxKind::Unquote | SyntaxKind::UnquoteSplicing) {
                    let name = symbol(&reader_operand(&element, prefix)?)?;
                    if let Some(slot) = self.operand_slot(&name) {
                        self.holes.insert(
                            element.text_range(),
                            (slot, prefix == SyntaxKind::UnquoteSplicing),
                        );
                        self.counts[slot] += 1;
                    } else if prefix == SyntaxKind::UnquoteSplicing || !self.fresh.contains(&name) {
                        return None;
                    }
                    continue;
                }
            }
            pending.extend(elements(node));
        }
        Some(())
    }

    fn project(
        &mut self,
        element: &Element,
        context: EvalContext,
        body: Option<TemplateBody>,
    ) -> bool {
        let range = element.text_range();
        let Some(&(slot, spliced)) = self.holes.get(&range) else {
            return false;
        };
        if !spliced || body.is_some() {
            self.projections.push(Projection {
                slot,
                range,
                context,
                body,
                spliced,
            });
        }
        true
    }

    fn spliced(&self, element: &Element) -> bool {
        self.holes
            .get(&element.text_range())
            .is_some_and(|&(_, spliced)| spliced)
    }

    fn add_block(&mut self, name: &Element, context: &mut EvalContext) -> Option<()> {
        let name = if let Some(&(slot, false)) = self.holes.get(&name.text_range()) {
            BlockName::Operand(slot)
        } else if let Some(name) = symbol(name) {
            if name.prefix == Prefix::Uninterned {
                return Some(());
            }
            BlockName::Literal(name)
        } else if name
            .as_node()
            .is_some_and(|node| closed_list(node) && elements(node).next().is_none())
        {
            context.nil_block = true;
            return Some(());
        } else if reader_operand(name, SyntaxKind::Unquote)
            .and_then(|value| symbol(&value))
            .is_some_and(|name| self.fresh.contains(&name))
        {
            return Some(());
        } else {
            return None;
        };
        let index = self.blocks.len();
        self.blocks.push(TemplateBlock {
            parent: context.block,
            name,
        });
        context.block = Some(index);
        Some(())
    }

    fn form(
        &mut self,
        element: &Element,
        context: EvalContext,
        body: Option<TemplateBody>,
        depth: usize,
    ) {
        if depth == MAX_DEPTH
            || self.project(element, context, body)
            || context.usage == OperandRole::Place
        {
            return;
        }
        if let Some(function) = reader_operand(element, SyntaxKind::FunctionQuote) {
            if list(&function).is_some_and(|(head, _)| head.name == "LAMBDA") {
                self.form(
                    &function,
                    context.nested(OperandRole::Value),
                    None,
                    depth + 1,
                );
            }
            return;
        }
        if let Some(node) = element.as_node().filter(|node| closed_list(node)) {
            let mut fields = elements(node);
            if let Some(operator) = fields.next()
                && let Some((head, lambda_args)) = list(&operator)
                && head.name == "LAMBDA"
                && let [parameters, body @ ..] = lambda_args.as_slice()
                && !self.spliced(parameters)
                && fields.clone().all(|operand| !self.spliced(&operand))
            {
                self.require(head, true);
                let body_context = EvalContext {
                    binds_values: true,
                    ..context.nested(context.usage)
                };
                self.sequence(body, TemplateBody::Documented, body_context, depth + 1);
                let argument_context = context.nested(OperandRole::Value);
                for argument in fields {
                    self.form(
                        &argument,
                        argument_context,
                        Some(TemplateBody::Arguments),
                        depth + 1,
                    );
                }
                return;
            }
        }
        let Some((head, args)) = list(element) else {
            return;
        };
        let depth = depth + 1;
        let mut nested = context.nested(context.usage);
        let value = context.nested(OperandRole::Value);
        let test = context.nested(OperandRole::Test);
        let fixed = |builder: &Self, args: &[Element]| args.iter().all(|arg| !builder.spliced(arg));
        match (head.name.as_str(), args.as_slice()) {
            ("PROGN", args) => self.sequence(args, TemplateBody::Forms, context, depth),
            ("LOCALLY", args) => {
                let context = EvalContext {
                    binds_values: true,
                    ..context
                };
                self.sequence(args, TemplateBody::Declared, context, depth);
            }
            ("WHEN" | "UNLESS", [condition, body @ ..]) if !self.spliced(condition) => {
                self.form(condition, test, None, depth);
                self.sequence(body, TemplateBody::Forms, nested, depth);
            }
            ("IF", [condition, branches @ ..])
                if (1..=2).contains(&branches.len()) && fixed(self, &args) =>
            {
                self.form(condition, test, None, depth);
                for branch in branches {
                    self.form(branch, nested, None, depth);
                }
            }
            ("COND", clauses) if fixed(self, clauses) => {
                for clause in clauses {
                    let Some(node) = clause.as_node().filter(|node| closed_list(node)) else {
                        continue;
                    };
                    let fields: Vec<_> = elements(node).collect();
                    if let [condition, body @ ..] = fields.as_slice()
                        && !self.spliced(condition)
                    {
                        // A clause containing only its test can return that primary value.
                        let condition_context = if body.iter().any(|form| !self.spliced(form)) {
                            test
                        } else {
                            value
                        };
                        self.form(condition, condition_context, None, depth);
                        self.sequence(body, TemplateBody::Forms, nested, depth);
                    }
                }
            }
            ("AND" | "OR", args) if fixed(self, args) => {
                if let Some((last, first)) = args.split_last() {
                    let context = if head.name == "AND" { test } else { value };
                    for form in first {
                        self.form(form, context, None, depth);
                    }
                    self.form(last, nested, None, depth);
                }
            }
            ("LET" | "LET*", [bindings, body @ ..]) if !self.spliced(bindings) => {
                if let Some(node) = bindings.as_node().filter(|node| closed_list(node)) {
                    let mut init_context = value;
                    for spec in elements(node) {
                        if self.spliced(&spec) {
                            break;
                        }
                        if let Some(spec) = spec.as_node().filter(|node| closed_list(node)) {
                            let fields: Vec<_> = elements(spec).collect();
                            if let [_, init] = fields.as_slice()
                                && fixed(self, &fields)
                            {
                                self.form(init, init_context, None, depth);
                            }
                        }
                        init_context.binds_values |= head.name == "LET*";
                    }
                }
                nested.binds_values = true;
                self.sequence(body, TemplateBody::Declared, nested, depth);
            }
            (
                "CASE" | "ECASE" | "CCASE" | "TYPECASE" | "ETYPECASE" | "CTYPECASE",
                [key, clauses @ ..],
            ) if !self.spliced(key) && fixed(self, clauses) => {
                let key_context = if matches!(head.name.as_str(), "CCASE" | "CTYPECASE") {
                    value.nested(OperandRole::Place)
                } else {
                    value
                };
                self.form(key, key_context, None, depth);
                for clause in clauses {
                    if let Some(cnode) = clause.as_node().filter(|node| closed_list(node)) {
                        let fields: Vec<_> = elements(cnode).collect();
                        if let Some((keys, body)) = fields.split_first()
                            && !self.spliced(keys)
                            && !body.is_empty()
                        {
                            self.sequence(body, TemplateBody::Forms, nested, depth);
                        }
                    }
                }
            }
            ("MULTIPLE-VALUE-BIND" | "DESTRUCTURING-BIND", [vars, expr, body @ ..])
                if !self.spliced(vars) && !self.spliced(expr) =>
            {
                self.form(expr, value, None, depth);
                nested.binds_values = true;
                self.sequence(body, TemplateBody::Declared, nested, depth);
            }
            ("BLOCK", [block_name, body @ ..]) if !self.spliced(block_name) => {
                if self.add_block(block_name, &mut nested).is_some() {
                    self.sequence(body, TemplateBody::Forms, nested, depth);
                }
            }
            ("CATCH", [tag, body @ ..]) if !self.spliced(tag) => {
                self.form(tag, value, None, depth);
                self.sequence(body, TemplateBody::Forms, nested, depth);
            }
            ("RETURN-FROM", [block_name, rest @ ..])
                if !self.spliced(block_name) && rest.len() <= 1 && fixed(self, rest) =>
            {
                if let Some(val) = rest.first() {
                    self.form(val, value, None, depth);
                }
            }
            ("THE", [type_spec, expr]) if !self.spliced(type_spec) && !self.spliced(expr) => {
                self.form(expr, value, None, depth);
            }
            ("EVAL-WHEN", [situations, body @ ..])
                if situations.as_node().is_some_and(|node| {
                    closed_list(node)
                        && elements(node).any(|situation| {
                            symbol(&situation).is_some_and(|name| {
                                name.prefix == Prefix::Keyword && name.name == "EXECUTE"
                            })
                        })
                }) =>
            {
                self.sequence(body, TemplateBody::Forms, context, depth);
            }
            ("HANDLER-CASE", [expr, clauses @ ..])
                if !self.spliced(expr) && fixed(self, clauses) =>
            {
                self.form(expr, nested, None, depth);
                for clause in clauses {
                    if let Some(cnode) = clause.as_node().filter(|node| closed_list(node)) {
                        let fields: Vec<_> = elements(cnode).collect();
                        if fields.len() >= 2
                            && !self.spliced(&fields[0])
                            && !self.spliced(&fields[1])
                        {
                            let clause = EvalContext {
                                binds_values: true,
                                ..nested
                            };
                            self.sequence(&fields[2..], TemplateBody::Declared, clause, depth);
                        }
                    }
                }
            }
            (
                "WITH-OPEN-FILE"
                | "WITH-OPEN-STREAM"
                | "WITH-INPUT-FROM-STRING"
                | "WITH-OUTPUT-TO-STRING"
                | "HANDLER-BIND",
                [spec, body @ ..],
            ) if !self.spliced(spec) => {
                if head.name != "HANDLER-BIND"
                    && let Some(node) = spec.as_node().filter(|node| closed_list(node))
                {
                    let fields: Vec<_> = elements(node).collect();
                    if let [bind, arguments @ ..] = fields.as_slice()
                        && !self.spliced(bind)
                    {
                        if head.name == "WITH-INPUT-FROM-STRING" {
                            if let [input, options @ ..] = arguments
                                && !self.spliced(input)
                            {
                                self.form(input, value, None, depth);
                                for [key, operand] in options.as_chunks::<2>().0 {
                                    let Some(key) =
                                        symbol(key).filter(|name| name.prefix == Prefix::Keyword)
                                    else {
                                        break;
                                    };
                                    if self.spliced(operand) {
                                        break;
                                    }
                                    let role = if key.name == "INDEX" {
                                        OperandRole::Place
                                    } else {
                                        OperandRole::Value
                                    };
                                    self.form(operand, value.nested(role), None, depth);
                                }
                            }
                        } else {
                            let arg_context = EvalContext {
                                open_guard: value.open_guard || head.name == "WITH-OPEN-STREAM",
                                ..value
                            };
                            for argument in arguments {
                                self.form(
                                    argument,
                                    arg_context,
                                    Some(TemplateBody::Arguments),
                                    depth,
                                );
                            }
                        }
                        if head.name == "WITH-OUTPUT-TO-STRING" {
                            if arguments.is_empty() {
                                nested.usage = OperandRole::Discarded;
                            } else if let Some(name) =
                                symbol(&arguments[0]).filter(|name| name.name == "NIL")
                            {
                                self.require(name, true);
                                nested.usage = OperandRole::Discarded;
                            }
                        }
                    }
                }
                nested.binds_values = true;
                self.sequence(
                    body,
                    if head.name == "HANDLER-BIND" {
                        TemplateBody::Forms
                    } else {
                        TemplateBody::Declared
                    },
                    nested,
                    depth,
                );
            }
            ("DOLIST" | "DOTIMES", [spec, body @ ..]) if !self.spliced(spec) => {
                if let Some(spec) = spec.as_node().filter(|node| closed_list(node)) {
                    let fields: Vec<_> = elements(spec).collect();
                    if let [_, bound, rest @ ..] = fields.as_slice()
                        && rest.len() <= 1
                        && fixed(self, &fields)
                    {
                        let bound_context = EvalContext {
                            nil_block: true,
                            ..value
                        };
                        self.form(bound, bound_context, None, depth);
                        if let Some(result) = rest.first() {
                            self.form(
                                result,
                                EvalContext {
                                    nil_block: true,
                                    binds_values: true,
                                    ..nested
                                },
                                None,
                                depth,
                            );
                        }
                    }
                }
                nested.in_loop = true;
                nested.nil_block = true;
                nested.binds_values = true;
                // A template label can be a GO target for the caller's code.
                // Only project a single splice when all other forms are statements.
                if body.iter().filter(|element| self.spliced(element)).count() <= 1
                    && body.iter().all(|element| {
                        self.spliced(element) || element.as_node().is_some_and(closed_list)
                    })
                {
                    self.sequence(
                        body,
                        TemplateBody::Tagbody,
                        nested.nested(OperandRole::Discarded),
                        depth,
                    );
                }
            }
            ("LAMBDA", [params, body @ ..]) if !self.spliced(params) => {
                let body_context = EvalContext {
                    binds_values: true,
                    deferred: true,
                    in_loop: false,
                    ..value
                };
                self.sequence(body, TemplateBody::Documented, body_context, depth);
            }
            ("DEFUN", [fn_name, params, body @ ..])
                if !self.spliced(fn_name) && !self.spliced(params) =>
            {
                let mut body_context = EvalContext {
                    binds_values: true,
                    deferred: true,
                    in_loop: false,
                    block: None,
                    nil_block: false,
                    ..value
                };
                if self.add_block(fn_name, &mut body_context).is_some() {
                    self.sequence(body, TemplateBody::Documented, body_context, depth);
                }
            }
            ("FUNCTION", [target])
                if list(target).is_some_and(|(head, _)| head.name == "LAMBDA") =>
            {
                self.form(target, value, None, depth);
            }
            ("TAGBODY", body) => {
                if body.iter().filter(|element| self.spliced(element)).count() <= 1
                    && body.iter().all(|element| {
                        self.spliced(element) || element.as_node().is_some_and(closed_list)
                    })
                {
                    self.sequence(
                        body,
                        TemplateBody::Tagbody,
                        nested.nested(OperandRole::Discarded),
                        depth,
                    );
                }
            }
            ("THROW", [tag, result]) if !self.spliced(tag) && !self.spliced(result) => {
                self.form(tag, value, None, depth);
                self.form(result, value, None, depth);
            }
            ("RETURN", rest) if rest.len() <= 1 && fixed(self, rest) => {
                if let Some(val) = rest.first() {
                    self.form(val, value, None, depth);
                }
            }
            ("PROG2", [first, second, rest @ ..])
                if !self.spliced(first) && !self.spliced(second) =>
            {
                self.form(first, nested.nested(OperandRole::Discarded), None, depth);
                self.form(second, nested, None, depth);
                self.sequence(
                    rest,
                    TemplateBody::Forms,
                    nested.nested(OperandRole::Discarded),
                    depth,
                );
            }
            ("MULTIPLE-VALUE-LIST", [expr]) if !self.spliced(expr) => {
                self.form(expr, value, None, depth);
            }
            ("MULTIPLE-VALUE-CALL", [target, producers @ ..])
                if !self.spliced(target) && fixed(self, producers) =>
            {
                self.form(target, value, None, depth);
                for producer in producers {
                    self.form(producer, value, None, depth);
                }
            }
            ("MULTIPLE-VALUE-SETQ", [vars, expr]) if !self.spliced(vars) && !self.spliced(expr) => {
                self.form(expr, value, None, depth);
            }
            ("NTH-VALUE", [index, expr]) if !self.spliced(index) && !self.spliced(expr) => {
                self.form(index, value, None, depth);
                self.form(expr, value, None, depth);
            }
            ("UNWIND-PROTECT" | "PROG1" | "MULTIPLE-VALUE-PROG1", [first, cleanup @ ..])
                if !self.spliced(first) =>
            {
                let protected = EvalContext {
                    open_guard: nested.open_guard || head.name == "UNWIND-PROTECT",
                    ..nested
                };
                self.form(first, protected, None, depth);
                self.sequence(
                    cleanup,
                    TemplateBody::Forms,
                    nested.nested(OperandRole::Discarded),
                    depth,
                );
            }
            ("ASSERT", [condition]) if !self.spliced(condition) => {
                self.form(condition, test, None, depth)
            }
            ("CHECK-TYPE", [place, type_spec, ..])
                if !self.spliced(place) && !self.spliced(type_spec) =>
            {
                self.form(place, nested.nested(OperandRole::Place), None, depth);
            }
            ("SETF" | "SETQ", args) if fixed(self, args) && args.len().is_multiple_of(2) => {
                for [place, expression] in args.as_chunks::<2>().0 {
                    if head.name == "SETF" {
                        self.form(place, nested.nested(OperandRole::Place), None, depth);
                    }
                    self.form(expression, value, None, depth);
                }
            }
            ("INCF" | "DECF", [place, rest @ ..])
                if !self.spliced(place) && rest.len() <= 1 && fixed(self, rest) =>
            {
                self.form(place, nested.nested(OperandRole::Place), None, depth);
                if let Some(delta) = rest.first() {
                    self.form(delta, value, None, depth);
                }
            }
            ("PUSH" | "PUSHNEW", [item, place]) if !self.spliced(item) && !self.spliced(place) => {
                self.form(item, value, None, depth);
                self.form(place, nested.nested(OperandRole::Place), None, depth);
            }
            ("POP", [place]) if !self.spliced(place) => {
                self.form(place, nested.nested(OperandRole::Place), None, depth);
            }
            ("TIME" | "IGNORE-ERRORS", [operand]) if !self.spliced(operand) => {
                self.form(operand, nested, None, depth);
            }
            (name, args) if builtin_kind(name) == Some(BuiltinKind::Function) => {
                for operand in args {
                    self.form(operand, value, Some(TemplateBody::Arguments), depth);
                }
            }
            _ => return,
        }
        self.require(head, true);
    }

    fn sequence(
        &mut self,
        forms: &[Element],
        kind: TemplateBody,
        context: EvalContext,
        depth: usize,
    ) {
        let last_fixed = forms.iter().rposition(|form| !self.spliced(form));
        let mut prefix = true;
        for (i, form) in forms.iter().enumerate() {
            if prefix
                && matches!(
                    kind,
                    TemplateBody::Declared | TemplateBody::Documented | TemplateBody::Tagbody
                )
                && let Some((head, _)) = list(form)
                && head.name == "DECLARE"
            {
                self.require(head, true);
                continue;
            }
            let mut context = context;
            if last_fixed.is_some_and(|last| i < last) {
                context.usage = OperandRole::Discarded;
            } else if i + 1 < forms.len() {
                // A trailing splice might be empty. The current form may
                // supply the result even though it isn't last in the template.
                context.usage = OperandRole::Value;
            }
            let body = if prefix || kind == TemplateBody::Tagbody {
                kind
            } else {
                TemplateBody::Forms
            };
            self.form(form, context, Some(body), depth);
            prefix = false;
        }
    }
}

pub(super) fn list(element: &Element) -> Option<(SymbolName, Vec<Element>)> {
    let node = element.as_node().filter(|node| closed_list(node))?;
    let mut fields = elements(node);
    let head = symbol(&fields.next()?)?;
    Some((head, fields.collect()))
}

pub(super) fn let_binding(binding: &Element) -> Option<(Element, Option<Element>)> {
    if let Some(node) = binding.as_node().filter(|node| closed_list(node)) {
        let mut fields = elements(node);
        let name = fields.next()?;
        let init = fields.next();
        fields.next().is_none().then_some((name, init))
    } else {
        Some((binding.clone(), None))
    }
}

pub(super) fn reader_operand(element: &Element, prefix: SyntaxKind) -> Option<Element> {
    let node = element
        .as_node()
        .filter(|node| node.kind() == SyntaxKind::ReaderForm)?;
    let mut fields = elements(node);
    (fields.next()?.kind() == prefix).then_some(())?;
    let operand = fields.next()?;
    fields.next().is_none().then_some(operand)
}
