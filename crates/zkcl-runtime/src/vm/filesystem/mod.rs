use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, ReadDir};
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::{Primitive, named_keyword_arguments};
use crate::condition::{Class, Fault};
use crate::heap::{RootSlots, Word};
use crate::pathname::{self, Directory, DirectoryPart, Leaf, Pathname, PathnameError, Version};
use crate::{EvalError, Runtime};

#[cfg(not(target_family = "wasm"))]
mod async_read;
#[cfg(target_family = "wasm")]
mod async_read {
    use crate::condition::{Class, Fault};
    use crate::heap::RootSlots;
    use crate::host::HostOperation;
    use crate::{EvalError, Runtime};

    pub(in crate::vm) fn read_octets(
        _: &Runtime,
        _: &RootSlots,
        at: usize,
    ) -> Result<HostOperation, Fault> {
        Err(Fault::plain(
            Class::Error,
            EvalError::new(at, "asynchronous file reading requires a native worker"),
        ))
    }
}
pub(super) use async_read::read_octets;

pub(super) fn run(
    runtime: &Runtime,
    frame: &RootSlots,
    op: Primitive,
    args: Range<usize>,
    at: usize,
) -> Result<Word, Fault> {
    if matches!(op, Primitive::Directory) {
        named_keyword_arguments(runtime, frame, args.start + 1..args.end, [], at)?;
    }

    let original = file_designator(runtime, frame.get(args.start), at)?;
    let fault = |error| file_fault(runtime, &original, at, error);
    if !matches!(op, Primitive::Directory) && original.is_wild() {
        return Err(fault(EvalError::from_error(at, PathnameError::Wild)));
    }
    let pathname = pathname::translate(runtime, original.clone(), at)?;
    if matches!(op, Primitive::Directory) {
        let matches = directory(&pathname, at).map_err(fault)?;
        let mut result = runtime.nil();
        for (path, is_directory) in matches.into_iter().rev() {
            let matched = physical_pathname(&path, is_directory, at).map_err(fault)?;
            let matched = matched.to_lisp_value(runtime, at)?;
            result = runtime
                .cons(&matched, &result)
                .map_err(|error| EvalError::out_of_memory(at, error))?;
        }
        return Ok(result.slot.get());
    }

    let path = concrete_path(&pathname, at).map_err(fault)?;
    if matches!(op, Primitive::FileWriteDate | Primitive::FileAuthor) {
        let metadata =
            fs::metadata(&path).map_err(|error| fault(EvalError::file_io(at, &path, error)))?;
        if matches!(op, Primitive::FileWriteDate) {
            return file_write_date(&metadata, at);
        }
        let Some(author) = crate::platform::file_owner_name(&metadata) else {
            return Ok(Word::NIL);
        };
        return Ok(runtime
            .heap
            .string(&author)
            .map_err(|error| EvalError::out_of_memory(at, error))?);
    }
    if matches!(op, Primitive::DeleteFile) {
        // Do not resolve a symlink before unlinking the specified filename.
        fs::remove_file(&path).map_err(|error| fault(EvalError::file_io(at, &path, error)))?;
        return Ok(Word::T);
    }
    let (canonical, is_directory) = match canonical_entry(&path) {
        Ok(entry) => entry,
        Err(error) if matches!(op, Primitive::ProbeFile) && absent(&error) => return Ok(Word::NIL),
        Err(error) => return Err(fault(EvalError::file_io(at, &path, error))),
    };
    let matched = physical_pathname(&canonical, is_directory, at).map_err(fault)?;
    Ok(matched.to_lisp_value(runtime, at)?.slot.get())
}

fn file_write_date(metadata: &fs::Metadata, at: usize) -> Result<Word, Fault> {
    let Ok(written) = metadata.modified().or_else(|_| metadata.created()) else {
        return Ok(Word::NIL);
    };
    let seconds = match written.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::from(duration.as_secs()),
        Err(error) => {
            let duration = error.duration();
            -i128::from(duration.as_secs()) - i128::from(duration.subsec_nanos() != 0)
        }
    } + 2_208_988_800;
    if seconds < 0 {
        return Ok(Word::NIL);
    }
    isize::try_from(seconds)
        .ok()
        .and_then(Word::fixnum)
        .ok_or_else(|| {
            Fault::plain(
                Class::ArithmeticError,
                EvalError::new(at, "file write date exceeds the supported fixnum range"),
            )
        })
}

pub(super) fn rename(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<[Word; 3], Fault> {
    let old = file_designator(runtime, frame.get(args.start), at)?;
    let new = rename_destination(runtime, frame.get(args.start + 1), &old, at)?;
    let old_path = rename_path(runtime, &old, at)?;
    let new_path = rename_path(runtime, &new, at)?;
    let new_name = new.to_lisp_value(runtime, at)?;
    let old_truename = canonical_pathname(&old_path, at)
        .map_err(|error| file_fault(runtime, &old, at, error))?
        .to_lisp_value(runtime, at)?;
    fs::rename(&old_path, &new_path)
        .map_err(|error| file_fault(runtime, &new, at, EvalError::file_io(at, &new_path, error)))?;
    let new_truename = canonical_pathname(&new_path, at)
        .map_err(|error| file_fault(runtime, &new, at, error))?
        .to_lisp_value(runtime, at)?;
    Ok([new_name, old_truename, new_truename].map(|value| value.slot.get()))
}

fn rename_destination(
    runtime: &Runtime,
    word: Word,
    old: &Pathname,
    at: usize,
) -> Result<Pathname, Fault> {
    let new = if let Some(text) = runtime.heap.string_contents(word) {
        let mut new = pathname::namestring_designator(runtime, word, &text, old, at)?;
        if new.logical_host.is_some() && !text.contains([':', ';']) {
            // A bare new name keeps the old file's directory, including on logical hosts.
            new.directory = None;
        }
        new
    } else {
        pathname::from_word(runtime, word).ok_or_else(|| {
            Fault::type_error(runtime, 
                word,
                "PATHNAME",
                EvalError::new(at, "expected a pathname or namestring"),
            )
        })?
    };
    Ok(new.merge(old))
}

fn rename_path(runtime: &Runtime, pathname: &Pathname, at: usize) -> Result<PathBuf, Fault> {
    let fault = |error| file_fault(runtime, pathname, at, error);
    if pathname.is_wild() {
        return Err(fault(EvalError::from_error(at, PathnameError::Wild)));
    }
    concrete_path(&pathname::translate(runtime, pathname.clone(), at)?, at).map_err(fault)
}

pub(super) fn ensure_directories(
    runtime: &Runtime,
    frame: &RootSlots,
    args: Range<usize>,
    at: usize,
) -> Result<(Word, bool), Fault> {
    let original = file_designator(runtime, frame.get(args.start), at)?;
    let fault = |error| file_fault(runtime, &original, at, error);
    if original.wild_directory() {
        return Err(fault(EvalError::from_error(at, PathnameError::Wild)));
    }
    let pathname = pathname::translate(runtime, original.clone(), at)?;
    let directory = local_path(pathname.directory_path(), at).map_err(fault)?;
    let created = create_directories(&directory, at).map_err(fault)?;
    Ok((frame.get(args.start), created))
}

fn file_designator(runtime: &Runtime, word: Word, at: usize) -> Result<Pathname, Fault> {
    if let Some(pathname) = pathname::file_stream_pathname(runtime, word) {
        // OPEN has already applied defaults while establishing this pathname.
        return pathname::designator(runtime, pathname, at);
    }
    pathname::merged_designator(runtime, word, at)
}

fn file_fault(runtime: &Runtime, pathname: &Pathname, at: usize, error: EvalError) -> Fault {
    match pathname.to_lisp_value(runtime, at) {
        Ok(pathname) => Fault::file_error(runtime, pathname.slot.get(), error),
        Err(allocation_error) => allocation_error.into(),
    }
}

fn concrete_path(pathname: &Pathname, at: usize) -> Result<PathBuf, EvalError> {
    local_path(pathname.file_path(), at)
}

fn local_path(path: Result<PathBuf, PathnameError>, at: usize) -> Result<PathBuf, EvalError> {
    let path = path.map_err(|error| EvalError::from_error(at, error))?;
    Ok(if path.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        path
    })
}

fn physical_pathname(path: &Path, is_directory: bool, at: usize) -> Result<Pathname, EvalError> {
    let pathname = if is_directory {
        Pathname::from_directory_path(path)
    } else {
        Pathname::from_path(path)
    };
    pathname.map_err(|error| EvalError::from_error(at, error))
}

fn canonical_entry(path: &Path) -> io::Result<(PathBuf, bool)> {
    let canonical = fs::canonicalize(path)?;
    let is_directory = fs::metadata(&canonical)?.is_dir();
    Ok((canonical, is_directory))
}

fn canonical_pathname(path: &Path, at: usize) -> Result<Pathname, EvalError> {
    let (canonical, is_directory) =
        canonical_entry(path).map_err(|error| EvalError::file_io(at, path, error))?;
    physical_pathname(&canonical, is_directory, at)
}

fn create_directories(directory: &Path, at: usize) -> Result<bool, EvalError> {
    let mut missing = Vec::new();
    for path in directory
        .ancestors()
        .take_while(|path| !path.as_os_str().is_empty())
    {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(EvalError::file_io(
                    at,
                    path,
                    io::ErrorKind::NotADirectory.into(),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(path),
            Err(error) => return Err(EvalError::file_io(at, path, error)),
        }
    }
    let mut created = false;
    for path in missing.into_iter().rev() {
        match fs::create_dir(path) {
            Ok(()) => created = true,
            // Another caller can create this directory after the metadata check.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(error) => return Err(EvalError::file_io(at, path, error)),
        }
    }
    Ok(created)
}

fn absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

fn read_dir(path: &Path, at: usize) -> Result<Option<ReadDir>, EvalError> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(error) if absent(&error) => Ok(None),
        Err(error) => Err(EvalError::file_io(at, path, error)),
    }
}

fn directory(pathname: &Pathname, at: usize) -> Result<BTreeMap<PathBuf, bool>, EvalError> {
    let directory = pathname
        .local_directory()
        .map_err(|error| EvalError::from_error(at, error))?;
    let parts = directory.as_deref().map_or(&[][..], |dir| &dir.parts);
    let directory_wild = parts
        .iter()
        .any(|part| matches!(part, DirectoryPart::Wild | DirectoryPart::WildInferiors));
    let leaf_wild = matches!(pathname.name, Leaf::Wild) || matches!(pathname.file_type, Leaf::Wild);
    let mut results = BTreeMap::new();
    let mut prefix = pathname.clone();
    // Unix files have no numbered versions to filter. :WILD admits the one version.
    if prefix.version == Version::Wild {
        prefix.version = Version::Newest;
    }
    if !directory_wild && !leaf_wild {
        if let Some(Cow::Owned(directory)) = directory {
            prefix.directory = Some(directory);
        }
        let path = concrete_path(&prefix, at)?;
        let directories_only = missing(&pathname.name) && missing(&pathname.file_type);
        insert_canonical(&mut results, &path, directories_only, at)?;
        return Ok(results);
    }

    if let Some(directory) = &directory {
        prefix.directory = Some(Directory {
            absolute: directory.absolute,
            parts: Vec::new(),
        });
    }
    prefix.name = Leaf::Missing;
    prefix.file_type = Leaf::Missing;
    let start = concrete_path(&prefix, at)?;
    let mut pending = vec![(start, 0)];
    let mut visited = HashSet::new();

    while let Some((path, index)) = pending.pop() {
        match parts.get(index) {
            Some(DirectoryPart::Text(part)) => pending.push((path.join(part), index + 1)),
            Some(DirectoryPart::Up | DirectoryPart::Back) => {
                pending.push((path.join(".."), index + 1));
            }
            Some(part @ (DirectoryPart::Wild | DirectoryPart::WildInferiors)) => {
                let recursive = matches!(part, DirectoryPart::WildInferiors);
                let path = if recursive {
                    let path = match fs::canonicalize(&path) {
                        Ok(path) => path,
                        Err(error) if absent(&error) => continue,
                        Err(error) => return Err(EvalError::file_io(at, &path, error)),
                    };
                    if !visited.insert((path.clone(), index)) {
                        continue;
                    }
                    pending.push((path.clone(), index + 1));
                    path
                } else {
                    path
                };
                for entry in read_dir(&path, at)?.into_iter().flatten() {
                    let entry = entry.map_err(|error| EvalError::file_io(at, &path, error))?;
                    let child = entry.path();
                    match fs::metadata(&child) {
                        Ok(metadata) if metadata.is_dir() => {
                            pending.push((child, index + usize::from(!recursive)));
                        }
                        Ok(_) => {}
                        Err(error) if absent(&error) => {}
                        Err(error) => return Err(EvalError::file_io(at, &child, error)),
                    }
                }
            }
            None => directory_leaves(&mut results, &path, pathname, at)?,
        }
    }
    Ok(results)
}

fn directory_leaves(
    results: &mut BTreeMap<PathBuf, bool>,
    path: &Path,
    pattern: &Pathname,
    at: usize,
) -> Result<(), EvalError> {
    if missing(&pattern.name) && missing(&pattern.file_type) {
        return insert_canonical(results, path, true, at);
    }
    for entry in read_dir(path, at)?.into_iter().flatten() {
        let entry = entry.map_err(|error| EvalError::file_io(at, path, error))?;
        let name = entry.file_name();
        let candidate = physical_pathname(Path::new(&name), false, at)?;
        if matches_leaf(&pattern.name, &candidate.name)
            && matches_leaf(&pattern.file_type, &candidate.file_type)
        {
            insert_canonical(results, &entry.path(), false, at)?;
        }
    }
    Ok(())
}

fn insert_canonical(
    results: &mut BTreeMap<PathBuf, bool>,
    path: &Path,
    directories_only: bool,
    at: usize,
) -> Result<(), EvalError> {
    match canonical_entry(path) {
        Ok((canonical, is_directory)) if !directories_only || is_directory => {
            results.insert(canonical, is_directory);
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) if absent(&error) => Ok(()),
        Err(error) => Err(EvalError::file_io(at, path, error)),
    }
}

fn missing(leaf: &Leaf) -> bool {
    matches!(leaf, Leaf::Missing | Leaf::Unspecific)
}

fn matches_leaf(pattern: &Leaf, candidate: &Leaf) -> bool {
    match (pattern, candidate) {
        (Leaf::Wild, _) => true,
        (Leaf::Text(pattern), Leaf::Text(candidate)) => pattern == candidate,
        _ => missing(pattern) && missing(candidate),
    }
}
