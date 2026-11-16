use super::*;
use crate::image::{ImageReader, ImageWriter};

impl InvocationProtocol {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        image.usize(self.operations.len());
        for operation in &self.operations {
            image.word(operation.symbol.get())?;
            image.word(operation.function.get())?;
            write_roots(image, &operation.defaults)?;
            image.bool(operation.on_method);
        }
        let extensions = self.extensions.borrow();
        write_roots(image, &extensions.generics)?;
        write_roots(image, &extensions.methods)?;
        image.usize(self.slot_protocols.len());
        for slot in &self.slot_protocols {
            image.usize(match slot.op {
                SlotOp::Value => 0,
                SlotOp::SetValue => 1,
                SlotOp::Boundp => 2,
                SlotOp::ExistsP => 3,
                SlotOp::Makunbound => 4,
            });
            image.word(slot.function.get())?;
            image.word(slot.lambda_list.get())?;
            image.word(slot.combination.get())?;
            write_roots(image, &slot.methods)?;
        }
        image.word(self.standard_class.get())?;
        image.word(self.standard_effective_slot.get())?;
        image.word(self.function.get())?;
        image.word(self.computing.get())
    }

    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let mut operations = Vec::new();
        for _ in 0..image.usize()? {
            operations.push(ProtocolOperation {
                symbol: image.runtime.root_slot(image.word()?),
                function: image.runtime.root_slot(image.word()?),
                defaults: read_roots(image)?,
                on_method: image.bool()?,
            });
        }
        if operations.len() < 2 {
            return Err(EvalError::new(0, "invalid image invocation operations"));
        }
        let extensions = RefCell::new(ProtocolExtensions {
            generics: read_roots(image)?,
            methods: read_roots(image)?,
        });
        let mut slot_protocols = Vec::new();
        for _ in 0..image.usize()? {
            let op = match image.usize()? {
                0 => SlotOp::Value,
                1 => SlotOp::SetValue,
                2 => SlotOp::Boundp,
                3 => SlotOp::ExistsP,
                4 => SlotOp::Makunbound,
                _ => return Err(EvalError::new(0, "invalid image invocation slot operation")),
            };
            slot_protocols.push(SlotProtocol {
                op,
                function: image.runtime.root_slot(image.word()?),
                lambda_list: image.runtime.root_slot(image.word()?),
                combination: image.runtime.root_slot(image.word()?),
                methods: read_roots(image)?,
            });
        }
        if [
            SlotOp::Value,
            SlotOp::SetValue,
            SlotOp::Boundp,
            SlotOp::Makunbound,
        ]
        .iter()
        .any(|&op| slot_protocols.iter().filter(|slot| slot.op == op).count() != 1)
        {
            return Err(EvalError::new(0, "invalid image invocation slot protocols"));
        }
        Ok(Self {
            operations,
            extensions,
            slot_protocols,
            standard_class: image.runtime.root_slot(image.word()?),
            standard_effective_slot: image.runtime.root_slot(image.word()?),
            function: image.runtime.root_slot(image.word()?),
            computing: image.runtime.root_slot(image.word()?),
        })
    }
}

fn write_roots(image: &mut ImageWriter<'_>, roots: &[RootSlot]) -> Result<(), EvalError> {
    image.usize(roots.len());
    for root in roots {
        image.word(root.get())?;
    }
    Ok(())
}

fn read_roots(image: &mut ImageReader<'_>) -> Result<Vec<RootSlot>, EvalError> {
    let mut roots = Vec::new();
    for _ in 0..image.usize()? {
        roots.push(image.runtime.root_slot(image.word()?));
    }
    Ok(roots)
}
