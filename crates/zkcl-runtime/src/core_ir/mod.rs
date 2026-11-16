//! Core IR for resolved Lisp forms and the passes that prepare them for bytecode emission.

mod analysis;
mod image;
mod inlining;
mod integer_range;
pub(crate) mod nesting;
mod simplify;
mod type_predicates;
mod types;

use std::ops::ControlFlow;
use std::rc::Rc;

use num_bigint::BigInt;
use rustc_hash::FxHashMap;

use crate::heap::RootSlot;
use crate::read::Form;
use crate::symbol::{Name, NameKey};
use crate::vm::CaptureKind;

use analysis::{FunctionAnalysisContext, analyze_fn};
pub use inlining::can_store_inline_candidate;
use inlining::inline_pass;
pub(crate) use integer_range::IntegerRange;
use nesting::hoist_closed_local_functions;
use simplify::fold_and_simplify;
pub(crate) use simplify::is_single_value_expr;
pub(crate) use types::ftype_signature;
pub use types::{CoreType, CoreValuesSpec, ParsedTheType, parse_the_type, parse_type_spec};

const INLINE_SIZE_LIMIT: usize = 24;
const MAX_OPT_ROUNDS: usize = 6;

fn restore_map<K: Eq + std::hash::Hash, V>(map: &mut FxHashMap<K, V>, key: K, previous: Option<V>) {
    if let Some(value) = previous {
        map.insert(key, value);
    } else {
        map.remove(&key);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineDeclaration {
    Inline,
    NotInline,
}

#[derive(Clone, Copy)]
pub(crate) enum OptimizationQuality {
    Speed,
    Safety,
    Debug,
    Space,
    CompilationSpeed,
}

/// Lexical and compilation-unit `OPTIMIZE` qualities (`0..=3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptimizationQualities {
    pub speed: u8,
    pub safety: u8,
    pub debug: u8,
    pub space: u8,
    pub compilation_speed: u8,
}

impl Default for OptimizationQualities {
    fn default() -> Self {
        Self {
            speed: 1,
            safety: 1,
            debug: 1,
            space: 1,
            compilation_speed: 1,
        }
    }
}

impl OptimizationQualities {
    pub(crate) fn set(&mut self, quality: OptimizationQuality, value: u8) {
        match quality {
            OptimizationQuality::Speed => self.speed = value,
            OptimizationQuality::Safety => self.safety = value,
            OptimizationQuality::Debug => self.debug = value,
            OptimizationQuality::Space => self.space = value,
            OptimizationQuality::CompilationSpeed => self.compilation_speed = value,
        }
    }

    pub(crate) fn favors_speed(self) -> bool {
        self.speed > self.space.max(self.debug).max(self.compilation_speed)
    }

    fn allows_automatic_inlining(self) -> bool {
        // Keeping calls preserves debugger frames and avoids the compile-time
        // work of inspecting, cloning, and reanalyzing inline expansions.
        self.debug.max(self.compilation_speed) <= self.speed.max(self.space)
    }

    fn local_inline_size_limit(self) -> usize {
        if self.favors_speed() {
            40
        } else if self.space > self.speed.max(self.debug).max(self.compilation_speed) {
            if self.space == 3 && self.speed == 0 {
                0
            } else {
                8
            }
        } else {
            INLINE_SIZE_LIMIT
        }
    }
}

macro_rules! define_id {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u32);
        impl $name {
            #[inline]
            pub const fn index(self) -> usize { self.0 as usize }
        }
    )*};
}

define_id!(VarId, LocalFunctionId, BlockId, TagbodyId);

/// A variable binding's declared type and the uses found during analysis.
#[derive(Clone)]
pub struct VariableBinding {
    pub name: Name,
    pub special: bool,
    pub assigned: bool,
    pub captured: bool,
    pub read_count: u32,
    /// Type promised by the binding's declaration under safety zero.
    pub trusted_type: CoreType,
    /// Whether `(declare (dynamic-extent ...))` applies to this variable.
    pub dynamic_extent: bool,
}

impl VariableBinding {
    pub(crate) fn fresh_binding(&self) -> Self {
        Self {
            name: self.name.clone(),
            special: self.special,
            trusted_type: self.trusted_type,
            dynamic_extent: self.dynamic_extent,
            assigned: false,
            captured: false,
            read_count: 0,
        }
    }

    pub fn capture_kind(&self) -> CaptureKind {
        if self.assigned {
            CaptureKind::MutableCell
        } else {
            CaptureKind::Value
        }
    }
}

/// The lexical target of `RETURN-FROM` and its control-transfer requirements.
#[derive(Clone)]
pub struct BlockExitTarget {
    pub name: Name,
    pub used: bool,
    pub escapes: bool,
    pub crosses_dynamic_scope: bool,
}

impl BlockExitTarget {
    pub(crate) fn fresh_binding(&self) -> Self {
        Self {
            name: self.name.clone(),
            used: false,
            escapes: false,
            crosses_dynamic_scope: false,
        }
    }

    pub const fn is_local_jump(&self) -> bool {
        self.used && !self.escapes && !self.crosses_dynamic_scope
    }
}

/// Tag key inside a `TAGBODY` form.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Tag {
    Symbol(NameKey),
    Integer(isize),
    Bignum(BigInt),
}

/// Shared identity and source-order label numbers of one lexical TAGBODY.
pub(super) type TagLabels = Rc<FxHashMap<Tag, usize>>;

/// The lexical labels established by a `TAGBODY` and their control-transfer requirements.
#[derive(Clone, Debug)]
pub struct TagbodyTargets {
    pub tags: TagLabels,
    pub used: bool,
    pub escapes: bool,
    pub crosses_dynamic_scope: bool,
}

impl TagbodyTargets {
    pub(crate) fn fresh_binding(&self) -> Self {
        Self {
            tags: self.tags.clone(),
            used: false,
            escapes: false,
            crosses_dynamic_scope: false,
        }
    }

    pub const fn is_local_jump(&self) -> bool {
        !self.escapes && !self.crosses_dynamic_scope
    }
}

/// The name and use of a local function binding.
#[derive(Clone)]
pub struct LocalFunctionBinding {
    pub name: Name,
    pub setf: bool,
    pub call_count: u32,
    pub escapes: bool,
    pub recursive: bool,
    pub has_notinline_call: bool,
    /// Whether `(declare (dynamic-extent #'...))` applies to this local function.
    pub dynamic_extent: bool,
}

/// Defaulted optional or keyword parameter descriptor in [`CoreParameterBindings`].
#[derive(Clone)]
pub struct CoreDefaultedParameter {
    pub var: VarId,
    pub init: Option<CoreExpr>,
    pub supplied: Option<VarId>,
}

/// Keyword parameter descriptor in [`CoreKeywordParameters`].
#[derive(Clone)]
pub struct CoreKeywordParameter {
    pub key: Name,
    pub param: CoreDefaultedParameter,
}

/// Keyword section descriptor in [`CoreParameterBindings`].
#[derive(Clone)]
pub struct CoreKeywordParameters {
    pub params: Vec<CoreKeywordParameter>,
    pub allow_other_keys: bool,
}

/// Parameter binding descriptor for a [`CoreFn`] or [`LocalFunction`].
#[derive(Clone)]
pub struct CoreParameterBindings {
    required_lexicals_only: bool,
    pub all_params: Vec<VarId>,
    pub required: Vec<VarId>,
    pub optional: Vec<CoreDefaultedParameter>,
    pub rest: Option<VarId>,
    pub keywords: Option<CoreKeywordParameters>,
    pub aux: Vec<(VarId, Option<CoreExpr>)>,
}

impl CoreParameterBindings {
    pub fn new(
        all_params: Vec<VarId>,
        has_special_params: bool,
        required: Vec<VarId>,
        optional: Vec<CoreDefaultedParameter>,
        rest: Option<VarId>,
        keywords: Option<CoreKeywordParameters>,
        aux: Vec<(VarId, Option<CoreExpr>)>,
    ) -> Self {
        Self {
            required_lexicals_only: !has_special_params
                && optional.is_empty()
                && rest.is_none()
                && keywords.is_none()
                && aux.is_empty(),
            all_params,
            required,
            optional,
            rest,
            keywords,
            aux,
        }
    }

    pub fn empty() -> Self {
        // Captured functions have no definition here. Their placeholder must
        // not be eligible for the zero-argument inlining path.
        Self {
            required_lexicals_only: false,
            all_params: Vec::new(),
            required: Vec::new(),
            optional: Vec::new(),
            rest: None,
            keywords: None,
            aux: Vec::new(),
        }
    }

    pub fn inlineable_fixed_params(&self) -> Option<&[VarId]> {
        self.required_lexicals_only
            .then_some(self.required.as_slice())
    }

    pub fn initializers_and_body<'a>(
        &'a self,
        body: &'a CoreExpr,
    ) -> impl Iterator<Item = &'a CoreExpr> {
        let optional = self.optional.iter().filter_map(|param| param.init.as_ref());
        let keywords = self
            .keywords
            .iter()
            .flat_map(|keys| &keys.params)
            .filter_map(|key| key.param.init.as_ref());
        let aux = self.aux.iter().filter_map(|(_, init)| init.as_ref());
        optional
            .chain(keywords)
            .chain(aux)
            .chain(std::iter::once(body))
    }

    /// Visit parameter bindings in lambda-list order. Each initform runs
    /// before its variable is bound, and before its supplied-p variable.
    fn for_each_binding_mut(&mut self, mut bind: impl FnMut(&mut VarId, Option<&mut CoreExpr>)) {
        fn defaulted(
            param: &mut CoreDefaultedParameter,
            bind: &mut impl FnMut(&mut VarId, Option<&mut CoreExpr>),
        ) {
            bind(&mut param.var, param.init.as_mut());
            if let Some(supplied) = &mut param.supplied {
                bind(supplied, None);
            }
        }

        for var in &mut self.required {
            bind(var, None);
        }
        for param in &mut self.optional {
            defaulted(param, &mut bind);
        }
        if let Some(rest) = &mut self.rest {
            bind(rest, None);
        }
        if let Some(keywords) = &mut self.keywords {
            for key in &mut keywords.params {
                defaulted(&mut key.param, &mut bind);
            }
        }
        for (var, init) in &mut self.aux {
            bind(var, init.as_mut());
        }
    }

    pub fn for_each_init_mut(&mut self, f: &mut impl FnMut(&mut CoreExpr)) {
        for opt in &mut self.optional {
            if let Some(init) = &mut opt.init {
                f(init);
            }
        }
        if let Some(kws) = &mut self.keywords {
            for kw in &mut kws.params {
                if let Some(init) = &mut kw.param.init {
                    f(init);
                }
            }
        }
        for (_, init) in &mut self.aux {
            if let Some(init) = init {
                f(init);
            }
        }
    }
}

/// A local function in a [`CoreFn`], defined here or captured from an enclosing
/// scope. Captured functions share the table and leave the body empty.
#[derive(Clone)]
pub struct LocalFunction {
    pub binding: LocalFunctionBinding,
    pub params: CoreParameterBindings,
    pub(crate) source_lambda_list: Option<Rc<Form>>,
    pub(crate) source_expression: Option<Rc<Form>>,
    pub body: Box<CoreExpr>,
    pub at: usize,
}

impl LocalFunction {
    fn take_definition(&mut self) -> (CoreParameterBindings, Box<CoreExpr>) {
        (
            std::mem::replace(&mut self.params, CoreParameterBindings::empty()),
            std::mem::replace(&mut self.body, Box::new(CoreExpr::nil(self.at))),
        )
    }
}

/// A constant literal in Core IR.
#[derive(Clone)]
pub enum CoreConst {
    Nil,
    True,
    Fixnum(i64),
    Char(char),
    String(String),
    Symbol(Name),
    LiteralForm(Form),
}

/// Binding target in [`CoreKind::Let`].
#[derive(Clone)]
pub enum CoreBindingTarget {
    Lexical(VarId),
    Special(Name),
}

/// Call target in [`CoreKind::Call`].
#[derive(Clone)]
pub enum CoreCallee {
    Named(Name),
    Local(LocalFunctionId),
    Expr(Box<CoreExpr>),
}

/// Core IR expression node with source offset.
#[derive(Clone)]
pub struct CoreExpr {
    pub kind: CoreKind,
    pub at: usize,
}

impl CoreExpr {
    pub fn new(at: usize, kind: CoreKind) -> Self {
        Self { kind, at }
    }
    pub fn nil(at: usize) -> Self {
        Self::new(at, CoreKind::Const(CoreConst::Nil))
    }

    /// Visit immediate child expressions. Lambda bodies and out-of-line
    /// local function definitions are handled by the pass that owns them.
    fn for_each_child_mut(&mut self, mut visit: impl FnMut(&mut CoreExpr)) {
        match &mut self.kind {
            CoreKind::WriteVar { value, .. }
            | CoreKind::WriteGlobal { value, .. }
            | CoreKind::ReturnFrom { value, .. }
            | CoreKind::The { expr: value, .. }
            | CoreKind::Flet { body: value, .. }
            | CoreKind::Block { body: value, .. } => visit(value),
            CoreKind::Progn(forms) => forms.iter_mut().for_each(visit),
            CoreKind::If { test, then_, else_ } => {
                visit(test);
                visit(then_);
                visit(else_);
            }
            CoreKind::Let { bindings, body, .. } => {
                for (_, init) in bindings {
                    visit(init);
                }
                visit(body);
            }
            CoreKind::Tagbody { segments, .. } => {
                for (_, forms) in segments {
                    forms.iter_mut().for_each(&mut visit);
                }
            }
            CoreKind::Call { callee, args, .. } => {
                if let CoreCallee::Expr(function) = callee {
                    visit(function);
                }
                args.iter_mut().for_each(visit);
            }
            CoreKind::CallValues { callee, forms } => {
                visit(callee);
                forms.iter_mut().for_each(visit);
            }
            CoreKind::Const(_)
            | CoreKind::ReadVar(_)
            | CoreKind::ReadGlobal(_)
            | CoreKind::ReadGlobalFn { .. }
            | CoreKind::ReadLocalFn(_)
            | CoreKind::Go { .. }
            | CoreKind::Lambda { .. }
            | CoreKind::LoadTimeValue { .. }
            | CoreKind::RawSpecial(..) => {}
        }
    }

    /// Walk expressions whose binding IDs belong to the same CoreFn. A lambda is
    /// visited as a leaf because its captures map to a new set of IDs. Passing
    /// the local-function table includes FLET/LABELS initforms and bodies.
    pub(crate) fn walk_in_function(
        &self,
        local_functions: Option<&[LocalFunction]>,
        visit: &mut impl FnMut(&CoreExpr) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        visit(self)?;
        let mut walk = |child: &CoreExpr| child.walk_in_function(local_functions, visit);
        match &self.kind {
            CoreKind::WriteVar { value, .. }
            | CoreKind::WriteGlobal { value, .. }
            | CoreKind::ReturnFrom { value, .. }
            | CoreKind::Block { body: value, .. }
            | CoreKind::The { expr: value, .. } => walk(value)?,
            CoreKind::Progn(exprs) => exprs.iter().try_for_each(walk)?,
            CoreKind::If { test, then_, else_ } => {
                walk(test)?;
                walk(then_)?;
                walk(else_)?;
            }
            CoreKind::Let { bindings, body, .. } => {
                for (_, init) in bindings {
                    walk(init)?;
                }
                walk(body)?;
            }
            CoreKind::Flet { fns, body, .. } => {
                if let Some(table) = local_functions {
                    for id in fns {
                        let local = &table[id.index()];
                        local
                            .params
                            .initializers_and_body(&local.body)
                            .try_for_each(&mut walk)?;
                    }
                }
                walk(body)?;
            }
            CoreKind::Tagbody { segments, .. } => {
                for (_, exprs) in segments {
                    exprs.iter().try_for_each(&mut walk)?;
                }
            }
            CoreKind::Call { callee, args, .. } => {
                if let CoreCallee::Expr(function) = callee {
                    walk(function)?;
                }
                args.iter().try_for_each(walk)?;
            }
            CoreKind::CallValues { callee, forms } => {
                walk(callee)?;
                forms.iter().try_for_each(walk)?;
            }
            CoreKind::Const(_)
            | CoreKind::ReadVar(_)
            | CoreKind::ReadGlobal(_)
            | CoreKind::ReadGlobalFn { .. }
            | CoreKind::ReadLocalFn(_)
            | CoreKind::Go { .. }
            | CoreKind::Lambda { .. }
            | CoreKind::LoadTimeValue { .. }
            | CoreKind::RawSpecial(..) => {}
        }
        ControlFlow::Continue(())
    }
}

/// Core IR expression variants.
#[derive(Clone)]
pub enum CoreKind {
    Const(CoreConst),
    ReadVar(VarId),
    ReadGlobal(Name),
    ReadGlobalFn {
        name: Name,
        setf: bool,
    },
    ReadLocalFn(LocalFunctionId),
    WriteVar {
        var: VarId,
        value: Box<CoreExpr>,
    },
    WriteGlobal {
        name: Name,
        value: Box<CoreExpr>,
    },
    Progn(Vec<CoreExpr>),
    If {
        test: Box<CoreExpr>,
        then_: Box<CoreExpr>,
        else_: Box<CoreExpr>,
    },
    Let {
        sequential: bool,
        bindings: Vec<(CoreBindingTarget, CoreExpr)>,
        body: Box<CoreExpr>,
    },
    Flet {
        recursive: bool,
        fns: Vec<LocalFunctionId>,
        body: Box<CoreExpr>,
    },
    Block {
        id: BlockId,
        body: Box<CoreExpr>,
    },
    ReturnFrom {
        id: BlockId,
        value: Box<CoreExpr>,
    },
    Tagbody {
        id: TagbodyId,
        segments: Vec<(Option<usize>, Vec<CoreExpr>)>,
    },
    Go {
        id: TagbodyId,
        tag_index: usize,
    },
    Call {
        callee: CoreCallee,
        args: Vec<CoreExpr>,
        // The lexical OPTIMIZE qualities at this call, including after inline expansion.
        qualities: OptimizationQualities,
        inline_declaration: Option<InlineDeclaration>,
        ftype_ret: CoreType,
        fixnum_arithmetic: Option<zkcl_bytecode::FixnumArithmetic>,
    },
    CallValues {
        callee: Box<CoreExpr>,
        forms: Vec<CoreExpr>,
    },
    Lambda {
        func: Box<CoreFn>,
    },
    LoadTimeValue {
        cell: Rc<RootSlot>,
    },
    The {
        type_spec: Form,
        expr: Box<CoreExpr>,
        safety: u8,
    },
    RawSpecial(Form, Vec<RawLexicalBinding>),
}

#[derive(Clone)]
pub enum RawLexicalBinding {
    Declaration(Name, Rc<RootSlot>),
    Optimize(OptimizationQualities),
    VariableType {
        name: Name,
        spec: Form,
        is_free: bool,
    },
    FunctionType {
        name: Name,
        setf: bool,
        spec: Form,
    },
    Special(Name),
    Inline {
        name: Name,
        setf: bool,
        declaration: InlineDeclaration,
    },
    Macro {
        name: Name,
        expander: Rc<RootSlot>,
        symbol_macro: bool,
    },
}

/// Function-level Core IR unit with local tables and outer capture links `(child, parent)`.
#[derive(Clone)]
pub struct CoreFn {
    pub(crate) fixnums: IntegerRange,
    pub name: Option<Name>,
    pub params: CoreParameterBindings,
    pub(crate) source_lambda_list: Option<Rc<Form>>,
    pub(crate) source_expression: Option<Rc<Form>>,
    pub body: Box<CoreExpr>,
    pub vars: Vec<VariableBinding>,
    pub blocks: Vec<BlockExitTarget>,
    pub tagbodies: Vec<TagbodyTargets>,
    pub local_functions: Vec<LocalFunction>,
    pub parent_var_captures: Vec<(VarId, VarId)>,
    pub parent_block_captures: Vec<(BlockId, BlockId)>,
    pub parent_tagbody_captures: Vec<(TagbodyId, TagbodyId)>,
    pub parent_function_captures: Vec<(LocalFunctionId, LocalFunctionId)>,
    // Captures copy_source against the enclosing bytecode compiler's lexical scopes.
    pub external_var_captures: Vec<(VarId, Name)>,
    pub external_block_captures: Vec<(BlockId, Name)>,
    /// One captured activation per external TAGBODY, shared by all its label indices.
    pub external_tagbody_captures: Vec<TagbodyId>,
    pub external_function_captures: Vec<(LocalFunctionId, Name, bool)>,
    pub at: usize,
    pub(crate) debug: u8,
}

impl CoreFn {
    pub fn new(
        target: &zkcl_target::CompilationTarget,
        name: Option<Name>,
        params: CoreParameterBindings,
        body: Box<CoreExpr>,
        at: usize,
    ) -> Self {
        Self {
            name,
            params,
            source_lambda_list: None,
            source_expression: None,
            fixnums: IntegerRange::for_target(target),
            body,
            vars: Vec::new(),
            blocks: Vec::new(),
            tagbodies: Vec::new(),
            local_functions: Vec::new(),
            parent_var_captures: Vec::new(),
            parent_block_captures: Vec::new(),
            parent_tagbody_captures: Vec::new(),
            parent_function_captures: Vec::new(),
            external_var_captures: Vec::new(),
            external_block_captures: Vec::new(),
            external_tagbody_captures: Vec::new(),
            external_function_captures: Vec::new(),
            at,
            debug: OptimizationQualities::default().debug,
        }
    }

    pub fn alloc_var(&mut self, binding: VariableBinding) -> VarId {
        let id = VarId(self.vars.len() as u32);
        self.vars.push(binding);
        id
    }

    pub fn alloc_block(&mut self, target: BlockExitTarget) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(target);
        id
    }

    pub fn alloc_tagbody(&mut self, targets: TagbodyTargets) -> TagbodyId {
        let id = TagbodyId(self.tagbodies.len() as u32);
        self.tagbodies.push(targets);
        id
    }

    pub fn alloc_local_fn(&mut self, function: LocalFunction) -> LocalFunctionId {
        let id = LocalFunctionId(self.local_functions.len() as u32);
        self.local_functions.push(function);
        id
    }

    pub fn has_external_captures(&self) -> bool {
        !self.external_var_captures.is_empty()
            || !self.external_block_captures.is_empty()
            || !self.external_tagbody_captures.is_empty()
            || !self.external_function_captures.is_empty()
    }

    pub fn has_captures(&self) -> bool {
        !self.parent_var_captures.is_empty()
            || !self.parent_block_captures.is_empty()
            || !self.parent_tagbody_captures.is_empty()
            || !self.parent_function_captures.is_empty()
            || self.has_external_captures()
    }
}

/// Optimize `fn_ir` via iterative inlining, constant folding/DCE, and static analysis.
pub fn optimize_core_fn(fn_ir: &mut CoreFn, inline_lookup: &dyn Fn(&Name) -> Option<Rc<CoreFn>>) {
    optimize_core_fn_inner(fn_ir, inline_lookup);
}

fn optimize_core_fn_inner(
    fn_ir: &mut CoreFn,
    inline_lookup: &dyn Fn(&Name) -> Option<Rc<CoreFn>>,
) -> bool {
    let mut any_changed = false;
    let mut analysis = FunctionAnalysisContext::default();
    analyze_fn(fn_ir, &mut analysis);
    if fn_ir.debug == 3 {
        return false;
    }
    for _ in 0..MAX_OPT_ROUNDS {
        let inlined = inline_pass(fn_ir, inline_lookup);
        if inlined {
            analyze_fn(fn_ir, &mut analysis);
        }
        let hoisted = hoist_closed_local_functions(fn_ir);
        if hoisted {
            analyze_fn(fn_ir, &mut analysis);
        }
        let folded = fold_and_simplify(fn_ir);
        if folded {
            analyze_fn(fn_ir, &mut analysis);
        }
        if !inlined && !hoisted && !folded {
            break;
        }
        any_changed = true;
    }
    any_changed
}

fn any_expr_node(
    expr: &CoreExpr,
    local_functions: &[LocalFunction],
    pred: &impl Fn(&CoreExpr) -> bool,
) -> bool {
    expr.walk_in_function(Some(local_functions), &mut |node| {
        if pred(node) || matches!(&node.kind, CoreKind::Lambda { func } if any_fn_node(func, pred))
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_break()
}

fn any_fn_node(fn_ir: &CoreFn, pred: &impl Fn(&CoreExpr) -> bool) -> bool {
    fn_ir
        .params
        .initializers_and_body(&fn_ir.body)
        .any(|expr| any_expr_node(expr, &fn_ir.local_functions, pred))
}

/// Returns `true` if `expr` (or any nested `Lambda`) contains [`CoreKind::RawSpecial`].
fn has_raw_special(expr: &CoreExpr, local_functions: &[LocalFunction]) -> bool {
    any_expr_node(expr, local_functions, &|e| {
        matches!(e.kind, CoreKind::RawSpecial(..))
    })
}

fn fn_has_raw_special(fn_ir: &CoreFn) -> bool {
    any_fn_node(fn_ir, &|e| matches!(e.kind, CoreKind::RawSpecial(..)))
}
