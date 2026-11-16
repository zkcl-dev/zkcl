//! Common Lisp pathnames and local filesystem namestrings.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use crate::condition::Fault;
use crate::heap::{ListSpine, Word};
use crate::package::Package;
use crate::symbol::Name;
use crate::{EvalError, Root, Runtime};

mod logical;

pub(crate) use logical::{LogicalHosts, hosts_equal, logical_host, set_translations, translations};

pub(crate) const COMPILED_FILE_TYPE: &str = "zkclfasl";

pub(crate) fn compiled_output(input: &Pathname, output: Option<&Pathname>) -> Pathname {
    let mut defaults = input.clone();
    defaults.file_type = Leaf::Text(if input.logical_host.is_some() {
        COMPILED_FILE_TYPE.to_ascii_uppercase()
    } else {
        COMPILED_FILE_TYPE.into()
    });
    match output {
        Some(output) => output.merge(&defaults),
        None => defaults,
    }
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) struct Pathname {
    pub(crate) logical_host: Option<String>,
    pub(crate) host_unspecific: bool,
    pub(crate) device_unspecific: bool,
    pub(crate) directory: Option<Directory>,
    pub(crate) name: Leaf,
    pub(crate) file_type: Leaf,
    pub(crate) version: Version,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Directory {
    pub(crate) absolute: bool,
    pub(crate) parts: Vec<DirectoryPart>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum DirectoryPart {
    Text(String),
    Up,
    Back,
    Wild,
    WildInferiors,
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum Leaf {
    #[default]
    Missing,
    Unspecific,
    Text(String),
    Wild,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum Version {
    #[default]
    Missing,
    Unspecific,
    Newest,
    Wild,
    Number(isize),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PathnameError {
    #[error("invalid logical pathname namestring")]
    LogicalSyntax,
    #[error("invalid logical pathname namestring")]
    LogicalVersion(#[source] std::num::ParseIntError),
    #[error("no logical pathname host named {0}")]
    UnknownHost(String),
    #[error("logical pathname host does not match the requested host")]
    HostMismatch,
    #[error("no applicable logical pathname translation")]
    NoTranslation,
    #[error("logical pathname translation repeats a pathname")]
    TranslationCycle,
    #[error("a Unix namestring cannot contain a NUL character")]
    NullCharacter,
    #[error("dangling escape in namestring")]
    IncompleteEscape,
    #[error("filesystem pathname is not UTF-8")]
    NonUtf8,
    #[error("pathname is not representable on the local filesystem")]
    Unrepresentable,
    #[error(":UP or :BACK cannot immediately follow :ABSOLUTE or :WILD-INFERIORS")]
    InvalidUpwardComponent,
    #[error("expected a concrete pathname rather than a wild pathname")]
    Wild,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Field {
    Host,
    Device,
    Directory,
    Name,
    Type,
    Version,
}

impl Field {
    fn index(self) -> usize {
        self as usize
    }
}

impl Pathname {
    pub(crate) fn parse(text: &str) -> Result<Self, PathnameError> {
        Self::parse_text(text, true, false)
    }

    pub(crate) fn from_path(path: &Path) -> Result<Self, PathnameError> {
        let text = path.to_str().ok_or(PathnameError::NonUtf8)?;
        Self::parse_text(text, false, false)
    }

    pub(crate) fn from_directory_path(path: &Path) -> Result<Self, PathnameError> {
        let text = path.to_str().ok_or(PathnameError::NonUtf8)?;
        Self::parse_text(text, false, true)
    }

    fn parse_text(text: &str, syntax: bool, directory_only: bool) -> Result<Self, PathnameError> {
        if text.contains('\0') {
            return Err(PathnameError::NullCharacter);
        }
        if text.is_empty() {
            return Ok(Self::default());
        }

        let mut segments: Vec<_> = text.split('/').filter(|part| !part.is_empty()).collect();
        let absolute = text.starts_with('/');
        let mut last_is_directory = false;
        if let Some(last) = segments.last() {
            if syntax {
                last_is_directory = matches!(unescape(last)?.as_str(), "." | "..");
            } else {
                last_is_directory = matches!(*last, "." | "..");
            }
        }
        let file = if directory_only || last_is_directory || text.ends_with('/') {
            None
        } else {
            segments.pop()
        };

        let mut parts = Vec::with_capacity(segments.len());
        for part in segments {
            let part = match part {
                "*" if syntax => DirectoryPart::Wild,
                "**" if syntax => DirectoryPart::WildInferiors,
                _ => {
                    let part = if syntax { unescape(part)? } else { part.into() };
                    match part.as_str() {
                        "." => continue,
                        ".." => DirectoryPart::Up,
                        _ => DirectoryPart::Text(part),
                    }
                }
            };
            parts.push(part);
        }
        let directory = (absolute || text.contains('/') || directory_only || last_is_directory)
            .then_some(Directory { absolute, parts });

        let (name, file_type, version) = if let Some(file) = file {
            let dot = if syntax {
                last_unescaped_dot(file)?
            } else {
                file.rfind('.').filter(|&index| index != 0)
            };
            let (name, file_type) = if let Some(index) = dot {
                (
                    parse_leaf(&file[..index], syntax)?,
                    parse_leaf(&file[index + 1..], syntax)?,
                )
            } else {
                (parse_leaf(file, syntax)?, Leaf::Missing)
            };
            (name, file_type, Version::Newest)
        } else {
            (Leaf::Missing, Leaf::Missing, Version::Missing)
        };

        Ok(Self {
            logical_host: None,
            host_unspecific: false,
            device_unspecific: false,
            directory,
            name,
            file_type,
            version,
        })
    }

    pub(crate) fn merge(&self, defaults: &Self) -> Self {
        self.merge_with(defaults, Version::Newest)
    }

    pub(crate) fn merge_with(&self, defaults: &Self, default_version: Version) -> Self {
        let converted;
        let defaults = if self.logical_host.is_some() != defaults.logical_host.is_some() {
            converted = defaults.with_case(self.logical_host.is_some());
            &converted
        } else {
            defaults
        };
        let directory = match (&self.directory, &defaults.directory) {
            (None, other) => other.clone(),
            (Some(relative), Some(base)) if !relative.absolute => Some(Directory {
                absolute: base.absolute,
                parts: resolve_back(base.parts.iter().chain(&relative.parts)),
            }),
            (directory, _) => directory.clone(),
        };
        let version = if self.version != Version::Missing {
            self.version
        } else if self.name == Leaf::Missing && defaults.version != Version::Missing {
            defaults.version
        } else {
            default_version
        };
        Self {
            logical_host: self.logical_host.clone(),
            host_unspecific: self.logical_host.is_none()
                && (self.host_unspecific || defaults.host_unspecific),
            device_unspecific: self.logical_host.is_some()
                || self.device_unspecific
                || defaults.device_unspecific,
            directory,
            name: self.name.or(&defaults.name),
            file_type: self.file_type.or(&defaults.file_type),
            version,
        }
    }

    pub(crate) fn namestring(&self) -> Option<String> {
        let mut result = String::new();
        if let Some(host) = &self.logical_host {
            result.push_str(host);
            result.push(':');
        }
        result.push_str(&self.directory_namestring());
        result.push_str(&self.file_namestring()?);
        Some(result)
    }

    pub(crate) fn file_namestring(&self) -> Option<String> {
        if self.logical_host.is_some() {
            logical::file_namestring(self)
        } else {
            self.render_file(true)
        }
    }

    pub(crate) fn directory_namestring(&self) -> String {
        if self.logical_host.is_some() {
            logical::directory_namestring(self.directory.as_ref())
        } else {
            render_directory(self.directory.as_ref(), true)
        }
    }

    pub(crate) fn file_path(&self) -> Result<PathBuf, PathnameError> {
        let directory = self.local_directory()?;
        if self.is_wild() {
            return Err(PathnameError::Wild);
        }
        let mut result = render_directory(directory.as_deref(), false);
        result.push_str(
            &self
                .render_file(false)
                .ok_or(PathnameError::Unrepresentable)?,
        );
        Ok(result.into())
    }

    pub(crate) fn directory_path(&self) -> Result<PathBuf, PathnameError> {
        let directory = self.local_directory()?;
        if self.wild_directory() {
            return Err(PathnameError::Wild);
        }
        Ok(render_directory(directory.as_deref(), false).into())
    }

    pub(crate) fn local_directory(&self) -> Result<Option<Cow<'_, Directory>>, PathnameError> {
        if self.logical_host.is_some() {
            return Err(PathnameError::Unrepresentable);
        }
        let Some(directory) = &self.directory else {
            return Ok(None);
        };
        let upward = |part: &DirectoryPart| matches!(part, DirectoryPart::Up | DirectoryPart::Back);
        let directory = if directory.parts.contains(&DirectoryPart::Back) {
            Cow::Owned(Directory {
                absolute: directory.absolute,
                parts: resolve_back(directory.parts.iter()),
            })
        } else {
            Cow::Borrowed(directory)
        };
        if (directory.absolute && directory.parts.first().is_some_and(upward))
            || directory
                .parts
                .windows(2)
                .any(|pair| pair[0] == DirectoryPart::WildInferiors && upward(&pair[1]))
        {
            return Err(PathnameError::InvalidUpwardComponent);
        }
        Ok(Some(directory))
    }

    pub(crate) fn is_wild(&self) -> bool {
        self.wild_leaf(&self.name)
            || self.wild_leaf(&self.file_type)
            || self.version == Version::Wild
            || self.wild_directory()
    }

    pub(crate) fn wild_leaf(&self, leaf: &Leaf) -> bool {
        leaf == &Leaf::Wild
            || self.logical_host.is_some() && matches!(leaf, Leaf::Text(text) if text.contains('*'))
    }

    pub(crate) fn wild_directory(&self) -> bool {
        self.directory.as_ref().is_some_and(|directory| {
            directory.parts.iter().any(|part| {
                matches!(part, DirectoryPart::Wild | DirectoryPart::WildInferiors)
                    || self.logical_host.is_some()
                        && matches!(part, DirectoryPart::Text(text) if text.contains('*'))
            })
        })
    }

    fn with_case(&self, logical: bool) -> Self {
        let mut result = self.clone();
        let convert = |text: &mut String| {
            if logical {
                text.make_ascii_uppercase();
            } else {
                text.make_ascii_lowercase();
            }
        };
        if let Some(directory) = &mut result.directory {
            for part in &mut directory.parts {
                if let DirectoryPart::Text(text) = part {
                    convert(text);
                }
            }
        }
        for field in [&mut result.name, &mut result.file_type] {
            if let Leaf::Text(text) = field {
                convert(text);
            }
        }
        result
    }

    fn render_file(&self, escaped: bool) -> Option<String> {
        if matches!(self.version, Version::Number(_) | Version::Wild) {
            return None;
        }
        let mut result = String::new();
        if let Some(name) = self.name.text() {
            if escaped && matches!(self.name, Leaf::Text(_)) {
                let escape_dots = self.file_type.text().is_none();
                escape(&mut result, name, escape_dots, false);
            } else {
                result.push_str(name);
            }
        } else if self.file_type.text().is_some() {
            return None;
        }
        if let Some(file_type) = self.file_type.text() {
            result.push('.');
            if escaped && matches!(self.file_type, Leaf::Text(_)) {
                escape(&mut result, file_type, true, true);
            } else {
                result.push_str(file_type);
            }
        }
        Some(result)
    }

    pub(crate) fn to_lisp_value<'rt>(
        &self,
        runtime: &'rt Runtime,
        at: usize,
    ) -> Result<Root<'rt>, EvalError> {
        if self.logical_host.is_some() && !logical::valid_components(self) {
            return Err(EvalError::from_error(at, PathnameError::LogicalSyntax)
                .with_condition(crate::condition::Class::TypeError));
        }
        let fields = runtime.root_frame(6);
        for field in [
            Field::Host,
            Field::Device,
            Field::Directory,
            Field::Name,
            Field::Type,
            Field::Version,
        ] {
            let component = self.component(runtime, field, false, at)?;
            fields.set(field.index(), component.slot.get());
        }
        let word = runtime
            .heap
            .pathname(|| std::array::from_fn(|index| fields.get(index)))
            .map_err(|error| EvalError::out_of_memory(at, error))?;
        Ok(runtime.root(word))
    }

    pub(crate) fn component<'rt>(
        &self,
        runtime: &'rt Runtime,
        field: Field,
        common_case: bool,
        at: usize,
    ) -> Result<Root<'rt>, EvalError> {
        let common_case = common_case && self.logical_host.is_none();
        match field {
            Field::Host => match &self.logical_host {
                Some(host) => text_component(runtime, host, false, at),
                None => unspecific(runtime, self.host_unspecific, at),
            },
            Field::Device => unspecific(runtime, self.device_unspecific, at),
            Field::Directory => match &self.directory {
                Some(directory) => directory.to_lisp_list(runtime, common_case, at),
                None => Ok(runtime.nil()),
            },
            Field::Name => self.name.to_lisp_value(runtime, common_case, at),
            Field::Type => self.file_type.to_lisp_value(runtime, common_case, at),
            Field::Version => match self.version {
                Version::Missing => Ok(runtime.nil()),
                Version::Unspecific => keyword(runtime, "UNSPECIFIC", at),
                Version::Newest => keyword(runtime, "NEWEST", at),
                Version::Wild => keyword(runtime, "WILD", at),
                Version::Number(number) => Ok(runtime.fixnum(number).expect("pathname version")),
            },
        }
    }
}

impl Leaf {
    fn or(&self, default: &Self) -> Self {
        if self == &Self::Missing {
            default.clone()
        } else {
            self.clone()
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Wild => Some("*"),
            Self::Missing | Self::Unspecific => None,
        }
    }

    fn to_lisp_value<'rt>(
        &self,
        runtime: &'rt Runtime,
        common_case: bool,
        at: usize,
    ) -> Result<Root<'rt>, EvalError> {
        match self {
            Self::Missing => Ok(runtime.nil()),
            Self::Unspecific => keyword(runtime, "UNSPECIFIC", at),
            Self::Wild => keyword(runtime, "WILD", at),
            Self::Text(text) => text_component(runtime, text, common_case, at),
        }
    }
}

impl Directory {
    fn to_lisp_list<'rt>(
        &self,
        runtime: &'rt Runtime,
        common_case: bool,
        at: usize,
    ) -> Result<Root<'rt>, EvalError> {
        let mut tail = runtime.nil();
        for part in self.parts.iter().rev() {
            let head = match part {
                DirectoryPart::Text(text) => text_component(runtime, text, common_case, at)?,
                DirectoryPart::Up => keyword(runtime, "UP", at)?,
                DirectoryPart::Back => keyword(runtime, "BACK", at)?,
                DirectoryPart::Wild => keyword(runtime, "WILD", at)?,
                DirectoryPart::WildInferiors => keyword(runtime, "WILD-INFERIORS", at)?,
            };
            tail = runtime
                .cons(&head, &tail)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
        }
        let head = keyword(
            runtime,
            if self.absolute {
                "ABSOLUTE"
            } else {
                "RELATIVE"
            },
            at,
        )?;
        runtime
            .cons(&head, &tail)
            .map_err(|error| EvalError::out_of_memory(at, error))
    }
}

fn resolve_back<'a>(parts: impl Iterator<Item = &'a DirectoryPart>) -> Vec<DirectoryPart> {
    let mut result = Vec::with_capacity(parts.size_hint().0);
    for part in parts {
        if matches!(part, DirectoryPart::Back)
            && matches!(
                result.last(),
                Some(DirectoryPart::Text(_) | DirectoryPart::Wild)
            )
        {
            result.pop();
        } else {
            result.push(part.clone());
        }
    }
    result
}

fn render_directory(directory: Option<&Directory>, escaped: bool) -> String {
    let Some(directory) = directory else {
        return String::new();
    };
    let mut result = String::new();
    if directory.absolute {
        result.push('/');
    } else if directory.parts.is_empty() {
        result.push_str("./");
    }
    for part in &directory.parts {
        match part {
            DirectoryPart::Text(text) if escaped => escape(&mut result, text, false, false),
            DirectoryPart::Text(text) => result.push_str(text),
            DirectoryPart::Up | DirectoryPart::Back => result.push_str(".."),
            DirectoryPart::Wild => result.push('*'),
            DirectoryPart::WildInferiors => result.push_str("**"),
        }
        result.push('/');
    }
    result
}

fn escape(result: &mut String, text: &str, dots: bool, leading_dot: bool) {
    let literal_glob = matches!(text, "*" | "**");
    for (index, ch) in text.chars().enumerate() {
        if matches!(ch, '\\' | ':')
            || (literal_glob && ch == '*' && index == 0)
            || (dots && ch == '.' && (index != 0 || leading_dot))
        {
            result.push('\\');
        }
        result.push(ch);
    }
}

fn unescape(text: &str) -> Result<String, PathnameError> {
    let mut chars = text.chars();
    let mut result = String::with_capacity(text.len());
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            result.push(chars.next().ok_or(PathnameError::IncompleteEscape)?);
        } else {
            result.push(ch);
        }
    }
    Ok(result)
}

fn last_unescaped_dot(text: &str) -> Result<Option<usize>, PathnameError> {
    let mut chars = text.char_indices();
    let mut index = None;
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '\\' => {
                chars.next().ok_or(PathnameError::IncompleteEscape)?;
            }
            '.' if offset != 0 => index = Some(offset),
            _ => {}
        }
    }
    Ok(index)
}

fn parse_leaf(text: &str, syntax: bool) -> Result<Leaf, PathnameError> {
    if syntax && text == "*" {
        Ok(Leaf::Wild)
    } else {
        Ok(Leaf::Text(if syntax {
            unescape(text)?
        } else {
            text.into()
        }))
    }
}

pub(crate) fn initial_defaults(runtime: &Runtime, at: usize) -> Result<Root<'_>, EvalError> {
    // Relative defaults still use the working directory when opening a file
    // if the embedding process has no UTF-8 working-directory name.
    std::env::current_dir()
        .ok()
        .and_then(|path| Pathname::from_directory_path(&path).ok())
        .unwrap_or_default()
        .to_lisp_value(runtime, at)
}

impl Runtime {
    /// Copy the current physical directory from `*DEFAULT-PATHNAME-DEFAULTS*`.
    /// Returns `None` for an unbound, logical, or otherwise nonlocal pathname.
    /// This operation runs no Lisp.
    pub fn local_pathname_defaults(&self) -> Option<PathBuf> {
        let symbol = self.find_common_lisp("*DEFAULT-PATHNAME-DEFAULTS*")?;
        let word = self.heap.symbol_cell(symbol, false)?;
        from_word(self, word)?.directory_path().ok()
    }

    /// Encode a host filesystem path as a physical Common Lisp namestring.
    pub fn physical_pathname_namestring(path: &Path) -> Option<String> {
        Pathname::from_path(path).ok()?.namestring()
    }
}

pub(crate) fn default_pathname(runtime: &Runtime, at: usize) -> Result<Pathname, Fault> {
    let symbol = runtime.intern_common_lisp("*DEFAULT-PATHNAME-DEFAULTS*", at)?;
    let word = runtime.heap.symbol_cell(symbol, false).ok_or_else(|| {
        Fault::cell(runtime, 
            crate::condition::Class::UnboundVariable,
            symbol,
            EvalError::new(at, "*DEFAULT-PATHNAME-DEFAULTS* is unbound"),
        )
    })?;
    from_word(runtime, word).ok_or_else(|| {
        Fault::type_error(runtime, 
            word,
            "PATHNAME",
            EvalError::new(at, "expected a pathname default"),
        )
    })
}

pub(crate) fn from_word(runtime: &Runtime, word: Word) -> Option<Pathname> {
    normalized_fields(runtime, runtime.heap.pathname_fields(word)?)
}

pub(crate) fn file_stream_pathname(runtime: &Runtime, word: Word) -> Option<Word> {
    let info = runtime.heap.stream_info(word)?;
    info.kind.file().then_some(info.contents)
}

pub(crate) fn normalized_fields(runtime: &Runtime, fields: [Word; 6]) -> Option<Pathname> {
    let [host, device, directory, name, file_type, version] = fields;
    let unspecific = |word| {
        (word == Word::NIL || stored_keyword(runtime, word).as_deref() == Some("UNSPECIFIC"))
            .then_some(word != Word::NIL)
    };
    if directory != Word::NIL && runtime.heap.cons_fields(directory).is_none() {
        return None;
    }
    let logical_host = runtime.heap.string_contents(host);
    let path = Pathname {
        host_unspecific: if logical_host.is_some() {
            false
        } else {
            unspecific(host)?
        },
        logical_host,
        device_unspecific: unspecific(device)?,
        directory: decode_directory_with(runtime, directory, false, stored_keyword)?,
        name: decode_leaf_with(runtime, name, false, stored_keyword)?,
        file_type: decode_leaf_with(runtime, file_type, false, stored_keyword)?,
        version: decode_version_with(runtime, version, stored_keyword)?,
    };
    (path.logical_host.is_none() || logical::valid_components(&path)).then_some(path)
}

pub(crate) fn designator(runtime: &Runtime, word: Word, at: usize) -> Result<Pathname, Fault> {
    if let Some(pathname) = from_word(runtime, word) {
        return Ok(pathname);
    }
    if let Some(text) = runtime.heap.string_contents(word) {
        let defaults = default_pathname(runtime, at)?;
        return namestring_designator(runtime, word, &text, &defaults, at);
    }
    Err(Fault::pathname_designator(runtime, 
        word,
        EvalError::new(at, "expected a pathname or namestring"),
    ))
}

pub(crate) fn merged_designator(
    runtime: &Runtime,
    word: Word,
    at: usize,
) -> Result<Pathname, Fault> {
    if let Some(text) = runtime.heap.string_contents(word) {
        let defaults = default_pathname(runtime, at)?;
        let pathname = namestring_designator(runtime, word, &text, &defaults, at)?;
        return Ok(pathname.merge(&defaults));
    }
    Ok(designator(runtime, word, at)?.merge(&default_pathname(runtime, at)?))
}

pub(crate) fn namestring_designator(
    runtime: &Runtime,
    word: Word,
    text: &str,
    defaults: &Pathname,
    at: usize,
) -> Result<Pathname, Fault> {
    parse_namestring(runtime, text, None, defaults.logical_host.as_deref())
        .map_err(|error| Fault::file_error(runtime, word, EvalError::from_error(at, error)))
}

pub(crate) fn parse_namestring(
    runtime: &Runtime,
    text: &str,
    requested_host: Option<&str>,
    default_host: Option<&str>,
) -> Result<Pathname, PathnameError> {
    logical::parse_namestring(runtime, text, requested_host, default_host)
}

pub(crate) fn logical_pathname(runtime: &Runtime, text: &str) -> Result<Pathname, PathnameError> {
    logical::coerce(runtime, text)
}

pub(crate) fn normalize_logical(path: &mut Pathname) -> Result<(), PathnameError> {
    logical::normalize(path)
}

pub(crate) fn translate(runtime: &Runtime, path: Pathname, at: usize) -> Result<Pathname, Fault> {
    if path.logical_host.is_none() {
        return Ok(path);
    }
    logical::translate(runtime, &path, at)
}

pub(crate) use logical::{pathname_match, translate_pathname, translation_host};

pub(crate) fn keyword_name(runtime: &Runtime, word: Word) -> Option<String> {
    if runtime.heap.symbol_home(word) == Some(Package::KEYWORD) {
        runtime.heap.symbol_name(word)
    } else {
        None
    }
}

pub(crate) fn decode_unspecific(runtime: &Runtime, word: Word) -> Option<bool> {
    if word == Word::NIL {
        Some(false)
    } else {
        (keyword_name(runtime, word).as_deref() == Some("UNSPECIFIC")).then_some(true)
    }
}

pub(crate) fn decode_leaf(runtime: &Runtime, word: Word, common: bool) -> Option<Leaf> {
    decode_leaf_with(runtime, word, common, keyword_name)
}

fn decode_leaf_with(
    runtime: &Runtime,
    word: Word,
    common: bool,
    keyword: fn(&Runtime, Word) -> Option<String>,
) -> Option<Leaf> {
    if word == Word::NIL {
        return Some(Leaf::Missing);
    }
    if let Some(text) = runtime.heap.string_contents(word) {
        return valid_segment(&text).then(|| Leaf::Text(case(text, common)));
    }
    match keyword(runtime, word).as_deref()? {
        "UNSPECIFIC" => Some(Leaf::Unspecific),
        "WILD" => Some(Leaf::Wild),
        _ => None,
    }
}

pub(crate) fn decode_directory(
    runtime: &Runtime,
    word: Word,
    common: bool,
) -> Option<Option<Directory>> {
    decode_directory_with(runtime, word, common, keyword_name)
}

fn decode_directory_with(
    runtime: &Runtime,
    word: Word,
    common: bool,
    keyword: fn(&Runtime, Word) -> Option<String>,
) -> Option<Option<Directory>> {
    if word == Word::NIL {
        return Some(None);
    }
    if let Some(text) = runtime.heap.string_contents(word) {
        if !valid_segment(&text) || text.is_empty() || matches!(text.as_str(), "." | "..") {
            return None;
        }
        return Some(Some(Directory {
            absolute: true,
            parts: vec![DirectoryPart::Text(case(text, common))],
        }));
    }
    if keyword(runtime, word).as_deref() == Some("WILD") {
        return Some(Some(Directory {
            absolute: true,
            parts: vec![DirectoryPart::WildInferiors],
        }));
    }
    let (head, tail) = runtime.heap.cons_fields(word)?;
    let absolute = match keyword(runtime, head).as_deref()? {
        "ABSOLUTE" => true,
        "RELATIVE" => false,
        _ => return None,
    };
    let mut spine = ListSpine::new(tail);
    let mut parts = Vec::new();
    while let Some((head, _)) = spine.next(&runtime.heap).ok()? {
        let part = if let Some(text) = runtime.heap.string_contents(head) {
            if !valid_segment(&text) || text.is_empty() || matches!(text.as_str(), "." | "..") {
                return None;
            }
            DirectoryPart::Text(case(text, common))
        } else {
            match keyword(runtime, head).as_deref()? {
                "UP" => DirectoryPart::Up,
                "BACK" => DirectoryPart::Back,
                "WILD" => DirectoryPart::Wild,
                "WILD-INFERIORS" => DirectoryPart::WildInferiors,
                _ => return None,
            }
        };
        parts.push(part);
    }
    (spine.tail() == Word::NIL).then_some(Some(Directory { absolute, parts }))
}

pub(crate) fn decode_version(runtime: &Runtime, word: Word) -> Option<Version> {
    decode_version_with(runtime, word, keyword_name)
}

fn decode_version_with(
    runtime: &Runtime,
    word: Word,
    keyword: fn(&Runtime, Word) -> Option<String>,
) -> Option<Version> {
    if word == Word::NIL {
        return Some(Version::Missing);
    }
    if let Some(number) = word.as_fixnum() {
        return (number > 0).then_some(Version::Number(number));
    }
    match keyword(runtime, word).as_deref()? {
        "UNSPECIFIC" => Some(Version::Unspecific),
        "NEWEST" => Some(Version::Newest),
        "WILD" => Some(Version::Wild),
        _ => None,
    }
}

fn stored_keyword(runtime: &Runtime, word: Word) -> Option<String> {
    // A stored marker keeps its pathname meaning if the user uninterns it.
    runtime.heap.symbol_name(word)
}

fn valid_segment(text: &str) -> bool {
    !text.contains(['/', '\0'])
}

fn case(mut text: String, common: bool) -> String {
    // The local convention is lowercase. Flip uniform ASCII casing, so
    // :COMMON conversion is reversible and mixed case remains exact.
    if common {
        let lower = text.bytes().any(|ch| ch.is_ascii_lowercase());
        let upper = text.bytes().any(|ch| ch.is_ascii_uppercase());
        match (lower, upper) {
            (true, false) => text.make_ascii_uppercase(),
            (false, true) => text.make_ascii_lowercase(),
            _ => {}
        }
    }
    text
}

fn text_component<'rt>(
    runtime: &'rt Runtime,
    text: &str,
    common: bool,
    at: usize,
) -> Result<Root<'rt>, EvalError> {
    let text = case(text.to_owned(), common);
    let word = runtime
        .heap
        .string(&text)
        .map_err(|error| EvalError::out_of_memory(at, error))?;
    Ok(runtime.root(word))
}

fn unspecific<'rt>(
    runtime: &'rt Runtime,
    specified: bool,
    at: usize,
) -> Result<Root<'rt>, EvalError> {
    if specified {
        keyword(runtime, "UNSPECIFIC", at)
    } else {
        Ok(runtime.nil())
    }
}

fn keyword<'rt>(runtime: &'rt Runtime, name: &str, at: usize) -> Result<Root<'rt>, EvalError> {
    let word = runtime.intern(&Name::keyword(name), at)?;
    Ok(runtime.root(word))
}
