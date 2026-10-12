//! Shared HTTP client construction.
//!
//! `reqwest` 0.13 is built with `rustls-no-provider` (see the workspace
//! `Cargo.toml`): its default `rustls` feature pulls in `aws-lc-rs`, a native build,
//! while `sqlx` and `tokio-rustls` in this tree already enable `ring`. The cost of
//! that choice is that `reqwest` panics at `Client::build()` unless a process-wide
//! crypto provider has been installed first. Every client in the workspace,
//! including `reqwest::get` shortcuts in tests, is therefore built through
//! [`client_builder`], which installs `ring` on first use. Nothing here touches
//! the network.

/// Install `ring` as the process-wide rustls crypto provider.
///
/// Idempotent: a second call, or an earlier install by the embedding
/// application, leaves the existing provider in place.
pub fn ensure_crypto_provider() {
    // `Err` means a provider is already installed, which is the state we want.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A `reqwest::ClientBuilder` that is safe to `build()`.
///
/// Use this instead of `reqwest::Client::builder()` anywhere in the workspace.
pub fn client_builder() -> reqwest::ClientBuilder {
    ensure_crypto_provider();
    reqwest::Client::builder()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_without_installing_a_provider_first() {
        // Would panic inside reqwest if no provider were installed.
        let client = client_builder().build().expect("client builds");
        drop(client);
    }

    #[test]
    fn ensure_is_idempotent() {
        ensure_crypto_provider();
        ensure_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
