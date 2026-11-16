//! Dispatch of predecoded operations with a resumable program counter.

use core::ops::ControlFlow;
use core::task::Poll;

pub trait DispatchFrame {
    type Instruction: Copy;
    type Exit;

    fn instructions(&self) -> &[Self::Instruction];
    fn pc(&mut self) -> &mut usize;
    fn execute(&mut self, pc: usize, instruction: Self::Instruction) -> ControlFlow<Self::Exit>;
    fn checkpoint(&mut self) -> ControlFlow<Self::Exit>;
}

pub enum DispatchOutcome<T> {
    EndOfFunction,
    Exit(T),
}

#[inline]
pub fn run_frame<F: DispatchFrame>(frame: &mut F) -> DispatchOutcome<F::Exit> {
    loop {
        let pc = *frame.pc();
        let Some(&instruction) = frame.instructions().get(pc) else {
            return DispatchOutcome::EndOfFunction;
        };
        *frame.pc() = pc + 1;
        if let ControlFlow::Break(exit) = frame.execute(pc, instruction) {
            return DispatchOutcome::Exit(exit);
        }
        if let ControlFlow::Break(exit) = frame.checkpoint() {
            return DispatchOutcome::Exit(exit);
        }
    }
}

pub async fn yield_now() {
    let mut yielded = false;
    core::future::poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}
