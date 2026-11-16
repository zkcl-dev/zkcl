use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::condition::Fault;
use crate::heap::{ListSpine, Word};
use crate::{EvalError, Root, Runtime};

use super::{Directory, DirectoryPart, Leaf, Pathname, PathnameError, Version, from_word};

mod image;

#[derive(Default)]
pub(crate) struct LogicalHosts {
    rules: HashMap<String, Vec<Translation>>,
}

struct Translation {
    from: Pathname,
    to: Pathname,
}

fn word(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

fn wildcard_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'*'))
        && !text.contains("**")
}

fn canonical_host(text: &str) -> Option<String> {
    word(text).then(|| text.to_ascii_uppercase())
}

pub(crate) fn logical_host(runtime: &Runtime, text: &str) -> Option<String> {
    let host = canonical_host(text)?;
    runtime
        .logical_hosts
        .borrow()
        .rules
        .contains_key(&host)
        .then_some(host)
}

pub(crate) fn translation_host(runtime: &Runtime, value: Word, at: usize) -> Result<String, Fault> {
    runtime
        .heap
        .string_contents(value)
        .and_then(|host| canonical_host(&host))
        .ok_or_else(|| {
            Fault::type_error(runtime, 
                value,
                "STRING",
                EvalError::new(at, "expected a logical host name"),
            )
        })
}

pub(super) fn parse_namestring(
    runtime: &Runtime,
    text: &str,
    requested: Option<&str>,
    default: Option<&str>,
) -> Result<Pathname, PathnameError> {
    if let Some(host) = requested {
        return parse(text, host);
    }
    if let Some((prefix, _)) = text.split_once(':') {
        if let Some(host) = logical_host(runtime, prefix) {
            return parse(text, &host);
        }
    } else if let Some(host) = default
        && let Ok(path) = parse(text, host)
    {
        return Ok(path);
    }
    Pathname::parse(text)
}

pub(super) fn coerce(runtime: &Runtime, text: &str) -> Result<Pathname, PathnameError> {
    let (prefix, _) = text.split_once(':').ok_or(PathnameError::LogicalSyntax)?;
    let host =
        logical_host(runtime, prefix).ok_or_else(|| PathnameError::UnknownHost(prefix.into()))?;
    parse(text, &host)
}

fn parse(text: &str, host: &str) -> Result<Pathname, PathnameError> {
    let text = if let Some((prefix, rest)) = text.split_once(':') {
        if !host.eq_ignore_ascii_case(prefix) {
            return Err(PathnameError::HostMismatch);
        }
        rest
    } else {
        text
    };
    let (absolute, text) = match text.strip_prefix(';') {
        Some(relative) => (false, relative),
        None => (true, text),
    };
    let mut parts = text.split(';').peekable();
    let mut directories = Vec::new();
    let file = loop {
        let part = parts.next().expect("split always supplies a component");
        if parts.peek().is_none() {
            break part;
        }
        directories.push(match part {
            "*" => DirectoryPart::Wild,
            "**" => DirectoryPart::WildInferiors,
            text if wildcard_word(text) => DirectoryPart::Text(text.to_ascii_uppercase()),
            _ => return Err(PathnameError::LogicalSyntax),
        });
    };
    let mut pieces = file.split('.');
    let name = match pieces.next().unwrap() {
        "" => Leaf::Missing,
        name => leaf(name)?,
    };
    let file_type = pieces.next().map(leaf).transpose()?.unwrap_or_default();
    let version = match pieces.next() {
        None => Version::Missing,
        Some("*") => Version::Wild,
        Some(text) if text.eq_ignore_ascii_case("NEWEST") => Version::Newest,
        Some(text) if text.bytes().all(|b| b.is_ascii_digit()) => {
            let number: isize = text.parse().map_err(PathnameError::LogicalVersion)?;
            if number <= 0 || Word::fixnum(number).is_none() {
                return Err(PathnameError::LogicalSyntax);
            }
            Version::Number(number)
        }
        _ => return Err(PathnameError::LogicalSyntax),
    };
    if pieces.next().is_some() {
        return Err(PathnameError::LogicalSyntax);
    }
    Ok(Pathname {
        logical_host: Some(host.into()),
        device_unspecific: true,
        directory: Some(Directory {
            absolute,
            parts: directories,
        }),
        name,
        file_type,
        version,
        ..Pathname::default()
    })
}

fn leaf(text: &str) -> Result<Leaf, PathnameError> {
    match text {
        "*" => Ok(Leaf::Wild),
        text if wildcard_word(text) => Ok(Leaf::Text(text.to_ascii_uppercase())),
        _ => Err(PathnameError::LogicalSyntax),
    }
}

pub(super) fn normalize(path: &mut Pathname) -> Result<(), PathnameError> {
    if path.logical_host.is_none() {
        return Ok(());
    }
    path.device_unspecific = true;
    *path = path.with_case(true);
    for leaf in [&mut path.name, &mut path.file_type] {
        if matches!(leaf, Leaf::Text(text) if text == "*") {
            *leaf = Leaf::Wild;
        }
    }
    if let Some(directory) = &mut path.directory {
        for part in &mut directory.parts {
            if let DirectoryPart::Text(text) = part {
                match text.as_str() {
                    "*" => *part = DirectoryPart::Wild,
                    "**" => *part = DirectoryPart::WildInferiors,
                    _ => {}
                }
            }
        }
    }
    if valid_components(path) {
        Ok(())
    } else {
        Err(PathnameError::LogicalSyntax)
    }
}

pub(super) fn valid_components(path: &Pathname) -> bool {
    let uppercase = |text: &str| !text.bytes().any(|c| c.is_ascii_lowercase());
    path.logical_host
        .as_deref()
        .is_some_and(|host| word(host) && uppercase(host))
        && path.device_unspecific
        && !path.host_unspecific
        && path.version != Version::Unspecific
        && [&path.name, &path.file_type]
            .into_iter()
            .all(|part| match part {
                Leaf::Missing | Leaf::Wild => true,
                Leaf::Text(text) => wildcard_word(text) && uppercase(text),
                Leaf::Unspecific => false,
            })
        && path.directory.as_ref().is_none_or(|directory| {
            directory.parts.iter().all(|part| match part {
                DirectoryPart::Text(text) => wildcard_word(text) && uppercase(text),
                DirectoryPart::Wild | DirectoryPart::WildInferiors => true,
                _ => false,
            })
        })
}

pub(super) fn directory_namestring(directory: Option<&Directory>) -> String {
    let Some(directory) = directory else {
        return String::new();
    };
    let mut result = String::new();
    if !directory.absolute {
        result.push(';');
    }
    for part in &directory.parts {
        result.push_str(match part {
            DirectoryPart::Text(text) => text,
            DirectoryPart::Wild => "*",
            DirectoryPart::WildInferiors => "**",
            _ => unreachable!("logical directory component"),
        });
        result.push(';');
    }
    result
}

pub(super) fn file_namestring(path: &Pathname) -> Option<String> {
    let mut result = path.name.text().unwrap_or_default().to_owned();
    if let Some(file_type) = path.file_type.text() {
        result.push('.');
        result.push_str(file_type);
    } else {
        return matches!(path.version, Version::Missing | Version::Newest).then_some(result);
    }
    match path.version {
        Version::Missing => {}
        Version::Newest => result.push_str(".NEWEST"),
        Version::Wild => result.push_str(".*"),
        Version::Number(number) => {
            result.push('.');
            result.push_str(&number.to_string());
        }
        Version::Unspecific => return None,
    }
    Some(result)
}

pub(crate) fn hosts_equal(path: &Pathname, logical: Option<&str>, unspecific: bool) -> bool {
    path.logical_host.as_deref() == logical
        && (logical.is_some() || path.host_unspecific == unspecific)
}

pub(crate) fn set_translations(
    runtime: &Runtime,
    host: &str,
    rules: Word,
    at: usize,
) -> Result<(), Fault> {
    let invalid = || {
        Fault::type_error(runtime, 
            rules,
            "LIST",
            EvalError::new(
                at,
                "expected a proper list of logical pathname translations",
            ),
        )
    };
    let failure = |value, error: PathnameError| {
        Fault::file_error(runtime, value, EvalError::new(at, error.to_string()))
    };
    let mut parsed = Vec::new();
    let mut spine = ListSpine::new(rules);
    while let Some((rule, _)) = spine.next(&runtime.heap).map_err(|_| invalid())? {
        let (from, tail) = runtime.heap.cons_fields(rule).ok_or_else(invalid)?;
        let (to, Word::NIL) = runtime.heap.cons_fields(tail).ok_or_else(invalid)? else {
            return Err(invalid());
        };
        let from_path = if let Some(text) = runtime.heap.string_contents(from) {
            parse(&text, host).map_err(|error| failure(from, error))?
        } else {
            from_word(runtime, from)
                .filter(|path| path.logical_host.as_deref() == Some(host))
                .ok_or_else(|| failure(from, PathnameError::HostMismatch))?
        };
        let to_path = if let Some(text) = runtime.heap.string_contents(to)
            && text
                .split_once(':')
                .is_some_and(|(prefix, _)| host.eq_ignore_ascii_case(prefix))
        {
            parse(&text, host).map_err(|error| failure(to, error))?
        } else {
            super::designator(runtime, to, at)?
        };
        parsed.push(Translation {
            from: from_path,
            to: to_path,
        });
    }
    if spine.tail() != Word::NIL {
        return Err(invalid());
    }
    // Publish the host and all of its rules together after validating them.
    runtime
        .logical_hosts
        .borrow_mut()
        .rules
        .insert(host.into(), parsed);
    Ok(())
}

pub(crate) fn translations<'rt>(
    runtime: &'rt Runtime,
    host: &str,
    host_word: Word,
    at: usize,
) -> Result<Root<'rt>, Fault> {
    let registry = runtime.logical_hosts.borrow();
    let rules = registry.rules.get(host).ok_or_else(|| {
        Fault::type_error(runtime, 
            host_word,
            "STRING",
            EvalError::new(at, format!("no logical pathname host named {host}")),
        )
    })?;
    let cons = |head: &Root<'_>, tail: &Root<'_>| {
        runtime
            .cons(head, tail)
            .map_err(|error| EvalError::out_of_memory(at, error))
    };
    let mut result = runtime.nil();
    let nil = runtime.nil();
    for rule in rules.iter().rev() {
        let from = rule.from.to_lisp_value(runtime, at)?;
        let to = rule.to.to_lisp_value(runtime, at)?;
        let pair = cons(&from, &cons(&to, &nil)?)?;
        result = cons(&pair, &result)?;
    }
    Ok(result)
}

pub(super) fn translate(
    runtime: &Runtime,
    original: &Pathname,
    at: usize,
) -> Result<Pathname, Fault> {
    let registry = runtime.logical_hosts.borrow();
    let file_error = |error: PathnameError| match original.to_lisp_value(runtime, at) {
        Ok(pathname) => {
            Fault::file_error(runtime, pathname.slot.get(), EvalError::new(at, error.to_string()))
        }
        Err(error) => error.into(),
    };
    let mut path = original.clone();
    let mut visited = HashSet::new();
    while let Some(host) = &path.logical_host {
        // A rule can keep rewriting the pathname without ever repeating it.
        runtime.check_execution_safepoint(at)?;
        if visited.contains(&path) {
            return Err(file_error(PathnameError::TranslationCycle));
        }
        let translations = registry
            .rules
            .get(host)
            .ok_or_else(|| file_error(PathnameError::UnknownHost(host.clone())))?;
        let rule = translations
            .iter()
            .find(|rule| pathname_match(&path, &rule.from))
            .ok_or_else(|| file_error(PathnameError::NoTranslation))?;
        let next = translate_pathname(&path, &rule.from, &rule.to).map_err(file_error)?;
        visited.insert(path);
        path = next;
    }
    Ok(path)
}

pub(crate) fn pathname_match(path: &Pathname, pattern: &Pathname) -> bool {
    let logical = pattern.logical_host.is_some();
    path.logical_host == pattern.logical_host
        && matches_leaf(&path.name, &pattern.name, logical)
        && matches_leaf(&path.file_type, &pattern.file_type, logical)
        && (matches!(pattern.version, Version::Missing | Version::Wild)
            || path.version == pattern.version)
        && match_directory(path.directory.as_ref(), pattern.directory.as_ref(), logical).is_some()
}

fn matches_leaf(value: &Leaf, pattern: &Leaf, logical: bool) -> bool {
    match (value, pattern) {
        (_, Leaf::Missing | Leaf::Wild) => true,
        (Leaf::Text(value), Leaf::Text(pattern)) if logical && pattern.contains('*') => {
            captures(value, pattern).is_some()
        }
        _ => value == pattern,
    }
}

fn captures<'a>(value: &'a str, pattern: &str) -> Option<Vec<&'a str>> {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap();
    let Some(last) = parts.next_back() else {
        return (first == value).then(Vec::new);
    };
    let mut remaining = value.strip_prefix(first)?.strip_suffix(last)?;
    let mut captures = Vec::new();
    for part in parts {
        let index = remaining.find(part)?;
        captures.push(&remaining[..index]);
        remaining = &remaining[index + part.len()..];
    }
    captures.push(remaining);
    Some(captures)
}

struct DirectoryCapture<'a> {
    parts: &'a [DirectoryPart],
    pattern: Option<&'a DirectoryPart>,
}

fn match_directory<'a>(
    source: Option<&'a Directory>,
    pattern: Option<&'a Directory>,
    logical: bool,
) -> Option<Vec<DirectoryCapture<'a>>> {
    let parts = source.map_or(&[][..], |directory| directory.parts.as_slice());
    let Some(pattern) = pattern else {
        return Some(vec![DirectoryCapture {
            parts,
            pattern: None,
        }]);
    };
    if source.is_some_and(|directory| directory.absolute != pattern.absolute) {
        return None;
    }
    let mut work = vec![(0, 0)];
    let mut previous = HashMap::from([((0, 0), (0, 0))]);
    while let Some(position @ (start, step)) = work.pop() {
        let Some(pat) = pattern.parts.get(step) else {
            if start == parts.len() {
                let mut ends = vec![0; pattern.parts.len() + 1];
                let mut position = position;
                while position != (0, 0) {
                    let parent = previous[&position];
                    if position.1 != parent.1 {
                        ends[position.1] = position.0;
                    }
                    position = parent;
                }
                return Some(
                    pattern
                        .parts
                        .iter()
                        .enumerate()
                        .filter_map(|(index, part)| {
                            let wildcard = match part {
                                DirectoryPart::Wild | DirectoryPart::WildInferiors => true,
                                DirectoryPart::Text(text) => logical && text.contains('*'),
                                _ => false,
                            };
                            wildcard.then(|| DirectoryCapture {
                                parts: &parts[ends[index]..ends[index + 1]],
                                pattern: Some(part),
                            })
                        })
                        .collect(),
                );
            }
            continue;
        };
        let mut advance = |next| {
            if let std::collections::hash_map::Entry::Vacant(entry) = previous.entry(next) {
                entry.insert(position);
                work.push(next);
            }
        };
        if *pat == DirectoryPart::WildInferiors {
            if start < parts.len() {
                advance((start + 1, step));
            }
            // Try ending this capture before consuming another directory.
            advance((start, step + 1));
        } else if let Some(value) = parts.get(start) {
            match pat {
                DirectoryPart::Wild if *value != DirectoryPart::WildInferiors => {}
                DirectoryPart::Text(pattern) if logical && pattern.contains('*') => {
                    let DirectoryPart::Text(text) = value else {
                        continue;
                    };
                    if self::captures(text, pattern).is_none() {
                        continue;
                    }
                }
                _ if pat == value => {}
                _ => continue,
            }
            advance((start + 1, step + 1));
        }
    }
    None
}

pub(crate) fn translate_pathname(
    source: &Pathname,
    from: &Pathname,
    to: &Pathname,
) -> Result<Pathname, PathnameError> {
    if !pathname_match(source, from) {
        return Err(PathnameError::NoTranslation);
    }
    let logical = to.logical_host.is_some();
    let change_case = logical != source.logical_host.is_some();
    let from_logical = from.logical_host.is_some();
    let to_physical = from_logical && !logical;
    let source = if change_case {
        Cow::Owned(source.with_case(logical))
    } else {
        Cow::Borrowed(source)
    };
    let from = if change_case {
        Cow::Owned(from.with_case(logical))
    } else {
        Cow::Borrowed(from)
    };
    let mut directory = match_directory(
        source.directory.as_ref(),
        from.directory.as_ref(),
        from_logical,
    )
    .ok_or(PathnameError::NoTranslation)?
    .into_iter();
    let mut result = to.clone();
    if let Some(to_directory) = &mut result.directory {
        let parts = std::mem::take(&mut to_directory.parts);
        for part in parts {
            match part {
                DirectoryPart::Wild | DirectoryPart::WildInferiors => {
                    let capture = directory.next().ok_or(PathnameError::NoTranslation)?;
                    if part == DirectoryPart::Wild && capture.parts.len() != 1 {
                        return Err(PathnameError::NoTranslation);
                    }
                    if to_physical {
                        check_physical_directory_parts(capture.parts)?;
                    }
                    to_directory.parts.extend_from_slice(capture.parts);
                }
                DirectoryPart::Text(pattern) if logical && pattern.contains('*') => {
                    let capture = directory.next().ok_or(PathnameError::NoTranslation)?;
                    let [DirectoryPart::Text(text)] = capture.parts else {
                        return Err(PathnameError::NoTranslation);
                    };
                    let from_pattern = match capture.pattern {
                        Some(DirectoryPart::Text(pattern)) if from_logical => {
                            Some(pattern.as_str())
                        }
                        _ => None,
                    };
                    let mapped = translate_word(text, from_pattern, &pattern)?;
                    to_directory.parts.push(DirectoryPart::Text(mapped));
                }
                _ => to_directory.parts.push(part),
            }
        }
    } else if let Some(directory) = &source.directory {
        if to_physical {
            check_physical_directory_parts(&directory.parts)?;
        }
        result.directory = Some(directory.clone());
    }
    result.name = translate_leaf(&source.name, &from.name, &to.name, from_logical, logical)?;
    result.file_type = translate_leaf(
        &source.file_type,
        &from.file_type,
        &to.file_type,
        from_logical,
        logical,
    )?;
    if matches!(result.version, Version::Missing | Version::Wild) {
        result.version = if to_physical {
            Version::Newest
        } else {
            source.version
        };
    }
    normalize(&mut result)?;
    Ok(result)
}

fn check_physical_directory_parts(parts: &[DirectoryPart]) -> Result<(), PathnameError> {
    if parts
        .iter()
        .any(|part| matches!(part, DirectoryPart::Text(text) if text.contains('*')))
    {
        return Err(PathnameError::Unrepresentable);
    }
    Ok(())
}

fn translate_leaf(
    source: &Leaf,
    from: &Leaf,
    to: &Leaf,
    from_logical: bool,
    to_logical: bool,
) -> Result<Leaf, PathnameError> {
    match to {
        Leaf::Missing | Leaf::Wild => {
            if from_logical
                && !to_logical
                && matches!(source, Leaf::Text(text) if text.contains('*'))
            {
                return Err(PathnameError::Unrepresentable);
            }
            Ok(source.clone())
        }
        Leaf::Text(pattern) if to_logical && pattern.contains('*') => {
            let Leaf::Text(text) = source else {
                return Err(PathnameError::NoTranslation);
            };
            let from_pattern = match from {
                Leaf::Text(from) if from_logical && from.contains('*') => Some(from.as_str()),
                _ => None,
            };
            translate_word(text, from_pattern, pattern).map(Leaf::Text)
        }
        _ => Ok(to.clone()),
    }
}

fn translate_word(text: &str, from: Option<&str>, to: &str) -> Result<String, PathnameError> {
    match from {
        Some(pattern) => substitute(
            to,
            &captures(text, pattern).ok_or(PathnameError::NoTranslation)?,
        ),
        None => substitute(to, &[text]),
    }
}

fn substitute(pattern: &str, captures: &[&str]) -> Result<String, PathnameError> {
    let mut pieces = pattern.split('*');
    let mut result = String::from(pieces.next().unwrap());
    let mut captures = captures.iter();
    for piece in pieces {
        result.push_str(captures.next().ok_or(PathnameError::NoTranslation)?);
        result.push_str(piece);
    }
    Ok(result)
}
