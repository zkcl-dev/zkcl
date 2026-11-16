use super::*;
use crate::image::{ImageReader, ImageWriter};

impl Finalizers {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        let waiting = self.waiting.borrow();
        image.usize(waiting.len());
        for registration in waiting.iter() {
            image.word(registration.pointer.get())?;
            image.word(registration.function.get())?;
        }
        let pending = self.pending.borrow();
        image.usize(pending.len());
        for function in pending.iter() {
            image.word(function.get())?;
        }
        Ok(())
    }

    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let mut waiting = Vec::new();
        for _ in 0..image.usize()? {
            waiting.push(FinalizerRegistration {
                pointer: image.runtime.root_slot(image.word()?),
                function: image.runtime.root_slot(image.word()?),
            });
        }
        let mut pending = Vec::new();
        for _ in 0..image.usize()? {
            pending.push(image.runtime.root_slot(image.word()?));
        }
        Ok(Self {
            waiting: waiting.into(),
            pending: pending.into(),
            // Collection epochs belong to the destination heap. Inspect weak
            // registrations at the first boundary even when its epoch is zero.
            last_collection: Cell::new(u64::MAX),
            delivering: Cell::new(false),
        })
    }
}
