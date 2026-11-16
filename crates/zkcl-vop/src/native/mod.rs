//! Native ABI selection and the shared layout of machine-code leaves.

#[allow(
    clippy::useless_conversion,
    reason = "dynasm! expands register expressions with .into()"
)]
mod aarch64;
mod armv7;
#[allow(
    clippy::useless_conversion,
    reason = "dynasm! expands register expressions with .into()"
)]
mod i686;
#[allow(
    clippy::useless_conversion,
    reason = "dynasm! expands register expressions with .into()"
)]
mod riscv64;
#[allow(
    clippy::useless_conversion,
    reason = "dynasm! expands register expressions with .into()"
)]
mod x86_64;

use target_lexicon::{
    Architecture, ArmArchitecture, CallingConvention, Endianness, Environment, OperatingSystem,
    PointerWidth, Riscv64Architecture, Triple, X86_32Architecture,
};
use zkcl_bytecode::native_abi::LEAF_MISS;

use crate::LeafFunction;
use crate::leaf::{BitwiseOp, Exit, Operand, Operation, Step, Test};

/// The instruction set and calling convention of a little-endian native leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Aarch64,
    Armv7,
    I686,
    Riscv64,
    X86_64SysV,
    X86_64Windows,
}

impl Target {
    /// Select the instruction set and C ABI using the destination's actual word layout.
    pub fn from_triple(triple: &Triple) -> Option<Self> {
        use CallingConvention::{AppleAarch64, SystemV, WindowsFastcall};
        if triple.endianness().ok()? != Endianness::Little {
            return None;
        }
        match (
            triple.architecture,
            triple.pointer_width().ok()?,
            triple.default_calling_convention().ok()?,
        ) {
            (
                Architecture::Aarch64(_),
                PointerWidth::U64,
                AppleAarch64 | SystemV | WindowsFastcall,
            ) => Some(Self::Aarch64),
            (Architecture::X86_64, PointerWidth::U64, SystemV) => Some(Self::X86_64SysV),
            (Architecture::X86_64, PointerWidth::U64, WindowsFastcall) => Some(Self::X86_64Windows),
            (Architecture::X86_32(X86_32Architecture::I686), PointerWidth::U32, SystemV)
                if triple.operating_system == OperatingSystem::Linux =>
            {
                Some(Self::I686)
            }
            (Architecture::Riscv64(Riscv64Architecture::Riscv64gc), PointerWidth::U64, SystemV)
                if triple.operating_system == OperatingSystem::Linux
                    && triple.environment == Environment::Gnu =>
            {
                Some(Self::Riscv64)
            }
            (
                Architecture::Arm(ArmArchitecture::Armv7 | ArmArchitecture::Armv7a),
                PointerWidth::U32,
                SystemV,
            ) if triple.operating_system == OperatingSystem::Linux
                && matches!(
                    triple.environment,
                    Environment::Gnueabi
                        | Environment::Gnueabihf
                        | Environment::Musleabi
                        | Environment::Musleabihf
                        | Environment::Androideabi
                ) =>
            {
                Some(Self::Armv7)
            }
            _ => None,
        }
    }

    pub fn word_bits(self) -> u8 {
        match self {
            Self::Armv7 | Self::I686 => 32,
            Self::Aarch64 | Self::Riscv64 | Self::X86_64SysV | Self::X86_64Windows => 64,
        }
    }

    /// A tail branch for a linker to bind to the runtime guard dispatcher.
    pub fn emit_dispatch_jump(self) -> Vec<u8> {
        match self {
            Self::Aarch64 => aarch64::dispatch_jump(),
            Self::Armv7 => armv7::dispatch_jump(),
            Self::I686 => i686::dispatch_jump(),
            Self::Riscv64 => riscv64::dispatch_jump(),
            Self::X86_64SysV | Self::X86_64Windows => x86_64::dispatch_jump(),
        }
    }

    fn emit(self, function: &LeafFunction, runtime_entry: bool) -> Option<Vec<u8>> {
        if self.word_bits() != function.word_bits() {
            return None;
        }
        match self {
            Self::Aarch64 => aarch64::emit(function, runtime_entry),
            Self::Armv7 => armv7::emit(function, runtime_entry),
            Self::I686 => i686::emit(function, runtime_entry),
            Self::Riscv64 => riscv64::emit(function, runtime_entry),
            Self::X86_64SysV | Self::X86_64Windows => {
                x86_64::emit(function, self == Self::X86_64Windows, runtime_entry)
            }
        }
    }
}

impl LeafFunction {
    /// Emit the scalar C ABI leaf when it can run without fallbacks or binding checks.
    pub fn emit(&self, target: Target) -> Option<Vec<u8>> {
        if self.needs_dispatch() {
            return None;
        }
        self.emit_guarded(target)
    }

    /// Emit position-independent code taking four tagged C ABI arguments. The
    /// caller checks [`Self::guards`] and handles the `LEAF_MISS` result.
    pub fn emit_guarded(&self, target: Target) -> Option<Vec<u8>> {
        target.emit(self, false)
    }

    /// Emit a self-contained `JitCEntryFn` for a leaf that always returns a value.
    pub fn emit_entry(&self, target: Target) -> Option<Vec<u8>> {
        if self.needs_dispatch() {
            return None;
        }
        target.emit(self, true)
    }
}

trait Assembler {
    type Label: Copy + Eq;

    fn label(&mut self) -> Self::Label;
    fn bind(&mut self, label: Self::Label);
    fn jump(&mut self, label: Self::Label);
    fn initialize_loop_budget(&mut self, budget: u32);
    fn take_backedge(&mut self, miss: Self::Label);
    fn test(
        &mut self,
        value: Operand,
        test: Test,
        yes: Self::Label,
        no: Self::Label,
        next: Option<Self::Label>,
    );
    fn copy(&mut self, destination: usize, value: Operand);
    fn compute(&mut self, destination: usize, operation: Operation, miss: Option<Self::Label>);
    fn return_value(&mut self, value: Operand);
}

fn emit_body(asm: &mut impl Assembler, function: &LeafFunction) {
    let labels: Vec<_> = function.blocks().iter().map(|_| asm.label()).collect();
    if let Some(budget) = function.loop_budget() {
        asm.initialize_loop_budget(budget);
    }
    let miss = function
        .blocks()
        .iter()
        .flat_map(|block| &block.steps)
        .any(Step::may_miss)
        .then(|| asm.label());
    for (index, block) in function.blocks().iter().enumerate() {
        asm.bind(labels[index]);
        for &step in &block.steps {
            match step {
                Step::CheckFixnum(value) => {
                    let pass = asm.label();
                    asm.test(value, Test::Fixnum, pass, miss.unwrap(), Some(pass));
                    asm.bind(pass);
                }
                Step::CheckLoopBudget => asm.take_backedge(miss.expect("loop budget miss")),
                Step::Copy { destination, value } => asm.copy(destination, value),
                Step::Compute { destination, op } => asm.compute(destination, op, miss),
            }
        }
        match block.exit {
            Exit::Return(value) => asm.return_value(value),
            Exit::Miss => asm.return_value(Operand::Immediate(LEAF_MISS)),
            Exit::Jump(target) => {
                if target != index + 1 {
                    asm.jump(labels[target]);
                }
            }
            Exit::Branch {
                value,
                test,
                yes,
                no,
            } => {
                asm.test(
                    value,
                    test,
                    labels[yes],
                    labels[no],
                    labels.get(index + 1).copied(),
                );
            }
        }
    }
    if let Some(miss) = miss {
        asm.bind(miss);
        asm.return_value(Operand::Immediate(LEAF_MISS));
    }
}
