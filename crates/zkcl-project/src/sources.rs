//! Offline source membership derived only from declarative project manifests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::manifest::{
    BuildTargetKind, DependencySource, MANIFEST_NAME, ManifestFile, ModuleName, parse_manifest,
};

/// Declared source inputs, in dependency-first declaration order.
/// Dynamic ASDF components and unselected conditional dependencies are unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSources {
    pub manifest: PathBuf,
    pub manifests: Vec<PathBuf>,
    pub files: Vec<PathBuf>,
    pub project_files: Vec<PathBuf>,
}

impl ProjectSources {
    /// Find a project or workspace declaration which actually owns `source`.
    /// Workspace membership includes every declared member's source targets.
    pub fn discover(source: &Path) -> Option<Self> {
        let source = source.canonicalize().ok()?;
        for directory in source.parent()?.ancestors() {
            let manifest = directory.join(MANIFEST_NAME);
            if !manifest.is_file() {
                continue;
            }
            let sources = Self::load(&manifest).ok()?;
            if sources.project_files.contains(&source) {
                return Some(sources);
            }
        }
        None
    }

    /// Load all workspace members and their local native dependencies without
    /// reading Lisp, evaluating code or fetching sources. Unbuilt declared outputs
    /// are omitted; missing authored inputs are reported to callers.
    pub fn load(manifest: &Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let manifest = manifest.canonicalize()?;
        let mut sources = Self {
            manifest: manifest.clone(),
            manifests: Vec::new(),
            files: Vec::new(),
            project_files: Vec::new(),
        };
        let mut workspace = None;
        for ancestor in manifest
            .parent()
            .expect("canonical manifest parent")
            .ancestors()
        {
            let candidate = ancestor.join(MANIFEST_NAME);
            if candidate.is_file()
                && let ManifestFile::Workspace(declaration) =
                    parse_manifest(&std::fs::read_to_string(&candidate)?)?
            {
                let mut members = BTreeMap::new();
                let mut roots = BTreeSet::new();
                for member in declaration.members {
                    let member = ancestor.join(member).canonicalize()?;
                    let member = if member.is_dir() {
                        member.join(MANIFEST_NAME).canonicalize()?
                    } else {
                        member
                    };
                    let member_root = member.parent().expect("canonical manifest parent");
                    if member_root == ancestor
                        || !member_root.starts_with(ancestor)
                        || !roots.insert(member_root.to_owned())
                    {
                        return Err("invalid or duplicate workspace member root".into());
                    }
                    let ManifestFile::Project(project) =
                        parse_manifest(&std::fs::read_to_string(&member)?)?
                    else {
                        return Err("workspace member must declare a project".into());
                    };
                    if members.insert(project.name, member).is_some() {
                        return Err("duplicate workspace module name".into());
                    }
                }
                if candidate == manifest || members.values().any(|member| member == &manifest) {
                    workspace = Some((candidate.canonicalize()?, members));
                }
                break;
            }
        }
        let members = workspace
            .as_ref()
            .map(|(_, members)| members.clone())
            .unwrap_or_default();
        let mut visited = BTreeSet::new();
        if let Some((root, _)) = workspace {
            sources.manifest = root.clone();
            sources.manifests.push(root);
            for member in members.values() {
                sources.visit(member, &mut visited, &members, true, 0)?;
            }
        } else {
            sources.visit(&manifest, &mut visited, &members, true, 0)?;
        }
        Ok(sources)
    }

    fn visit(
        &mut self,
        path: &Path,
        visited: &mut BTreeSet<PathBuf>,
        members: &BTreeMap<ModuleName, PathBuf>,
        is_root: bool,
        depth: usize,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if depth > 128 || visited.len() >= 1024 {
            return Err("project source dependency closure is too large".into());
        }
        let path = path.canonicalize()?;
        if !visited.insert(path.clone()) {
            return Ok(());
        }
        let ManifestFile::Project(project) = parse_manifest(&std::fs::read_to_string(&path)?)?
        else {
            return Err("source membership requires a project declaration".into());
        };
        let root = path.parent().expect("a canonical file has a parent");
        self.manifests.push(path.clone());
        for dependency in &project.dependencies {
            if dependency.condition.is_some()
                || project
                    .build
                    .as_ref()
                    .is_some_and(|build| build.dependencies.contains(&dependency.alias))
            {
                continue;
            }
            let provider_path = match &dependency.source {
                DependencySource::Path { path } => Some(root.join(path).join(MANIFEST_NAME)),
                DependencySource::Registry { registry: None } => {
                    members.get(&dependency.module).cloned()
                }
                _ => None,
            };
            if let Some(path) = provider_path {
                let ManifestFile::Project(provider) =
                    parse_manifest(&std::fs::read_to_string(&path)?)?
                else {
                    return Err("local dependency is not a project".into());
                };
                if provider.name != dependency.module
                    || dependency
                        .requirement
                        .as_ref()
                        .is_some_and(|requirement| !requirement.matches(&provider.version))
                {
                    return Err("local dependency does not match declared module or version".into());
                }
                self.visit(
                    &path,
                    visited,
                    members,
                    members.values().any(|member| member == &path),
                    depth + 1,
                )?;
            }
        }
        for target in &project.targets {
            if !is_root && target.kind != BuildTargetKind::Library {
                continue;
            }
            for file in &target.files {
                if project.is_build_output(file) && !root.join(file).exists() {
                    continue;
                }
                let file = root.join(file).canonicalize()?;
                if !file.starts_with(root) {
                    return Err("declared source escapes project directory".into());
                }
                if !self.files.contains(&file) {
                    if self.files.len() >= 16384 {
                        return Err("project declares too many source files".into());
                    }
                    self.files.push(file.clone());
                }
                if is_root && !self.project_files.contains(&file) {
                    self.project_files.push(file);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn workspace_sources_include_members_links_and_skip_unbuilt_outputs() {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../.agents/tmp/zkcl/workspace-core")
            .join(format!(
                "sources-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(
            root.join("zkcl.sexp"),
            r#"(workspace (members "app" "lib"))"#,
        )
        .unwrap();
        std::fs::write(
            root.join("app/zkcl.sexp"),
            r#"(project "test/app" (version "1.0.0")
            (dependency "lib" (module "test/lib") (version "^1"))
            (library (files "app.lisp")))"#,
        )
        .unwrap();
        std::fs::write(
            root.join("lib/zkcl.sexp"),
            r#"(project "test/lib" (version "1.0.0")
            (build "build.lisp" (output "generated.lisp"))
            (library (files "lib.lisp" "generated.lisp")))"#,
        )
        .unwrap();
        std::fs::write(root.join("app/app.lisp"), "(test:lib)").unwrap();
        std::fs::write(root.join("lib/lib.lisp"), "(defun lib () 42)").unwrap();
        let sources = ProjectSources::load(&root.join("app/zkcl.sexp")).unwrap();
        assert_eq!(
            sources.manifest,
            root.join("zkcl.sexp").canonicalize().unwrap()
        );
        assert_eq!(sources.manifests.len(), 3);
        assert_eq!(sources.files.len(), 2);
        assert_eq!(sources.project_files.len(), 2);
        assert_eq!(
            ProjectSources::discover(&root.join("lib/lib.lisp"))
                .unwrap()
                .manifest,
            sources.manifest
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
