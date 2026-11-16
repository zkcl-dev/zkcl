//! Authenticated HTTPS publishing client and control protocol.
//! Server adapters are available separately in `zkcl-registry-server`.
use crate::authority::Publication;
use crate::{ArchiveLimits, Digest, RegistryError, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::time::Duration;

pub const MAX_BODY: u64 = 64 * 1024 * 1024;
const MAX_RESPONSE: u64 = 8 * 1024 * 1024;
pub fn error(message: impl Into<String>) -> RegistryError {
    RegistryError::Registry(message.into())
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Control {
    Authenticate,
    ClaimNamespace {
        namespace: String,
    },
    Yank {
        module: String,
        version: String,
        yanked: bool,
    },
    PackagePublisher {
        module: String,
        publisher: String,
        enabled: bool,
    },
    NamespacePublisher {
        namespace: String,
        publisher: String,
        enabled: bool,
    },
}

pub struct RegistryClient {
    base: url::Url,
    client: reqwest::blocking::Client,
}
impl RegistryClient {
    pub fn new(endpoint: &str) -> Result<Self> {
        let base = url::Url::parse(endpoint).map_err(|_| error("invalid registry URL"))?;
        let local = matches!(
            base.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        if base.scheme() != "https" && !(base.scheme() == "http" && local) {
            return Err(error(
                "registry publishing requires HTTPS (HTTP is allowed only on loopback)",
            ));
        }
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(error(
                "registry URL must not contain credentials, query or fragment",
            ));
        }
        let builder = reqwest::blocking::Client::builder();
        // Local plaintext origins must not forward credentials through an
        // ambient HTTP proxy configured for unrelated external requests.
        let builder = if base.scheme() == "http" {
            builder.no_proxy()
        } else {
            builder
        };
        let client = builder
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| error("cannot initialize registry HTTP client"))?;
        Ok(Self { base, client })
    }
    pub fn identity(&self) -> String {
        self.base.as_str().trim_end_matches('/').to_owned()
    }
    fn send(&self, token: &str, path: &str, body: Vec<u8>, content_type: &str) -> Result<Vec<u8>> {
        validate_token(token)?;
        if body.len() as u64 > MAX_BODY {
            return Err(error("registry upload exceeds 64 MiB"));
        }
        let url = format!("{}/{path}", self.identity());
        let response = self
            .client
            .post(url)
            .bearer_auth(token)
            .header("content-type", content_type)
            .body(body)
            .send()
            .map_err(|_| error("registry HTTP request failed"))?;
        let status = response.status();
        let mut bytes = Vec::new();
        response.take(MAX_RESPONSE + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RESPONSE {
            return Err(error("registry response exceeds limit"));
        }
        // Do not repeat a remote response body: it may contain reflected credentials.
        if !status.is_success() {
            return Err(error(format!("registry request rejected ({status})")));
        }
        Ok(bytes)
    }
    pub fn control(&self, token: &str, control: &Control) -> Result<()> {
        self.send(
            token,
            "api/v1/control",
            serde_json::to_vec(control).map_err(|e| error(e.to_string()))?,
            "application/json",
        )?;
        Ok(())
    }
    pub fn publish(
        &self,
        token: &str,
        module: &str,
        version: &str,
        digest: Digest,
        archive: Vec<u8>,
    ) -> Result<Publication> {
        crate::file_registry::validate_module(module)?;
        crate::file_registry::validate_version(version)?;
        crate::verify_archive(archive.as_slice(), digest, ArchiveLimits::default())?;
        let bytes = self.send(
            token,
            &format!("api/v1/publish/{module}/{version}/{digest}"),
            archive,
            "application/x-tar",
        )?;
        let published: Publication =
            serde_json::from_slice(&bytes).map_err(|_| error("invalid publication response"))?;
        if published.version != version || published.checksum != digest.to_string() {
            return Err(error("registry publication response differs from upload"));
        }
        Ok(published)
    }
}

pub fn validate_token(token: &str) -> Result<()> {
    if token.is_empty() || token.len() > 4096 || token.bytes().any(|b| b <= 32 || b >= 127) {
        return Err(error("invalid registry token"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn publishing_transport_requires_private_credentials_and_https() {
        for url in [
            "http://registry.example",
            "https://token@registry.example",
            "https://registry.example?token=x",
            "ftp://localhost",
        ] {
            assert!(RegistryClient::new(url).is_err(), "{url}");
        }
        assert!(RegistryClient::new("https://registry.example").is_ok());
        assert!(validate_token("token\nheader").is_err());
    }
}
