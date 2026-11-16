//! Workspace membership and dependency maintenance commands.

use std::collections::BTreeSet;
use std::env;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use zkcl_project::manifest::{self, DependencySource, MANIFEST_NAME, ManifestFile};
use zkcl_syntax::syntax::{SyntaxKind, SyntaxNode, parse_source};

use super::workspace::Workspace;
use zkcl::project::resolved::write_metadata;

pub(super) fn command(
    request: super::super::args::WorkCommand,
) -> Result<ExitCode, Box<dyn Error>> {
    use super::super::args::WorkCommand;
    if let WorkCommand::Remove(request) = request {
        return remove_members(request);
    }
    let (operation, manifest, paths, json) = match request {
        WorkCommand::Init(request) => ("init", request.manifest_path, request.paths, false),
        WorkCommand::Use(request) => ("use", request.manifest_path, request.paths, false),
        WorkCommand::Sync(request) => ("sync", request.manifest_path, Vec::new(), false),
        WorkCommand::List(request) => (
            "list",
            request.selection.manifest_path,
            Vec::new(),
            request.json,
        ),
        WorkCommand::Remove(_) => unreachable!("remove handled before dispatch"),
    };
    if operation == "init" {
        let path = manifest.unwrap_or(env::current_dir()?.join(MANIFEST_NAME));
        let path = if path.is_dir() {
            path.join(MANIFEST_NAME)
        } else {
            path
        };
        let root = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize()?;
        let members = normalize_members(&root, &paths)?;
        let source = format!(
            "(workspace\n  (members {}))\n",
            members
                .iter()
                .map(|p| quote(p))
                .collect::<Vec<_>>()
                .join(" ")
        );
        manifest::parse_manifest(&source)?;
        let mut output = tempfile::NamedTempFile::new_in(&root)?;
        output.write_all(source.as_bytes())?;
        output.as_file().sync_all()?;
        output.persist_noclobber(&path)?;
        println!("Created workspace {}", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    let workspace = match manifest {
        Some(path) => Workspace::open(&path)?,
        None => Workspace::discover(&env::current_dir()?)?
            .ok_or("no workspace found; run zkcl work init PATH ...")?,
    };
    // Reopen after acquiring the guard: discovery above may have observed the
    // manifest before another writer committed its membership/version changes.
    let metadata_guard = if matches!(operation, "use" | "sync") {
        Some(manifest_lock(&workspace.root)?)
    } else {
        None
    };
    let workspace = if metadata_guard.is_some() {
        Workspace::open(&workspace.manifest_path)?
    } else {
        workspace
    };
    match operation {
        "use" => {
            if paths.is_empty() {
                return Err("work use requires at least one project path".into());
            }
            let mut members: Vec<_> = workspace
                .members
                .iter()
                .map(|p| p.manifest_path.clone())
                .collect();
            let cwd = env::current_dir()?;
            members.extend(paths.into_iter().map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                }
            }));
            let members = normalize_members(&workspace.root, &members)?;
            let source = fs::read_to_string(&workspace.manifest_path)?;
            let root = root_node(&source);
            let member_node = root
                .children()
                .find(|node| head(node) == "members")
                .expect("validated workspace members");
            let closing = usize::from(member_node.text_range().end()) - 1;
            let additions = &members[workspace.members.len()..];
            if !additions.is_empty() {
                let mut edited = source.clone();
                edited.insert_str(
                    closing,
                    &format!(
                        " {}",
                        additions
                            .iter()
                            .map(|p| quote(p))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ),
                );
                manifest::parse_manifest(&edited)?;
                write_metadata(&workspace.manifest_path, &edited)?;
            }
            println!("Workspace contains {} modules", members.len());
        }
        "sync" => {
            if !paths.is_empty() {
                return Err("work sync does not accept paths".into());
            }
            let mut updates = Vec::new();
            for member in &workspace.members {
                let source = fs::read_to_string(&member.manifest_path)?;
                let edited = sync_source(&source, &workspace)?;
                if edited != source {
                    updates.push((member.manifest_path.clone(), edited));
                }
            }
            for (path, source) in &updates {
                write_metadata(path, source)?;
            }
            println!("Synchronized {} manifests", updates.len());
        }
        "list" => {
            if !paths.is_empty() {
                return Err("work list does not accept paths".into());
            }
            if json {
                let modules: Vec<_> = workspace.members.iter().map(|member| serde_json::json!({
                    "module": member.manifest.name.as_str(), "version": member.manifest.version.to_string(),
                    "path": member.root.strip_prefix(&workspace.root).expect("workspace member inside root")
                })).collect();
                println!("{}", serde_json::to_string_pretty(&modules)?);
            } else {
                for member in &workspace.members {
                    println!(
                        "{} {} {}",
                        member.manifest.name,
                        member.manifest.version,
                        member.root.strip_prefix(&workspace.root)?.display()
                    );
                }
            }
        }
        _ => unreachable!(),
    }
    Ok(ExitCode::SUCCESS)
}

fn remove_members(request: super::super::args::WorkMembers) -> Result<ExitCode, Box<dyn Error>> {
    let workspace = match request.manifest_path {
        Some(path) => Workspace::open(&path)?,
        None => Workspace::discover(&env::current_dir()?)?
            .ok_or("no workspace found; run zkcl work init PATH ...")?,
    };
    let _lock = manifest_lock(&workspace.root)?;
    let workspace = Workspace::open(&workspace.manifest_path)?;
    let mut removals = BTreeSet::new();
    for requested in &request.members {
        let index = workspace
            .members
            .iter()
            .position(|member| {
                member.manifest.name.as_str() == requested
                    || member.root == Path::new(requested)
                    || member.root.strip_prefix(&workspace.root).ok() == Some(Path::new(requested))
                    || member.manifest_path.strip_prefix(&workspace.root).ok()
                        == Some(Path::new(requested))
            })
            .ok_or_else(|| format!("unknown workspace member {requested:?}"))?;
        removals.insert(index);
    }
    if removals.len() == workspace.members.len() {
        return Err("a workspace needs at least one member".into());
    }
    let mut source = fs::read_to_string(&workspace.manifest_path)?;
    let members = root_node(&source)
        .children()
        .find(|node| head(node) == "members")
        .expect("validated workspace members");
    let ranges: Vec<_> = members
        .children_with_tokens()
        .filter_map(|element| element.into_token())
        .filter(|token| token.kind() == SyntaxKind::String)
        .map(|token| token.text_range())
        .collect();
    for index in removals.iter().rev() {
        let range = ranges[*index];
        source.replace_range(usize::from(range.start())..usize::from(range.end()), "");
    }
    manifest::parse_manifest(&source)?;
    write_metadata(&workspace.manifest_path, &source)?;
    zkcl::project::resolved::remove_locked_projects(
        &workspace.root,
        removals
            .iter()
            .map(|index| &workspace.members[*index].manifest.name),
    )?;
    Ok(ExitCode::SUCCESS)
}

/// Serialize manifest edits across workspace membership and project dependency commands.
/// The returned file releases the OS lock when dropped.
pub(super) fn manifest_lock(root: &Path) -> Result<File, Box<dyn Error>> {
    let directory = root.join("target/zkcl");
    fs::create_dir_all(&directory)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("manifest.lock"))?;
    file.lock()?;
    Ok(file)
}

fn normalize_members(root: &Path, paths: &[PathBuf]) -> Result<Vec<String>, Box<dyn Error>> {
    let mut roots = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut members = Vec::new();
    for path in paths {
        let path = path.canonicalize()?;
        let manifest_path = if path.is_dir() {
            path.join(MANIFEST_NAME)
        } else {
            path
        }
        .canonicalize()?;
        let directory = manifest_path
            .parent()
            .ok_or("member manifest has no parent")?;
        if directory == root || !directory.starts_with(root) {
            return Err("workspace members must be inside the workspace root".into());
        }
        let ManifestFile::Project(project) =
            manifest::parse_manifest(&fs::read_to_string(&manifest_path)?)?
        else {
            return Err("workspace members must be project manifests".into());
        };
        if !roots.insert(directory.to_owned()) {
            return Err(format!("duplicate workspace member {}", directory.display()).into());
        }
        if !names.insert(project.name.clone()) {
            return Err(format!("duplicate workspace module {}", project.name).into());
        }
        let member_path = if manifest_path
            .file_name()
            .is_some_and(|name| name == MANIFEST_NAME)
        {
            directory
        } else {
            &manifest_path
        };
        members.push(
            member_path
                .strip_prefix(root)?
                .to_str()
                .ok_or("workspace member path must be UTF-8")?
                .replace('\\', "/"),
        );
    }
    Ok(members)
}

fn sync_source(source: &str, workspace: &Workspace) -> Result<String, Box<dyn Error>> {
    let ManifestFile::Project(project) = manifest::parse_manifest(source)? else {
        return Err("expected project manifest".into());
    };
    let root = root_node(source);
    let declarations = root.children().filter(|node| head(node) == "dependency");
    let mut edits = Vec::new();
    for (dependency, node) in project.dependencies.iter().zip(declarations) {
        if !matches!(
            dependency.source,
            DependencySource::Registry { registry: None }
        ) {
            continue;
        }
        let Some(member) = workspace
            .members
            .iter()
            .find(|member| member.manifest.name == dependency.module)
        else {
            continue;
        };
        let Some(version) = node.children().find(|node| head(node) == "version") else {
            continue;
        };
        let value = version
            .children_with_tokens()
            .filter_map(|element| element.into_token())
            .find(|token| token.kind() == SyntaxKind::String)
            .expect("validated version string");
        let range = value.text_range();
        edits.push((
            usize::from(range.start())..usize::from(range.end()),
            quote(&member.manifest.version.to_string()),
        ));
    }
    let mut edited = source.to_owned();
    for (range, replacement) in edits.into_iter().rev() {
        edited.replace_range(range, &replacement);
    }
    manifest::parse_manifest(&edited)?;
    Ok(edited)
}

fn root_node(source: &str) -> SyntaxNode {
    parse_source(source)
        .root()
        .children()
        .find(|node| node.kind() == SyntaxKind::List)
        .expect("validated manifest declaration")
}

fn head(node: &SyntaxNode) -> String {
    node.children_with_tokens()
        .filter_map(|element| element.into_token())
        .find(|token| token.kind() == SyntaxKind::Atom)
        .map(|token| token.text().to_ascii_lowercase())
        .unwrap_or_default()
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}
