use super::*;
use crate::image::{ImageReader, ImageWriter, invalid};
impl Form {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        let _nesting = image.enter_nested()?;
        image.usize(self.at);
        image.usize(self.source.at);
        image.bool(self.source.range.is_some());
        if let Some(range) = &self.source.range {
            image.usize(range.start);
            image.usize(range.end);
        }
        image.usize(self.source.origin as usize);
        image.usize(self.source.expansion.len());
        for range in &self.source.expansion {
            image.usize(range.start);
            image.usize(range.end);
        }
        match &self.original_object {
            None => image.usize(0),
            Some(FormObject::NeedsSyntax(root)) => {
                image.usize(1);
                image.word(root.get())?;
            }
            Some(FormObject::WithSyntax(root)) => {
                image.usize(2);
                image.word(root.get())?;
            }
        }
        match &self.kind {
            FormKind::Integer(value) => {
                image.usize(0);
                image.string(&value.to_string());
            }
            FormKind::Character(value) => {
                image.usize(1);
                image.usize(*value as usize);
            }
            FormKind::String(value) => {
                image.usize(2);
                image.string(value);
            }
            FormKind::Literal(root) => {
                image.usize(3);
                image.word(root.get())?;
            }
            FormKind::Name(name) => {
                image.usize(4);
                image.name(name)?;
            }
            FormKind::List(items, tail) => {
                image.usize(5);
                image.usize(items.len());
                for item in items {
                    item.write_image(image)?;
                }
                image.bool(tail.is_some());
                if let Some(tail) = tail {
                    tail.write_image(image)?;
                }
            }
        }
        Ok(())
    }
    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let _nesting = image.enter_nested()?;
        let at = image.usize()?;
        let source_at = image.usize()?;
        let source_range = if image.bool()? {
            let start = image.usize()?;
            let end = image.usize()?;
            if start > end {
                return Err(invalid());
            }
            Some(start..end)
        } else {
            None
        };
        let origin = match image.usize()? {
            0 => FormOrigin::Original,
            1 => FormOrigin::Reader,
            2 => FormOrigin::Macro,
            3 => FormOrigin::Compiler,
            _ => return Err(invalid()),
        };
        let mut expansion = Vec::new();
        for _ in 0..image.usize()? {
            let start = image.usize()?;
            let end = image.usize()?;
            if start > end {
                return Err(invalid());
            }
            expansion.push(start..end);
        }
        let original_object = match image.usize()? {
            0 => None,
            1 => Some(FormObject::NeedsSyntax(Rc::new(image.runtime.root_slot(
                image.word()?,
            )))),
            2 => Some(FormObject::WithSyntax(Rc::new(image.runtime.root_slot(
                image.word()?,
            )))),
            _ => return Err(invalid()),
        };
        let kind = match image.usize()? {
            0 => FormKind::Integer(image.string()?.parse().map_err(|_| invalid())?),
            1 => FormKind::Character(
                char::from_u32(u32::try_from(image.usize()?).map_err(|_| invalid())?)
                    .ok_or_else(invalid)?,
            ),
            2 => FormKind::String(image.string()?),
            3 => FormKind::Literal(Rc::new(image.runtime.root_slot(image.word()?))),
            4 => FormKind::Name(image.name()?),
            5 => {
                let mut items = Vec::new();
                for _ in 0..image.usize()? {
                    items.push(Self::read_image(image)?);
                }
                let tail = if image.bool()? {
                    Some(Box::new(Self::read_image(image)?))
                } else {
                    None
                };
                FormKind::List(items, tail)
            }
            _ => return Err(invalid()),
        };
        Ok(Self {
            at,
            kind,
            original_object,
            source: FormSource {
                at: source_at,
                range: source_range,
                origin,
                expansion,
            },
        })
    }
}
