//! Server-side publisher authorization. Authentication belongs to the hosting
//! service; no token, registry name, or caller-supplied subject is a credential.
//!
//! The directory is private server state. Opening an admin handle requires local
//! administrator access and must never be offered as a remote operation. A single
//! locked, atomically replaced state document commits grants, releases and audit
//! together. Content is imported before commit; interrupted uploads may leave
//! unreferenced immutable blobs, never a visible partial release.

use crate::{ContentStore, Digest, RegistryError, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;

/// A trusted authentication adapter, implemented by the hosting server using its
/// established identity provider. Verify issuer, audience, expiry and credential
/// revocation there. Return an immutable issuer-qualified account/team identifier,
/// not an account display name. Never implement this as an identity passthrough.
pub trait PublisherAuthenticator {
    type Credentials: ?Sized;
    fn authenticate(&self, credentials: &Self::Credentials) -> Result<String>;
}

/// Private server administrator access. There is deliberately no public remote
/// operation that turns an authenticated publisher into an administrator.
#[derive(Clone, Debug)]
pub struct AuthorityAdmin {
    root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RegistryAuthority<A> {
    admin: AuthorityAdmin,
    authenticator: A,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct Publication {
    pub version: String,
    /// Lowercase SHA-256 of the canonical source archive.
    pub checksum: String,
    pub manifest: String,
    pub publisher: String,
    pub yanked: bool,
}

/// Sequence order is the transaction order. Subjects are trusted identity
/// provider identifiers; audit is durable but not a signed transparency log.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct AuthorityAudit {
    pub sequence: u64,
    pub actor: String,
    pub action: String,
    pub scope: String,
    pub subject: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorityStore {
    namespaces: BTreeMap<String, NamespaceRecord>,
    delegates: BTreeMap<String, BTreeSet<String>>,
    releases: BTreeMap<String, BTreeMap<String, Publication>>,
    audit: Vec<AuthorityAudit>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamespaceRecord {
    /// Permanently bound owner; publisher grants may change independently.
    owner: String,
    publishers: BTreeSet<String>,
}

impl AuthorityAdmin {
    /// Trusted local operation. The state directory is not attacker-writable.
    /// No name (including a registry ID named "official") provisions `zk`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let admin = Self {
            root: fs::canonicalize(root)?,
        };
        admin.transaction(|_| Ok(()))?;
        Ok(admin)
    }

    pub fn serve<A: PublisherAuthenticator>(&self, authenticator: A) -> RegistryAuthority<A> {
        RegistryAuthority {
            admin: self.clone(),
            authenticator,
        }
    }

    /// Explicit provisioning binds `zk` to the team's verified stable identity.
    /// Repeating identical provisioning is idempotent; reassignment is rejected.
    pub fn provision_namespace(&self, namespace: &str, owner: &str) -> Result<()> {
        validate_namespace(namespace)?;
        validate_subject(owner)?;
        self.transaction(|state| provision(state, namespace, owner, "administrator"))
    }

    pub fn audit(&self) -> Result<Vec<AuthorityAudit>> {
        self.transaction(|state| Ok(state.audit.clone()))
    }

    /// Materialize a coherent read snapshot for sparse/TUF export. The output
    /// must be a new directory; mutable publication and grant state stay private.
    pub fn export_registry(
        &self,
        output: &std::path::Path,
        identity: &str,
    ) -> Result<crate::FileRegistry> {
        if output.try_exists()? {
            return Err(denied("authority snapshot destination already exists"));
        }
        self.transaction(|state| {
            let registry = crate::FileRegistry::init(output, identity)?;
            for (module, releases) in &state.releases {
                for publication in releases.values() {
                    let checksum: Digest = publication.checksum.parse()?;
                    registry.import_archive(
                        module,
                        &publication.version,
                        checksum,
                        self.store().archive(checksum)?,
                    )?;
                    if publication.yanked {
                        registry.set_yanked(module, &publication.version, true)?;
                    }
                }
            }
            Ok(registry)
        })
    }

    fn store(&self) -> ContentStore {
        ContentStore::new(self.root.join("content"))
    }

    fn transaction<T>(
        &self,
        operation: impl FnOnce(&mut AuthorityStore) -> Result<T>,
    ) -> Result<T> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("authority.lock"))?;
        lock.lock()?;
        let path = self.root.join("authority.json");
        let mut state = match File::open(&path) {
            Ok(file) => {
                if file.metadata()?.len() > 64 * 1024 * 1024 {
                    return Err(denied("authority state limit"));
                }
                serde_json::from_reader(file)
                    .map_err(|e| denied(&format!("invalid authority state: {e}")))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AuthorityStore::default(),
            Err(e) => return Err(e.into()),
        };
        let result = operation(&mut state)?;
        let mut pending = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(pending.as_file_mut(), &state)
            .map_err(|e| denied(&format!("cannot encode authority state: {e}")))?;
        pending.flush()?;
        if pending.as_file().metadata()?.len() > 64 * 1024 * 1024 {
            return Err(denied("authority state limit"));
        }
        pending.as_file().sync_all()?;
        pending
            .persist(path)
            .map_err(|e| RegistryError::Io(e.error))?;
        File::open(&self.root)?.sync_all()?;
        Ok(result)
    }
}

impl<A: PublisherAuthenticator> RegistryAuthority<A> {
    pub fn subject(&self, credentials: &A::Credentials) -> Result<String> {
        let subject = self.authenticator.authenticate(credentials)?;
        validate_subject(&subject)?;
        Ok(subject)
    }

    pub fn claim_namespace(&self, credentials: &A::Credentials, namespace: &str) -> Result<()> {
        let subject = self.subject(credentials)?;
        validate_namespace(namespace)?;
        if namespace == "zk" {
            return Err(denied("zk requires explicit team provisioning"));
        }
        self.admin
            .transaction(|state| provision(state, namespace, &subject, &subject))
    }

    /// Namespace owner manages namespace-wide publishers. Revoking one does not
    /// remove an independent per-package grant; revoke that explicit grant too.
    pub fn set_namespace_publisher(
        &self,
        credentials: &A::Credentials,
        namespace: &str,
        publisher: &str,
        enabled: bool,
    ) -> Result<()> {
        let actor = self.subject(credentials)?;
        validate_namespace(namespace)?;
        validate_subject(publisher)?;
        self.admin.transaction(|state| {
            let ns = state
                .namespaces
                .get_mut(namespace)
                .ok_or_else(|| denied("unknown namespace"))?;
            if ns.owner != actor {
                return Err(denied("namespace owner required"));
            }
            if enabled {
                ns.publishers.insert(publisher.to_owned());
            } else {
                ns.publishers.remove(publisher);
            }
            audit(
                state,
                &actor,
                if enabled {
                    "grant-namespace"
                } else {
                    "revoke-namespace"
                },
                namespace,
                publisher,
            );
            Ok(())
        })
    }

    /// Only the permanent namespace owner can delegate publication for a package.
    /// A package delegate cannot expand its grant or publish sibling packages.
    pub fn set_package_publisher(
        &self,
        credentials: &A::Credentials,
        module: &str,
        publisher: &str,
        enabled: bool,
    ) -> Result<()> {
        let actor = self.subject(credentials)?;
        let namespace = namespace_of(module)?;
        validate_subject(publisher)?;
        self.admin.transaction(|state| {
            if state
                .namespaces
                .get(namespace)
                .is_none_or(|ns| ns.owner != actor)
            {
                return Err(denied("namespace owner required"));
            }
            let delegates = state.delegates.entry(module.to_owned()).or_default();
            if enabled {
                delegates.insert(publisher.to_owned());
            } else {
                delegates.remove(publisher);
            }
            audit(
                state,
                &actor,
                if enabled {
                    "grant-package"
                } else {
                    "revoke-package"
                },
                module,
                publisher,
            );
            Ok(())
        })
    }

    /// Accept a canonical uploaded archive after authentication and authorization.
    /// The host must additionally validate manifest semantics/dependency policy.
    pub fn publish(
        &self,
        credentials: &A::Credentials,
        module: &str,
        version: &str,
        expected: Digest,
        archive: impl Read,
    ) -> Result<Publication> {
        let actor = self.subject(credentials)?;
        namespace_of(module)?;
        super::file_registry::validate_version(version)?;
        self.admin.transaction(|state| {
            authorize(state, module, &actor)?;
            let store = self.admin.store();
            store.import(archive, expected)?;
            let manifest = store.manifest(expected)?;
            let publication = Publication {
                version: version.to_owned(),
                checksum: expected.to_string(),
                manifest,
                publisher: actor.clone(),
                yanked: false,
            };
            let releases = state.releases.entry(module.to_owned()).or_default();
            if let Some(existing) = releases.get(version) {
                if existing.checksum == publication.checksum
                    && existing.manifest == publication.manifest
                {
                    return Ok(existing.clone());
                }
                return Err(RegistryError::ImmutableRelease {
                    module: module.to_owned(),
                    version: version.to_owned(),
                });
            }
            releases.insert(version.to_owned(), publication.clone());
            audit(state, &actor, "publish", module, version);
            Ok(publication)
        })
    }

    pub fn set_yanked(
        &self,
        credentials: &A::Credentials,
        module: &str,
        version: &str,
        yanked: bool,
    ) -> Result<()> {
        let actor = self.subject(credentials)?;
        namespace_of(module)?;
        self.admin.transaction(|state| {
            authorize(state, module, &actor)?;
            let release = state
                .releases
                .get_mut(module)
                .and_then(|rs| rs.get_mut(version))
                .ok_or_else(|| denied("unknown release"))?;
            release.yanked = yanked;
            audit(
                state,
                &actor,
                if yanked { "yank" } else { "unyank" },
                module,
                version,
            );
            Ok(())
        })
    }

    pub fn releases(&self, module: &str) -> Result<Vec<Publication>> {
        namespace_of(module)?;
        self.admin.transaction(|state| {
            Ok(state
                .releases
                .get(module)
                .map(|rs| rs.values().cloned().collect())
                .unwrap_or_default())
        })
    }

    /// Downloads retain archive verification, even for withdrawn releases.
    pub fn archive(&self, checksum: Digest) -> Result<File> {
        self.admin.store().archive(checksum)
    }
}

fn provision(state: &mut AuthorityStore, namespace: &str, owner: &str, actor: &str) -> Result<()> {
    if let Some(existing) = state.namespaces.get(namespace) {
        return if existing.owner == owner {
            Ok(())
        } else {
            Err(denied("namespace permanently belongs to another owner"))
        };
    }
    state.namespaces.insert(
        namespace.to_owned(),
        NamespaceRecord {
            owner: owner.to_owned(),
            publishers: BTreeSet::new(),
        },
    );
    audit(state, actor, "provision-namespace", namespace, owner);
    Ok(())
}

fn authorize(state: &AuthorityStore, module: &str, actor: &str) -> Result<()> {
    let namespace = namespace_of(module)?;
    let ns = state
        .namespaces
        .get(namespace)
        .ok_or_else(|| denied("unknown namespace"))?;
    if ns.owner == actor
        || ns.publishers.contains(actor)
        || state
            .delegates
            .get(module)
            .is_some_and(|delegates| delegates.contains(actor))
    {
        Ok(())
    } else {
        Err(denied("publisher has no grant for this package"))
    }
}

fn audit(state: &mut AuthorityStore, actor: &str, action: &str, scope: &str, subject: &str) {
    state.audit.push(AuthorityAudit {
        sequence: state.audit.len() as u64 + 1,
        actor: actor.to_owned(),
        action: action.to_owned(),
        scope: scope.to_owned(),
        subject: subject.to_owned(),
    });
}

fn namespace_of(module: &str) -> Result<&str> {
    super::file_registry::validate_module(module)?;
    Ok(module.split_once('/').expect("validated module").0)
}
fn validate_namespace(namespace: &str) -> Result<()> {
    super::file_registry::validate_module(&format!("{namespace}/probe"))
}
fn validate_subject(subject: &str) -> Result<()> {
    if subject.is_empty()
        || subject.len() > 1024
        || subject.trim() != subject
        || subject.chars().any(char::is_control)
    {
        Err(denied("invalid authenticated publisher identity"))
    } else {
        Ok(())
    }
}
fn denied(message: &str) -> RegistryError {
    RegistryError::Registry(format!("publisher authority: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Auth;
    // Test credentials deliberately differ from the resulting verified subject.
    impl PublisherAuthenticator for Auth {
        type Credentials = str;
        fn authenticate(&self, credential: &str) -> Result<String> {
            match credential {
                "owner-token" => Ok("issuer:team:zkcl".into()),
                "alice-token" => Ok("issuer:user:alice".into()),
                "ci-token" => Ok("issuer:ci:release".into()),
                _ => Err(denied("invalid credential")),
            }
        }
    }
    fn archive(module: &str, body: &str) -> (Vec<u8>, Digest) {
        let source = crate::tests::temporary();
        fs::write(
            source.path().join("zkcl.sexp"),
            format!("(project \"{module}\" (version \"1.0.0\"))"),
        )
        .unwrap();
        fs::write(source.path().join("main.lisp"), body).unwrap();
        let mut bytes = Vec::new();
        let digest =
            crate::write_source_archive(source.path(), &["zkcl.sexp", "main.lisp"], &mut bytes)
                .unwrap();
        (bytes, digest)
    }
    #[test]
    fn explicit_official_provisioning_and_permanent_namespace_binding() {
        let root = crate::tests::temporary();
        let admin = AuthorityAdmin::open(root.path()).unwrap();
        let service = admin.serve(Auth);
        assert!(service.claim_namespace("owner-token", "zk").is_err());
        assert!(
            service
                .claim_namespace("issuer:team:zkcl", "other")
                .is_err()
        );
        admin.provision_namespace("zk", "issuer:team:zkcl").unwrap();
        assert!(
            admin
                .provision_namespace("zk", "issuer:user:alice")
                .is_err()
        );
        service.claim_namespace("alice-token", "alice").unwrap();
        assert!(service.claim_namespace("owner-token", "alice").is_err());
        let reopened = AuthorityAdmin::open(root.path()).unwrap();
        assert!(
            reopened
                .provision_namespace("alice", "issuer:team:zkcl")
                .is_err()
        );
        // An unrelated registry's same namespace grants nothing here.
        let other_root = crate::tests::temporary();
        let other = AuthorityAdmin::open(other_root.path()).unwrap().serve(Auth);
        let (bytes, digest) = archive("zk/library", "first");
        assert!(
            other
                .publish(
                    "owner-token",
                    "zk/library",
                    "1.0.0",
                    digest,
                    bytes.as_slice()
                )
                .is_err()
        );
        assert_eq!(admin.audit().unwrap().len(), 2);
    }
    #[test]
    fn package_grants_revocation_and_immutable_release_are_durable() {
        let root = crate::tests::temporary();
        let admin = AuthorityAdmin::open(root.path()).unwrap();
        admin.provision_namespace("zk", "issuer:team:zkcl").unwrap();
        let service = admin.serve(Auth);
        let (bytes, digest) = archive("zk/library", "first");
        assert!(
            service
                .publish("ci-token", "zk/library", "1.0.0", digest, bytes.as_slice())
                .is_err()
        );
        service
            .set_package_publisher("owner-token", "zk/library", "issuer:ci:release", true)
            .unwrap();
        assert!(
            service
                .set_package_publisher("ci-token", "zk/other", "issuer:ci:release", true)
                .is_err()
        );
        assert!(
            service
                .publish("ci-token", "zk/other", "1.0.0", digest, bytes.as_slice())
                .is_err()
        );
        let original = service
            .publish("ci-token", "zk/library", "1.0.0", digest, bytes.as_slice())
            .unwrap();
        assert_eq!(
            service
                .publish("ci-token", "zk/library", "1.0.0", digest, bytes.as_slice())
                .unwrap(),
            original
        );
        let (changed, changed_digest) = archive("zk/library", "changed");
        assert!(matches!(
            service.publish(
                "ci-token",
                "zk/library",
                "1.0.0",
                changed_digest,
                changed.as_slice()
            ),
            Err(RegistryError::ImmutableRelease { .. })
        ));
        service
            .set_package_publisher("owner-token", "zk/library", "issuer:ci:release", false)
            .unwrap();
        let service = AuthorityAdmin::open(root.path()).unwrap().serve(Auth);
        assert!(
            service
                .publish("ci-token", "zk/library", "2.0.0", digest, bytes.as_slice())
                .is_err()
        );
        assert!(
            service
                .set_yanked("ci-token", "zk/library", "1.0.0", true)
                .is_err()
        );
        service
            .set_yanked("owner-token", "zk/library", "1.0.0", true)
            .unwrap();
        let released = service.releases("zk/library").unwrap();
        assert_eq!(released.len(), 1);
        assert!(released[0].yanked);
        assert_eq!(released[0].checksum, original.checksum);
        assert!(service.archive(digest).is_ok());
        let audit = admin.audit().unwrap();
        assert_eq!(
            audit.iter().map(|a| a.action.as_str()).collect::<Vec<_>>(),
            [
                "provision-namespace",
                "grant-package",
                "publish",
                "revoke-package",
                "yank"
            ]
        );
        assert!(
            audit
                .iter()
                .enumerate()
                .all(|(i, a)| a.sequence == i as u64 + 1)
        );
    }
}
