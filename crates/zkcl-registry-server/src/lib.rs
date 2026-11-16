//! HTTP publishing and registry server adapter for `zkcl-registry`.

use std::{io::Read, net::SocketAddr, time::Duration};
use zkcl_registry::authority::{PublisherAuthenticator, RegistryAuthority};
use zkcl_registry::remote::{Control, MAX_BODY, error, validate_token};
use zkcl_registry::{ArchiveLimits, ContentStore, Digest, RegistryError, Result};

/// Handle one API request. Authentication remains owned by the host adapter.
pub fn handle_publishing_request<A: PublisherAuthenticator<Credentials = str>>(
    authority: &RegistryAuthority<A>,
    path: &str,
    token: &str,
    body: &[u8],
) -> Result<Vec<u8>> {
    if body.len() as u64 > MAX_BODY {
        return Err(error("registry request exceeds limit"));
    }
    handle_publishing_reader(authority, path, token, body)
}

fn handle_publishing_reader<A: PublisherAuthenticator<Credentials = str>>(
    authority: &RegistryAuthority<A>,
    path: &str,
    token: &str,
    body: impl Read,
) -> Result<Vec<u8>> {
    validate_token(token)?;
    if path == "/api/v1/control" {
        let mut bytes = Vec::new();
        body.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 {
            return Err(error("registry control request exceeds limit"));
        }
        let control: Control = serde_json::from_slice(&bytes)
            .map_err(|_| error("invalid registry control request"))?;
        match control {
            Control::Authenticate => {
                authority.subject(token)?;
            }
            Control::ClaimNamespace { namespace } => {
                authority.claim_namespace(token, &namespace)?
            }
            Control::Yank {
                module,
                version,
                yanked,
            } => authority.set_yanked(token, &module, &version, yanked)?,
            Control::PackagePublisher {
                module,
                publisher,
                enabled,
            } => authority.set_package_publisher(token, &module, &publisher, enabled)?,
            Control::NamespacePublisher {
                namespace,
                publisher,
                enabled,
            } => authority.set_namespace_publisher(token, &namespace, &publisher, enabled)?,
        }
        return Ok(b"{}".to_vec());
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() != 8 || parts[1..4] != ["api", "v1", "publish"] {
        return Err(error("unknown registry endpoint"));
    }
    let module = format!("{}/{}", parts[4], parts[5]);
    let digest: Digest = parts[7].parse()?;
    let temporary = tempfile::tempdir()?;
    let store = ContentStore::with_limits(
        temporary.path(),
        ArchiveLimits {
            max_archive_bytes: MAX_BODY,
            ..ArchiveLimits::default()
        },
    );
    store.import(body, digest)?;
    let text = store.manifest(digest)?;
    let zkcl_project::manifest::ManifestFile::Project(manifest) =
        zkcl_project::manifest::parse_manifest(&text).map_err(|e| error(e.to_string()))?
    else {
        return Err(error("published archive must contain a project manifest"));
    };
    if manifest.name.as_str() != module
        || manifest.version.to_string() != parts[6]
        || manifest.license.is_none()
    {
        return Err(error(
            "published manifest must match coordinate and declare license",
        ));
    }
    if manifest.dependencies.iter().any(|dependency| {
        matches!(
            dependency.source,
            zkcl_project::manifest::DependencySource::Path { .. }
        )
    }) {
        return Err(error("published dependencies must use immutable sources"));
    }
    let publication =
        authority.publish(token, &module, parts[6], digest, store.archive(digest)?)?;
    serde_json::to_vec(&publication).map_err(|e| error(e.to_string()))
}

/// Serve the authority API and an optional signed immutable snapshot. TLS belongs
/// at the reverse proxy. The service defaults to loopback; explicitly configured
/// container/private interfaces can be served behind HTTPS ingress. Each upload
/// has a 120 second read deadline and a 64 MiB limit; authority disk operations run
/// on Tokio's blocking pool. Ctrl-C drains in-flight requests before returning.
pub fn serve_origin<A>(
    authority: RegistryAuthority<A>,
    address: SocketAddr,
    sparse: Option<std::path::PathBuf>,
) -> Result<()>
where
    A: PublisherAuthenticator<Credentials = str> + Send + Sync + 'static,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        use axum::{Router, routing::post};
        let authority = std::sync::Arc::new(authority);
        let mut router = Router::new()
            .route("/api/v1/control", post(api::<A>))
            .route(
                "/api/v1/publish/{namespace}/{name}/{version}/{digest}",
                post(api::<A>),
            )
            .with_state(authority)
            .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY as usize));
        if let Some(sparse) = sparse {
            let sparse = std::fs::canonicalize(sparse)?;
            // ServeDir streams files and constrains traversal to this directory.
            router = router.fallback_service(tower_http::services::ServeDir::new(sparse));
        }
        let listener = tokio::net::TcpListener::bind(address).await?;
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await?;
        Ok(())
    })
}

async fn api<A>(
    axum::extract::State(authority): axum::extract::State<std::sync::Arc<RegistryAuthority<A>>>,
    request: axum::extract::Request,
) -> axum::response::Response
where
    A: PublisherAuthenticator<Credentials = str> + Send + Sync + 'static,
{
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    let token = match request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    {
        Some(token) if validate_token(token).is_ok() => token.to_owned(),
        _ => return (StatusCode::UNAUTHORIZED, "authentication required").into_response(),
    };
    let authentication = {
        let authority = authority.clone();
        let token = token.clone();
        tokio::task::spawn_blocking(move || authority.subject(&token)).await
    };
    if !matches!(authentication, Ok(Ok(_))) {
        return (StatusCode::UNAUTHORIZED, "invalid credential").into_response();
    }
    let path = request.uri().path().to_owned();
    let body_limit = if path == "/api/v1/control" {
        16 * 1024
    } else {
        MAX_BODY
    };
    // Stream into a private temporary file. Memory use is bounded by the HTTP
    // chunk size rather than the maximum archive size.
    let temporary = match tempfile::NamedTempFile::new() {
        Ok(file) => file,
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot stage upload").into_response();
        }
    };
    let destination = match temporary.reopen() {
        Ok(file) => tokio::fs::File::from_std(file),
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "cannot stage upload").into_response();
        }
    };
    let staged = tokio::time::timeout(Duration::from_secs(120), async move {
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut destination = destination;
        let mut stream = request.into_body().into_data_stream();
        let mut size = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| error("cannot read request body"))?;
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| error("request body limit exceeded"))?;
            if size > body_limit {
                return Err(error("request body limit exceeded"));
            }
            destination.write_all(&chunk).await?;
        }
        destination.flush().await?;
        Result::Ok(())
    })
    .await;
    match staged {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return (StatusCode::PAYLOAD_TOO_LARGE, "request body rejected").into_response();
        }
        Err(_) => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                "request body deadline exceeded",
            )
                .into_response();
        }
    }
    match tokio::task::spawn_blocking(move || {
        handle_publishing_reader(&authority, &path, &token, temporary.reopen()?)
    })
    .await
    {
        Ok(Ok(body)) => ([("content-type", "application/json")], body).into_response(),
        Ok(Err(RegistryError::ImmutableRelease { .. })) => {
            (StatusCode::CONFLICT, "immutable release").into_response()
        }
        Ok(Err(_)) => (StatusCode::BAD_REQUEST, "registry request rejected").into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "registry operation failed",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zkcl_registry::authority::AuthorityAdmin;
    use zkcl_registry::remote::RegistryClient;

    struct Auth;
    impl PublisherAuthenticator for Auth {
        type Credentials = str;
        fn authenticate(&self, token: &str) -> Result<String> {
            match token {
                "owner-token" => Ok("issuer:user:alice".into()),
                _ => Err(error("invalid token")),
            }
        }
    }

    #[test]
    fn http_publication_authority_and_signed_export_source() {
        let root = tempfile::tempdir().unwrap();
        let admin = AuthorityAdmin::open(root.path()).unwrap();
        let authority = admin.serve(Auth);
        let (address_tx, address_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    address_tx.send(listener.local_addr().unwrap()).unwrap();
                    let router = axum::Router::new()
                        .route("/api/v1/control", axum::routing::post(api::<Auth>))
                        .route(
                            "/api/v1/publish/{namespace}/{name}/{version}/{digest}",
                            axum::routing::post(api::<Auth>),
                        )
                        .with_state(std::sync::Arc::new(authority));
                    axum::serve(listener, router)
                        .with_graceful_shutdown(async {
                            let _ = stop_rx.await;
                        })
                        .await
                        .unwrap();
                });
        });
        let client =
            RegistryClient::new(&format!("http://{}", address_rx.recv().unwrap())).unwrap();
        assert!(
            client
                .control("wrong-token", &Control::Authenticate)
                .is_err()
        );
        client
            .control("owner-token", &Control::Authenticate)
            .unwrap();
        client
            .control(
                "owner-token",
                &Control::ClaimNamespace {
                    namespace: "alice".into(),
                },
            )
            .unwrap();
        let source = tempfile::tempdir().unwrap();
        std::fs::write(
            source.path().join("zkcl.sexp"),
            "(project \"alice/lib\" (version \"1.0.0\") (license \"MIT\"))",
        )
        .unwrap();
        let mut archive = Vec::new();
        let digest =
            zkcl_registry::write_source_archive(source.path(), &["zkcl.sexp"], &mut archive)
                .unwrap();
        let publication = client
            .publish("owner-token", "alice/lib", "1.0.0", digest, archive.clone())
            .unwrap();
        assert_eq!(publication.publisher, "issuer:user:alice");
        assert!(
            client
                .publish("wrong-token", "alice/lib", "1.0.0", digest, archive.clone())
                .is_err()
        );
        assert!(
            client
                .publish("owner-token", "alice/lib", "2.0.0", digest, archive.clone())
                .is_err()
        );
        std::fs::write(source.path().join("extra.lisp"), "(values)").unwrap();
        let mut changed = Vec::new();
        let changed_digest = zkcl_registry::write_source_archive(
            source.path(),
            &["zkcl.sexp", "extra.lisp"],
            &mut changed,
        )
        .unwrap();
        assert!(
            client
                .publish("owner-token", "alice/lib", "1.0.0", changed_digest, changed)
                .is_err()
        );
        client
            .control(
                "owner-token",
                &Control::Yank {
                    module: "alice/lib".into(),
                    version: "1.0.0".into(),
                    yanked: true,
                },
            )
            .unwrap();
        let destination = tempfile::tempdir().unwrap();
        let snapshot = admin
            .export_registry(&destination.path().join("snapshot"), "test-registry")
            .unwrap();
        let records = snapshot.records("alice/lib").unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].yanked);
        assert_eq!(records[0].checksum, digest);
        assert!(snapshot.store().archive(digest).is_ok());
        let response = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("{}/api/v1/control", client.identity()))
            .bearer_auth("owner-token")
            .body(vec![b' '; 16 * 1024 + 1])
            .send()
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
        stop_tx.send(()).unwrap();
        thread.join().unwrap();
    }
}
