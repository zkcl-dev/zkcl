use std::collections::{HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;

use super::{
    ExecState, Invocation, Primitive, Resume, Signal, StepControl, Values, callback, classes,
    instances, type_check,
};
use crate::condition::{self, Class, Fault};
use crate::heap::{RootFrame, RootSlot, RootSlots, Word};
use crate::macros::CallExpander;
use crate::symbol::Package;
use crate::{EvalError, EvalErrorKind, Runtime};

enum Constructor {
    Builtin(Class),
    Defined(Word),
}

impl Constructor {
    fn lookup(runtime: &Runtime, spec: Word, at: usize) -> Result<Option<(Word, Self)>, Fault> {
        let class = if runtime.heap.is_symbol(spec) {
            let Some(class) = classes::find_named_class(runtime, spec, at)? else {
                return Ok(None);
            };
            class
        } else {
            spec
        };
        let Some(info) = runtime.heap.class_info(class) else {
            return Ok(None);
        };
        if let Some(builtin) = classes::builtin_condition(runtime, info.origin_name) {
            return Ok(Some((class, Self::Builtin(builtin))));
        }
        Ok(classes::class_definition(runtime, class)
            .filter(|def| def.kind == classes::RuntimeClassKind::Condition)
            .map(|_| (class, Self::Defined(class))))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal VM condition instantiation forwards frame slice and continuation resume"
    )]
    fn instantiate<'rt>(
        self,
        runtime: &'rt Runtime,
        op: Primitive,
        args: &RootSlots,
        initargs: Range<usize>,
        values: &mut Values,
        resume: Resume,
        at: usize,
    ) -> Result<Invocation<'rt>, Fault> {
        let condition = match self {
            Self::Builtin(class) => {
                condition::make_builtin_condition(runtime, class, args, initargs, at)?
            }
            Self::Defined(class) => {
                let completion = match op {
                    Primitive::MakeCondition => instances::Completion::Value,
                    Primitive::Warn => instances::Completion::Warn,
                    _ => instances::Completion::Signal {
                        unhandled: matches!(op, Primitive::Error),
                    },
                };
                return Ok(Invocation::Initialization(instances::Initialization::new(
                    runtime, class, args, initargs, completion, resume, at,
                )?));
            }
        };
        match op {
            Primitive::MakeCondition => {
                values.one(condition);
                Ok(Invocation::Done(resume))
            }
            Primitive::Warn => super::warn_invocation(runtime, condition, resume, at, None),
            _ => Ok(Invocation::Signal(Signal::from_lisp(
                runtime,
                condition,
                matches!(op, Primitive::Error),
                at,
                resume,
            ))),
        }
    }
}

pub(super) fn construct<'rt>(
    runtime: &'rt Runtime,
    op: Primitive,
    source: &RootSlots,
    args: Range<usize>,
    values: &mut Values,
    resume: Resume,
    at: usize,
) -> Result<Invocation<'rt>, Fault> {
    let spec = source.get(args.start);
    if let Some((_, class)) = Constructor::lookup(runtime, spec, at)? {
        return class.instantiate(
            runtime,
            op,
            source,
            args.start + 1..args.end,
            values,
            resume,
            at,
        );
    }
    let spec = source.get(args.start);
    if runtime.heap.cons_fields(spec).is_none()
        && type_check::derived_expander(runtime, spec).is_none()
    {
        return Err(invalid(at));
    }
    let roots = runtime.root_frame(args.len());
    for (i, reg) in args.enumerate() {
        roots.set(i, source.get(reg));
    }
    Ok(Invocation::ConstructCondition(Box::new(Construction {
        args: roots,
        op,
        resume,
        at,
        enclosing: None,
    })))
}

fn invalid(at: usize) -> Fault {
    EvalError::new(at, "expected a constructible condition type").into()
}

pub(super) struct Construction {
    args: RootFrame,
    op: Primitive,
    resume: Resume,
    at: usize,
    pub(super) enclosing: Option<usize>,
}

impl Construction {
    pub(super) fn dispatch<'rt>(
        &self,
        runtime: &'rt Runtime,
        exec: &mut ExecState<'rt>,
        pc: Option<usize>,
    ) -> Result<StepControl<'rt>, EvalError> {
        let class = callback::run(runtime, exec, |call, _| {
            let mut leaves = Vec::new();
            let spec = ConditionType::parse(
                runtime,
                &runtime.root_slot(self.args.get(0)),
                call,
                &mut leaves,
                self.at,
                0,
            )?;
            spec.choose(runtime, &leaves, self.at)
        });
        let class = match class {
            callback::Outcome::Completed(Ok(class)) => class,
            callback::Outcome::Completed(Err(fault)) => {
                return super::dispatch_error_with_pc(runtime, exec, fault, pc);
            }
            callback::Outcome::Unwind(target) => return Ok(StepControl::Unwind(target)),
            callback::Outcome::Unhandled(error) => return Err(error),
        };
        let invocation = class.instantiate(
            runtime,
            self.op,
            &self.args,
            1..self.args.len(),
            &mut exec.values,
            self.resume,
            self.at,
        );
        match invocation {
            Ok(mut call) => {
                if let Some(group) = self.enclosing {
                    call.associate_restart(group);
                }
                super::accept_with_pc(runtime, exec, call, pc)
            }
            Err(fault) => super::dispatch_error_with_pc(runtime, exec, fault, pc),
        }
    }
}

/// Preserve the union/intersection structure. Choosing the first arm of an OR
/// before checking an enclosing AND can produce an instance outside the type.
enum ConditionType {
    Class(usize),
    Union(Vec<Self>),
    Intersection(Vec<Self>),
}

impl ConditionType {
    fn parse(
        runtime: &Runtime,
        spec: &RootSlot,
        call: Option<&CallExpander<'_>>,
        leaves: &mut Vec<RootSlot>,
        at: usize,
        depth: usize,
    ) -> Result<Self, Fault> {
        if depth >= type_check::TYPE_NESTING_LIMIT {
            return Err(Fault::plain(
                Class::ProgramError,
                EvalError::new(at, "condition type expansion exceeds the nesting limit"),
            ));
        }
        if let Some((class, _)) = Constructor::lookup(runtime, spec.get(), at)? {
            leaves.push(runtime.root_slot(class));
            return Ok(Self::Class(leaves.len() - 1));
        }
        if let Some((expander, arguments)) = type_check::derived_expander(runtime, spec.get()) {
            let expanded = type_check::expand_with(runtime, expander, arguments, at, call)?;
            let expanded = runtime.root_slot(expanded);
            return Self::parse(runtime, &expanded, call, leaves, at, depth + 1);
        }
        let (head, tail) = runtime
            .heap
            .cons_fields(spec.get())
            .ok_or_else(|| invalid(at))?;
        let union = match condition::named_word(runtime, head, Package::COMMON_LISP).as_deref() {
            Some("OR") => true,
            Some("AND") => false,
            _ => return Err(invalid(at)),
        };
        let operands: Vec<_> = super::proper_list(runtime, tail, at)?
            .into_iter()
            .map(|word| runtime.root_slot(word))
            .collect();
        if operands.is_empty() {
            return Err(invalid(at));
        }
        let members = operands
            .iter()
            .map(|operand| Self::parse(runtime, operand, call, leaves, at, depth + 1))
            .collect::<Result<_, _>>()?;
        Ok(if union {
            Self::Union(members)
        } else {
            Self::Intersection(members)
        })
    }

    fn accepts(&self, runtime: &Runtime, candidate: &RootSlot, leaves: &[RootSlot]) -> bool {
        match self {
            Self::Class(i) => {
                classes::class_subtypep(runtime, candidate.get(), leaves[*i].get()) == Some(true)
            }
            Self::Union(members) => members
                .iter()
                .any(|m| m.accepts(runtime, candidate, leaves)),
            Self::Intersection(members) => members
                .iter()
                .all(|m| m.accepts(runtime, candidate, leaves)),
        }
    }

    fn choose(
        &self,
        runtime: &Runtime,
        leaves: &[RootSlot],
        at: usize,
    ) -> Result<Constructor, Fault> {
        for leaf in leaves {
            if self.accepts(runtime, leaf, leaves) {
                return Ok(Constructor::lookup(runtime, leaf.get(), at)?
                    .ok_or_else(|| invalid(at))?
                    .1);
            }
        }
        // Every inhabitant is a descendant of at least one of these roots.
        // Explore the existing subclass index breadth-first, so an existing
        // common ancestor wins over its more specialized descendants.
        let mut queue = VecDeque::new();
        self.search_roots(leaves, &mut queue);
        let mut seen = HashSet::new();
        while let Some(class) = queue.pop_front() {
            let serial = runtime
                .heap
                .class_info(class.get())
                .expect("condition class")
                .serial;
            if !seen.insert(serial) {
                continue;
            }
            if self.accepts(runtime, &class, leaves) {
                return Ok(Constructor::lookup(runtime, class.get(), at)?
                    .ok_or_else(|| invalid(at))?
                    .1);
            }
            queue.extend(crate::classes::class_direct_subclasses(
                runtime,
                class.get(),
                at,
            )?);
        }
        // No registered class inhabits the requested intersection. Reuse the
        // class graph for its layout and subsequent constructions.
        let mut choices = Vec::new();
        loop {
            let mut parents = Vec::new();
            let mut next = 0;
            self.collect_parents(leaves, &mut parents, &mut choices, &mut next);
            let direct = parents
                .iter()
                .filter(|parent| {
                    !parents.iter().any(|other| {
                        other.get() != parent.get()
                            && classes::class_subtypep(runtime, other.get(), parent.get())
                                == Some(true)
                    })
                })
                .map(|p| Rc::new(runtime.root_slot(p.get())))
                .collect();
            match super::class_definition::make_condition_intersection(runtime, direct, at) {
                Ok(class) => return Ok(Constructor::Defined(class)),
                Err(fault) => {
                    // Two individually valid parents can require opposite precedence
                    // orders. A different OR arm can still satisfy this intersection.
                    if fault.host.kind() != EvalErrorKind::LispCondition(Class::ProgramError)
                        || !next_union_arm(&mut choices)
                    {
                        return Err(fault);
                    }
                    runtime.check_execution_safepoint(at)?;
                }
            }
        }
    }

    fn search_roots(&self, leaves: &[RootSlot], queue: &mut VecDeque<Rc<RootSlot>>) {
        match self {
            Self::Class(i) => queue.push_back(Rc::new(leaves[*i].clone())),
            Self::Union(members) => {
                for member in members {
                    member.search_roots(leaves, queue);
                }
            }
            Self::Intersection(members) => members[0].search_roots(leaves, queue),
        }
    }

    fn collect_parents<'a>(
        &self,
        leaves: &'a [RootSlot],
        parents: &mut Vec<&'a RootSlot>,
        choices: &mut Vec<(usize, usize)>,
        next: &mut usize,
    ) {
        match self {
            Self::Class(i) if !parents.iter().any(|p| p.get() == leaves[*i].get()) => {
                parents.push(&leaves[*i]);
            }
            Self::Class(_) => {}
            Self::Union(members) => {
                if *next == choices.len() {
                    choices.push((0, members.len()));
                }
                let (arm, _) = choices[*next];
                *next += 1;
                members[arm].collect_parents(leaves, parents, choices, next);
            }
            Self::Intersection(members) => {
                for member in members {
                    member.collect_parents(leaves, parents, choices, next);
                }
            }
        }
    }
}

// Enumerate only the OR arms actually visited in this branch. Advancing an
// outer arm discards the choices belonging to its previous subtree.
fn next_union_arm(choices: &mut Vec<(usize, usize)>) -> bool {
    while let Some((arm, count)) = choices.last_mut() {
        *arm += 1;
        if *arm < *count {
            return true;
        }
        choices.pop();
    }
    false
}
