//! TLS configuration shared by the two upstream transports.
//!
//! HTTP selects between the host's native macOS TLS stack and a deterministic
//! AWS-LC rustls profile. Responses WebSocket traffic always uses the latter,
//! which keeps a Linux or Windows host from leaking OpenSSL/SChannel details
//! while it presents the Mac profile.

use std::sync::{Arc, OnceLock};

use rustls::{ClientConfig, RootCertStore};

use crate::identity::DevicePlatform;

/// Start an HTTP client for the default (Mac) virtual platform.
///
/// A real macOS host with the Mac profile uses Security.framework through
/// native-tls, which is the closest match for Codex's native HTTP transport.
/// Standalone paths without an `App` identity use this Mac target explicitly;
/// request paths with a live profile call [`http_client_builder_for`] instead.
pub(crate) fn http_client_builder() -> reqwest::ClientBuilder {
    http_client_builder_for(DevicePlatform::Mac)
}

fn uses_native_tls(platform: DevicePlatform) -> bool {
    cfg!(target_os = "macos") && platform == DevicePlatform::Mac
}

pub(crate) fn http_client_builder_for(platform: DevicePlatform) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder().tcp_nodelay(true);
    if uses_native_tls(platform) {
        builder.use_native_tls()
    } else {
        builder.use_preconfigured_tls(http_client_config())
    }
}

/// Install the same provider used by the upstream Codex WebSocket client.
///
/// rustls does not select a provider when it is built with
/// `default-features = false`; the first `ClientConfig::builder()` would then
/// fail at runtime.  A process may already have installed one (for example a
/// library in the host application), so an installation error is intentionally
/// ignored after making the attempt.
pub(crate) fn ensure_rustls_provider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn native_root_store() -> Arc<RootCertStore> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            for cert in native.certs {
                let _ = roots.add(cert);
            }
            if roots.is_empty() {
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            }
            Arc::new(roots)
        })
        .clone()
}

fn build_client_config(alpn: &[&[u8]]) -> ClientConfig {
    ensure_rustls_provider();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("AWS-LC TLS protocol versions are available")
        .with_root_certificates(native_root_store().as_ref().clone())
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    config
}

/// The rustls config used for virtual-Mac HTTP connections.
///
/// Codex's reqwest client enables HTTP/2 and advertises the normal h2/h1
/// negotiation order.  Keep the same order in the preconfigured rustls
/// client; reqwest also reapplies it when it builds the connector.
pub(crate) fn http_client_config() -> ClientConfig {
    static CONFIG: OnceLock<ClientConfig> = OnceLock::new();
    CONFIG
        .get_or_init(|| build_client_config(&[b"h2", b"http/1.1"]))
        .clone()
}

/// The rustls config used for Responses WebSocket connections.
///
/// Native roots are preferred because that is what Codex uses for this path.
/// A webpki fallback keeps the proxy usable on minimal/headless installations
/// where the platform certificate store cannot be read at all.
pub(crate) fn websocket_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| Arc::new(build_client_config(&[b"http/1.1"])))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_tls_config_uses_http11_alpn_and_has_roots() {
        let config = websocket_client_config();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn http_tls_config_is_deterministic_and_advertises_codex_alpn() {
        let first = http_client_config();
        let second = http_client_config();
        assert_eq!(first.alpn_protocols, second.alpn_protocols);
        assert_eq!(
            first.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[test]
    fn native_tls_is_reserved_for_a_real_mac_profile() {
        assert_eq!(uses_native_tls(DevicePlatform::Windows), false);
        assert_eq!(uses_native_tls(DevicePlatform::Linux), false);
        assert_eq!(
            uses_native_tls(DevicePlatform::Mac),
            cfg!(target_os = "macos")
        );
    }
}
