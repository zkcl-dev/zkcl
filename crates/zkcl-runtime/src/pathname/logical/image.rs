use super::*;
use crate::image::{ImageReader, ImageWriter};

impl LogicalHosts {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) -> Result<(), EvalError> {
        image.usize(self.rules.len());
        let mut rules: Vec<_> = self.rules.iter().collect();
        rules.sort_by_key(|(host, _)| *host);
        for (host, translations) in rules {
            image.string(host);
            image.usize(translations.len());
            for translation in translations {
                translation.from.write_image(image);
                translation.to.write_image(image);
            }
        }
        Ok(())
    }

    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let mut rules = HashMap::new();
        for _ in 0..image.usize()? {
            let host = image.string()?;
            if canonical_host(&host).as_ref() != Some(&host) {
                return Err(EvalError::new(0, "invalid image logical host"));
            }
            let mut translations = Vec::new();
            for _ in 0..image.usize()? {
                let from = Pathname::read_image(image)?;
                let to = Pathname::read_image(image)?;
                if from.logical_host.as_ref() != Some(&host) || !valid_components(&from) {
                    return Err(EvalError::new(
                        0,
                        "invalid image logical pathname translation",
                    ));
                }
                translations.push(Translation { from, to });
            }
            if rules.insert(host, translations).is_some() {
                return Err(EvalError::new(0, "duplicate image logical host"));
            }
        }
        Ok(Self { rules })
    }
}

// A namestring cannot distinguish every component state. Store the actual
// fields, including physical :UNSPECIFIC and logical version components.
impl Pathname {
    pub(crate) fn write_image(&self, image: &mut ImageWriter<'_>) {
        image.bool(self.logical_host.is_some());
        if let Some(host) = &self.logical_host {
            image.string(host);
        }
        image.bool(self.host_unspecific);
        image.bool(self.device_unspecific);
        image.bool(self.directory.is_some());
        if let Some(directory) = &self.directory {
            image.bool(directory.absolute);
            image.usize(directory.parts.len());
            for part in &directory.parts {
                match part {
                    DirectoryPart::Text(text) => {
                        image.usize(0);
                        image.string(text);
                    }
                    DirectoryPart::Up => image.usize(1),
                    DirectoryPart::Back => image.usize(2),
                    DirectoryPart::Wild => image.usize(3),
                    DirectoryPart::WildInferiors => image.usize(4),
                }
            }
        }
        write_leaf(image, &self.name);
        write_leaf(image, &self.file_type);
        match self.version {
            Version::Missing => image.usize(0),
            Version::Unspecific => image.usize(1),
            Version::Newest => image.usize(2),
            Version::Wild => image.usize(3),
            Version::Number(number) => {
                image.usize(4);
                image.string(&number.to_string());
            }
        }
    }

    pub(crate) fn read_image(image: &mut ImageReader<'_>) -> Result<Self, EvalError> {
        let logical_host = image.bool()?.then(|| image.string()).transpose()?;
        let host_unspecific = image.bool()?;
        let device_unspecific = image.bool()?;
        let directory = if image.bool()? {
            let absolute = image.bool()?;
            let mut parts = Vec::new();
            for _ in 0..image.usize()? {
                parts.push(match image.usize()? {
                    0 => DirectoryPart::Text(image.string()?),
                    1 => DirectoryPart::Up,
                    2 => DirectoryPart::Back,
                    3 => DirectoryPart::Wild,
                    4 => DirectoryPart::WildInferiors,
                    _ => return Err(EvalError::new(0, "invalid image pathname directory")),
                });
            }
            Some(Directory { absolute, parts })
        } else {
            None
        };
        let name = read_leaf(image)?;
        let file_type = read_leaf(image)?;
        let version = match image.usize()? {
            0 => Version::Missing,
            1 => Version::Unspecific,
            2 => Version::Newest,
            3 => Version::Wild,
            4 => Version::Number(
                image
                    .string()?
                    .parse()
                    .map_err(|_| EvalError::new(0, "invalid image pathname version"))?,
            ),
            _ => return Err(EvalError::new(0, "invalid image pathname version")),
        };
        let pathname = Self {
            logical_host,
            host_unspecific,
            device_unspecific,
            directory,
            name,
            file_type,
            version,
        };
        if pathname.logical_host.is_some() && !valid_components(&pathname) {
            return Err(EvalError::new(
                0,
                "invalid image logical pathname components",
            ));
        }
        Ok(pathname)
    }
}

fn write_leaf(image: &mut ImageWriter<'_>, leaf: &Leaf) {
    match leaf {
        Leaf::Missing => image.usize(0),
        Leaf::Unspecific => image.usize(1),
        Leaf::Text(text) => {
            image.usize(2);
            image.string(text);
        }
        Leaf::Wild => image.usize(3),
    }
}

fn read_leaf(image: &mut ImageReader<'_>) -> Result<Leaf, EvalError> {
    Ok(match image.usize()? {
        0 => Leaf::Missing,
        1 => Leaf::Unspecific,
        2 => Leaf::Text(image.string()?),
        3 => Leaf::Wild,
        _ => return Err(EvalError::new(0, "invalid image pathname leaf")),
    })
}
