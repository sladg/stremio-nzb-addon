use anyhow::{anyhow, Result};
use axum::{extract::State, http::StatusCode, Json};
use rustls::ClientConfig;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use url::Url;

use crate::AppState;

#[derive(Debug, Serialize, Deserialize)]
pub struct HealthcheckResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct IndexerCheckBody {
    pub url: String,
    #[serde(rename = "apiKey")]
    pub api_key: String,
}

pub async fn indexer_handler(
    State(state): State<Arc<AppState>>,
    body: Result<Json<IndexerCheckBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, Json<HealthcheckResult>) {
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(HealthcheckResult {
                    ok: false,
                    error: Some("Missing url or apiKey".to_string()),
                }),
            )
        }
    };

    if body.url.is_empty() || body.api_key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(HealthcheckResult {
                ok: false,
                error: Some("Missing url or apiKey".to_string()),
            }),
        );
    }

    let result = test_indexer_health(&state.http, &body.url, &body.api_key).await;
    (StatusCode::OK, Json(result))
}

#[derive(Debug, Deserialize)]
pub struct NntpCheckBody {
    pub server: String,
}

pub async fn nntp_handler(
    body: Result<Json<NntpCheckBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, Json<HealthcheckResult>) {
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(HealthcheckResult {
                    ok: false,
                    error: Some("Missing server".to_string()),
                }),
            )
        }
    };

    if body.server.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(HealthcheckResult {
                ok: false,
                error: Some("Missing server".to_string()),
            }),
        );
    }

    let result = test_nntp_health(&body.server).await;
    (StatusCode::OK, Json(result))
}

pub async fn test_indexer_health(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> HealthcheckResult {
    let mut caps_url = match Url::parse(url) {
        Ok(u) => u,
        Err(err) => {
            return HealthcheckResult {
                ok: false,
                error: Some(crate::util::redact_log(&err.to_string())),
            }
        }
    };
    caps_url.set_path("/api");
    caps_url
        .query_pairs_mut()
        .clear()
        .append_pair("t", "caps")
        .append_pair("apikey", api_key);

    let resp = match timeout(
        Duration::from_secs(5),
        client.get(caps_url).send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(err)) => {
            return HealthcheckResult {
                ok: false,
                error: Some(crate::util::redact_log(&err.to_string())),
            }
        }
        Err(_) => {
            return HealthcheckResult {
                ok: false,
                error: Some("Connection timeout (5s)".to_string()),
            }
        }
    };

    let status = resp.status();
    if !status.is_success() {
        return HealthcheckResult {
            ok: false,
            error: Some(format!(
                "HTTP {}: {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or("")
            )),
        };
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.contains("application/json") {
        if let Err(err) = resp.json::<serde_json::Value>().await {
            return HealthcheckResult {
                ok: false,
                error: Some(crate::util::redact_log(&err.to_string())),
            };
        }
    } else if content_type.contains("xml") {
        if let Err(err) = resp.text().await {
            return HealthcheckResult {
                ok: false,
                error: Some(crate::util::redact_log(&err.to_string())),
            };
        }
    }

    HealthcheckResult { ok: true, error: None }
}

pub async fn test_nntp_health(server_url: &str) -> HealthcheckResult {
    match timeout(Duration::from_secs(5), nntp_dance(server_url)).await {
        Ok(Ok(())) => HealthcheckResult { ok: true, error: None },
        Ok(Err(err)) => HealthcheckResult {
            ok: false,
            error: Some(crate::util::redact_log(&err.to_string())),
        },
        Err(_) => HealthcheckResult {
            ok: false,
            error: Some("Connection timeout (5s)".to_string()),
        },
    }
}

async fn nntp_dance(server_url: &str) -> Result<()> {
    let url = Url::parse(server_url)?;
    let secure = url.scheme() == "nntps";
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("missing host"))?
        .to_string();
    let port = url.port().unwrap_or(if secure { 563 } else { 119 });
    let username = urlencoding::decode(url.username())?.into_owned();
    let password = urlencoding::decode(url.password().unwrap_or(""))?.into_owned();

    let tcp = TcpStream::connect((host.as_str(), port)).await?;

    if secure {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut tls_cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        // Match Node's `rejectUnauthorized: false` for nntps://.
        tls_cfg
            .dangerous()
            .set_certificate_verifier(Arc::new(NoVerify));
        let connector = TlsConnector::from(Arc::new(tls_cfg));
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| anyhow!("invalid server name"))?;
        let stream = connector.connect(server_name, tcp).await?;
        run_dance(stream, &username, &password).await
    } else {
        run_dance(tcp, &username, &password).await
    }
}

/// BODY-probe a single article against an NNTP server, matching TS
/// `nzbAvailability.ts::probeArticleOnServer`. Returns `Ok(true)` for `222`
/// (article available) and `Ok(false)` for `430` (no such article). 5 s timeout.
///
/// Uses BODY (not STAT) because UsenetExpress / block backbones index articles
/// after their bodies are purged — only BODY returns the real `430`. The body
/// itself is *not* read; the socket is dropped immediately on receiving the
/// response code line.
pub async fn body_probe(server_url: &str, message_id: &str) -> Result<bool> {
    timeout(Duration::from_secs(5), body_probe_inner(server_url, message_id))
        .await
        .map_err(|_| anyhow!("Connection timeout (5s)"))?
}

async fn body_probe_inner(server_url: &str, message_id: &str) -> Result<bool> {
    let url = Url::parse(server_url)?;
    let secure = url.scheme() == "nntps";
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("missing host"))?
        .to_string();
    let port = url.port().unwrap_or(if secure { 563 } else { 119 });
    let username = urlencoding::decode(url.username())?.into_owned();
    let password = urlencoding::decode(url.password().unwrap_or(""))?.into_owned();

    let tcp = TcpStream::connect((host.as_str(), port)).await?;

    if secure {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut tls_cfg = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls_cfg
            .dangerous()
            .set_certificate_verifier(Arc::new(NoVerify));
        let connector = TlsConnector::from(Arc::new(tls_cfg));
        let server_name = rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| anyhow!("invalid server name"))?;
        let stream = connector.connect(server_name, tcp).await?;
        run_body_probe(stream, &username, &password, message_id).await
    } else {
        run_body_probe(tcp, &username, &password, message_id).await
    }
}

async fn run_body_probe<S>(
    mut stream: S,
    username: &str,
    password: &str,
    message_id: &str,
) -> Result<bool>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut buf = String::new();
    let mut tmp = [0u8; 1024];
    let mut authenticated = username.is_empty() && password.is_empty();

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(anyhow!("Connection closed unexpectedly"));
        }
        buf.push_str(std::str::from_utf8(&tmp[..n])?);

        while let Some(idx) = buf.find("\r\n") {
            let line = buf[..idx].to_string();
            buf.drain(..idx + 2);

            let code: u16 = line.get(..3).and_then(|s| s.parse().ok()).unwrap_or(0);

            if (code == 200 || code == 201) && !authenticated {
                if !username.is_empty() && !password.is_empty() {
                    stream
                        .write_all(format!("AUTHINFO USER {username}\r\n").as_bytes())
                        .await?;
                } else {
                    authenticated = true;
                    stream
                        .write_all(format!("BODY <{message_id}>\r\n").as_bytes())
                        .await?;
                }
            } else if code == 381 {
                stream
                    .write_all(format!("AUTHINFO PASS {password}\r\n").as_bytes())
                    .await?;
            } else if code == 281 {
                authenticated = true;
                stream
                    .write_all(format!("BODY <{message_id}>\r\n").as_bytes())
                    .await?;
            } else if (480..490).contains(&code) {
                return Err(anyhow!("Authentication failed: {line}"));
            } else if code == 222 {
                // Article exists; drop socket without reading body.
                return Ok(true);
            } else if code == 430 || code == 423 {
                // No such article (430) / no article with that number (423).
                return Ok(false);
            } else if code >= 500 {
                return Err(anyhow!("Server error: {line}"));
            }
        }
    }
}

async fn run_dance<S>(mut stream: S, username: &str, password: &str) -> Result<()>
where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
{
    let mut buf = String::new();
    let mut tmp = [0u8; 1024];
    let creds_provided = !username.is_empty() && !password.is_empty();

    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            // Connection closed mid-handshake. If creds were configured but
            // we got here without seeing a 281, treat as auth failure —
            // otherwise (no creds path) the close after a 200 is fine.
            return if creds_provided {
                Err(anyhow!("Connection closed before auth"))
            } else {
                Ok(())
            };
        }
        buf.push_str(std::str::from_utf8(&tmp[..n])?);

        while let Some(idx) = buf.find("\r\n") {
            let line = buf[..idx].to_string();
            buf.drain(..idx + 2);

            let code: u16 = line
                .get(..3)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            if code == 200 || code == 201 {
                if creds_provided {
                    stream
                        .write_all(format!("AUTHINFO USER {username}\r\n").as_bytes())
                        .await?;
                } else {
                    return Ok(());
                }
            } else if code == 381 {
                stream
                    .write_all(format!("AUTHINFO PASS {password}\r\n").as_bytes())
                    .await?;
            } else if code == 281 {
                return Ok(());
            } else if (480..490).contains(&code) {
                return Err(anyhow!("Authentication failed: {line}"));
            } else if code >= 500 {
                return Err(anyhow!("Server error: {line}"));
            }
        }
    }
}

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ECDSA_NISTP521_SHA512,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ED25519,
        ]
    }
}
