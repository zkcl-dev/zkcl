//! Orthogonal effect algebra shared across Core IR, bytecode lifting, SSA, and backends.

use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};

macro_rules! impl_bitflag_ops {
    ($ty:ident, $inner:ty, $mask:expr) => {
        impl $ty {
            /// Empty bitset.
            pub const EMPTY: Self = Self(0);

            /// Construct from raw bits, masking out undefined bits.
            #[inline]
            pub const fn from_bits_truncate(bits: $inner) -> Self {
                Self(bits & $mask)
            }

            /// Underlying raw bits.
            #[inline]
            pub const fn bits(self) -> $inner {
                self.0
            }

            /// Returns `true` when no flags are set.
            #[inline]
            pub const fn is_empty(self) -> bool {
                self.0 == 0
            }

            /// Returns `true` if every bit in `other` is set in `self`.
            #[inline]
            pub const fn contains(self, other: Self) -> bool {
                (self.0 & other.0) == other.0
            }

            /// Returns `true` if `self` and `other` share at least one bit.
            #[inline]
            pub const fn intersects(self, other: Self) -> bool {
                (self.0 & other.0) != 0
            }

            /// Returns `true` if `self` and `other` share no bits.
            #[inline]
            pub const fn disjoint(self, other: Self) -> bool {
                (self.0 & other.0) == 0
            }

            /// Set union (`self | other`).
            #[inline]
            pub const fn union(self, other: Self) -> Self {
                Self(self.0 | other.0)
            }

            /// Set intersection (`self & other`).
            #[inline]
            pub const fn intersection(self, other: Self) -> Self {
                Self(self.0 & other.0)
            }
        }

        impl BitOr for $ty {
            type Output = Self;

            #[inline]
            fn bitor(self, rhs: Self) -> Self {
                Self(self.0 | rhs.0)
            }
        }

        impl BitOrAssign for $ty {
            #[inline]
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl BitAnd for $ty {
            type Output = Self;

            #[inline]
            fn bitand(self, rhs: Self) -> Self {
                Self(self.0 & rhs.0)
            }
        }

        impl BitAndAssign for $ty {
            #[inline]
            fn bitand_assign(&mut self, rhs: Self) {
                self.0 &= rhs.0;
            }
        }
    };
}

/// Disjoint abstract memory and runtime-state partitions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct AliasDomain(u16);

impl AliasDomain {
    /// Lexical capture mutable box cells (`BoxLocal`, `ReadBox`, `WriteBox`).
    pub const BOX_CELL: Self = Self(1 << 0);
    /// Symbol dynamic/special/global value cells (`ReadCell(var)`, `WriteCell`).
    pub const SYMBOL_VALUE: Self = Self(1 << 1);
    /// Symbol function definitions and inline proclamations (`ReadCell(fn)`, guards, `DefineFunction`).
    pub const SYMBOL_FN: Self = Self(1 << 2);
    /// Cons cell `CAR` and `CDR` fields.
    pub const CONS_FIELD: Self = Self(1 << 3);
    /// Vector and array elements (`ReadFloatVector`, `WriteFloatVector`, array ops).
    pub const ARRAY_ELEM: Self = Self(1 << 4);
    /// CLOS / `defstruct` instance slots.
    pub const INSTANCE_SLOT: Self = Self(1 << 5);
    /// Multiple-value buffer and argument cursors.
    pub const VALUES_BUF: Self = Self(1 << 6);
    /// Dynamic binding, handler, restart, protect, and unwind stacks.
    pub const DYNAMIC_STACK: Self = Self(1 << 7);
    /// External host state, FFI memory, and I/O streams.
    pub const HOST_EXTERNAL: Self = Self(1 << 8);
    /// Union of all disjoint abstract memory domains.
    pub const ALL_MEM: Self = Self(0x01ff);
}

impl_bitflag_ops!(AliasDomain, u16, 0x01ff);

/// Atomic control-flow and progress effects (without hidden memory/GC implications).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct ControlEffect(u8);

impl ControlEffect {
    /// May transfer non-locally along the dynamic scope tree (`RETURN-FROM`, `GO`, `THROW`, unwind).
    pub const MAY_UNWIND: Self = Self(1 << 0);
    /// Speculative guard failure may deoptimize to bytecode `FrameState` without CL unwinding.
    pub const MAY_DEOPT: Self = Self(1 << 1);
    /// May cooperatively suspend the active task (`ZK-ASYNC`, JSPI, WasmFX).
    pub const MAY_SUSPEND: Self = Self(1 << 2);
    /// May fail to terminate (unbounded loops, general recursion, cyclic structure traversal).
    pub const MAY_DIVERGE: Self = Self(1 << 3);
    /// Union of all control-flow effects.
    pub const ALL: Self = Self(0x0f);
}

impl_bitflag_ops!(ControlEffect, u8, 0x0f);

/// Identity, GC safepoint, and speculative-assumption barrier contracts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct BarrierEffect(u8);

impl BarrierEffect {
    /// Allocates a fresh heap object with observable `EQ` identity (blocks duplication/CSE, not DCE).
    pub const ALLOC_FRESH: Self = Self(1 << 0);
    /// Safepoint that may trigger Immix garbage collection.
    pub const MAY_GC: Self = Self(1 << 1);
    /// Must not be eliminated by DCE even when its SSA output is unused or absent (e.g. guards).
    pub const REQUIRED_WHEN_UNUSED: Self = Self(1 << 2);
    /// Depends on an earlier dominating guard; must not be hoisted above `REQUIRED_WHEN_UNUSED`/`MAY_DEOPT`.
    pub const DEPENDS_ON_CHECKS: Self = Self(1 << 3);
    /// Union of all barrier flags.
    pub const ALL: Self = Self(0x0f);
}

impl_bitflag_ops!(BarrierEffect, u8, 0x0f);

/// Factored, orthogonal effect descriptor for an expression or IR instruction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct OpEffects {
    /// Abstract memory domains that may be read.
    pub reads: AliasDomain,
    /// Abstract memory domains that may be written.
    pub writes: AliasDomain,
    /// Non-local control-flow, deopt, suspension, and divergence effects.
    pub control: ControlEffect,
    /// Object-identity, GC safepoint, and speculation barrier obligations.
    pub barrier: BarrierEffect,
}

impl OpEffects {
    /// Pure total operation: no memory access, no control effects, no barriers.
    pub const PURE: Self = Self {
        reads: AliasDomain::EMPTY,
        writes: AliasDomain::EMPTY,
        control: ControlEffect::EMPTY,
        barrier: BarrierEffect::EMPTY,
    };

    /// Full Common Lisp `SIGNAL` profile: invokes arbitrary handlers without unwinding first,
    /// so it may read/write any domain, allocate, trigger GC, unwind, suspend, or diverge.
    pub const CL_SIGNAL: Self = Self {
        reads: AliasDomain::ALL_MEM,
        writes: AliasDomain::ALL_MEM,
        control: ControlEffect(
            ControlEffect::MAY_UNWIND.0
                | ControlEffect::MAY_SUSPEND.0
                | ControlEffect::MAY_DIVERGE.0,
        ),
        barrier: BarrierEffect(
            BarrierEffect::ALLOC_FRESH.0
                | BarrierEffect::MAY_GC.0
                | BarrierEffect::REQUIRED_WHEN_UNUSED.0,
        ),
    };

    /// Conservative descriptor for an unknown Lisp/host call.
    pub const UNKNOWN_CALL: Self = Self::CL_SIGNAL;

    /// Bounded execution checkpoint before control transfers or backedges.
    pub const CHECKPOINT: Self = Self {
        reads: AliasDomain::EMPTY,
        writes: AliasDomain::EMPTY,
        control: ControlEffect(ControlEffect::MAY_UNWIND.0 | ControlEffect::MAY_SUSPEND.0),
        barrier: BarrierEffect::REQUIRED_WHEN_UNUSED,
    };

    /// Pointwise union of two effect descriptors.
    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self {
            reads: self.reads.union(other.reads),
            writes: self.writes.union(other.writes),
            control: self.control.union(other.control),
            barrier: self.barrier.union(other.barrier),
        }
    }

    /// Returns `true` when no read, write, control, or barrier flags are present.
    #[inline]
    pub const fn is_pure(self) -> bool {
        self.reads.is_empty()
            && self.writes.is_empty()
            && self.control.is_empty()
            && self.barrier.is_empty()
    }

    /// Returns `true` if execution may trigger Immix garbage collection.
    #[inline]
    pub const fn may_gc(self) -> bool {
        self.barrier.contains(BarrierEffect::MAY_GC)
    }

    /// Returns `true` if execution may non-locally unwind out of the current dynamic scope.
    #[inline]
    pub const fn may_unwind(self) -> bool {
        self.control.contains(ControlEffect::MAY_UNWIND)
    }

    /// Returns `true` if a speculative guard may deoptimize back to bytecode.
    #[inline]
    pub const fn may_deopt(self) -> bool {
        self.control.contains(ControlEffect::MAY_DEOPT)
    }

    /// Returns `true` if execution may cooperatively suspend the current task.
    #[inline]
    pub const fn may_suspend(self) -> bool {
        self.control.contains(ControlEffect::MAY_SUSPEND)
    }

    /// Returns `true` if execution may fail to terminate.
    #[inline]
    pub const fn may_diverge(self) -> bool {
        self.control.contains(ControlEffect::MAY_DIVERGE)
    }

    /// Returns `true` if execution allocates a fresh heap object with `EQ` identity.
    #[inline]
    pub const fn allocates(self) -> bool {
        self.barrier.contains(BarrierEffect::ALLOC_FRESH)
    }

    /// Returns `true` if DCE can remove this operation when its result is unused.
    #[inline]
    pub const fn can_eliminate_if_unused(self) -> bool {
        self.writes.is_empty()
            && self.control.is_empty()
            && !self.barrier.contains(BarrierEffect::REQUIRED_WHEN_UNUSED)
    }

    /// Returns `true` if this operation can be safely duplicated or CSE'd without changing `EQ` identity.
    #[inline]
    pub const fn can_duplicate(self) -> bool {
        self.can_eliminate_if_unused() && !self.barrier.contains(BarrierEffect::ALLOC_FRESH)
    }

    /// Returns `true` if this operation can be speculatively executed on a path where it would not run.
    #[inline]
    pub const fn can_speculate(self) -> bool {
        self.can_eliminate_if_unused()
            && !self.barrier.contains(BarrierEffect::DEPENDS_ON_CHECKS)
            && !self.barrier.contains(BarrierEffect::MAY_GC)
    }

    /// Returns `true` if `self` followed immediately by `next` can be reordered to `next` then `self`.
    #[inline]
    pub const fn can_reorder(self, next: Self) -> bool {
        self.writes.disjoint(next.reads.union(next.writes))
            && next.writes.disjoint(self.reads)
            && self.control.is_empty()
            && next.control.is_empty()
            && !(next.barrier.contains(BarrierEffect::DEPENDS_ON_CHECKS)
                && (self.barrier.contains(BarrierEffect::REQUIRED_WHEN_UNUSED)
                    || self.control.contains(ControlEffect::MAY_DEOPT)))
    }

    /// Returns `true` if this operation can be hoisted out of a loop with aggregate `loop_effects`.
    #[inline]
    pub const fn can_hoist_out_of_loop(self, loop_effects: Self) -> bool {
        self.can_speculate() && self.reads.disjoint(loop_effects.writes)
    }
}

impl BitOr for OpEffects {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl BitOrAssign for OpEffects {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_effects_block_dce_and_check_dependent_hoisting() {
        let guard = OpEffects {
            reads: AliasDomain::SYMBOL_FN,
            writes: AliasDomain::EMPTY,
            control: ControlEffect::MAY_DEOPT,
            barrier: BarrierEffect::REQUIRED_WHEN_UNUSED,
        };
        let unchecked_arith = OpEffects {
            barrier: BarrierEffect::DEPENDS_ON_CHECKS,
            ..OpEffects::PURE
        };

        assert!(!guard.can_eliminate_if_unused());
        assert!(!guard.may_gc());
        assert!(guard.may_deopt());

        assert!(unchecked_arith.can_eliminate_if_unused());
        assert!(!unchecked_arith.can_speculate());
        assert!(!guard.can_reorder(unchecked_arith));
    }

    #[test]
    fn fresh_allocation_allows_dce_but_blocks_duplication_and_speculation() {
        let alloc = OpEffects {
            barrier: BarrierEffect::ALLOC_FRESH.union(BarrierEffect::MAY_GC),
            ..OpEffects::PURE
        };

        assert!(alloc.allocates());
        assert!(alloc.may_gc());
        assert!(alloc.can_eliminate_if_unused());
        assert!(!alloc.can_duplicate());
        assert!(!alloc.can_speculate());
    }

    #[test]
    fn disjoint_alias_domains_allow_reordering_and_loop_hoisting() {
        let read_box = OpEffects {
            reads: AliasDomain::BOX_CELL,
            ..OpEffects::PURE
        };
        let write_array = OpEffects {
            writes: AliasDomain::ARRAY_ELEM,
            ..OpEffects::PURE
        };

        assert!(write_array.can_reorder(read_box));
        assert!(read_box.can_hoist_out_of_loop(write_array));

        let write_box = OpEffects {
            writes: AliasDomain::BOX_CELL,
            ..OpEffects::PURE
        };
        assert!(!write_box.can_reorder(read_box));
        assert!(!read_box.can_hoist_out_of_loop(write_box));
    }

    #[test]
    fn cl_signal_and_checkpoint_enforce_exact_contracts() {
        assert!(OpEffects::CL_SIGNAL.may_gc());
        assert!(OpEffects::CL_SIGNAL.may_unwind());
        assert!(OpEffects::CL_SIGNAL.may_suspend());
        assert!(OpEffects::CL_SIGNAL.may_diverge());
        assert!(!OpEffects::CL_SIGNAL.can_eliminate_if_unused());

        assert!(OpEffects::CHECKPOINT.may_unwind());
        assert!(OpEffects::CHECKPOINT.may_suspend());
        assert!(!OpEffects::CHECKPOINT.may_gc());
        assert!(!OpEffects::CHECKPOINT.can_eliminate_if_unused());
    }
}
