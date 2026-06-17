//! Vault-backed [`a2w_engine::CredentialResolver`] implementation.
//!
//! [`StoreCredentialResolver`] bridges the AES-256-GCM [`Vault`] and the
//! [`Store`] so the engine can resolve `credential_ref`s to their plaintext
//! secrets at run time without ever touching unencrypted data in the IR.

#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::{Store, Vault};

/// A [`a2w_engine::CredentialResolver`] that resolves credential references
/// through a [`Vault`]-backed [`Store`].
///
/// Both the [`Store`] and the [`Vault`] are held behind [`Arc`] so the resolver
/// can be cloned cheaply and shared across async tasks without requiring
/// `Clone` on `Store` or `Vault` themselves (neither derives `Clone`, by
/// design — `Store` owns a connection pool; `Vault` guards key material).
pub struct StoreCredentialResolver {
    store: Arc<Store>,
    vault: Arc<Vault>,
}

impl StoreCredentialResolver {
    /// Construct a resolver from an already-`Arc`-wrapped store and vault.
    pub fn new(store: Arc<Store>, vault: Arc<Vault>) -> Self {
        Self { store, vault }
    }
}

#[async_trait::async_trait]
impl a2w_engine::CredentialResolver for StoreCredentialResolver {
    async fn resolve(
        &self,
        credential_ref: &str,
    ) -> Result<Option<String>, a2w_engine::CredentialError> {
        self.vault
            .get_secret(&self.store, credential_ref)
            .await
            .map_err(|e| a2w_engine::CredentialError::Lookup(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2w_engine::CredentialResolver as _;

    /// Build an in-memory store, store a secret, and assert round-trip through
    /// the resolver.
    #[tokio::test]
    async fn resolver_resolves_stored_secret() {
        let store = Arc::new(
            Store::connect("sqlite::memory:")
                .await
                .expect("connect in-memory store"),
        );
        let vault = Arc::new(Vault::new([7u8; 32]));

        vault
            .store_secret(&store, "api_key", "API Key", "s3cr3t")
            .await
            .expect("store secret");

        let resolver = StoreCredentialResolver::new(Arc::clone(&store), Arc::clone(&vault));

        let got = resolver
            .resolve("api_key")
            .await
            .expect("resolve must succeed");
        assert_eq!(got, Some("s3cr3t".to_string()), "resolved secret must match plaintext");
    }

    /// A missing credential ref must return `Ok(None)` rather than an error.
    #[tokio::test]
    async fn resolver_returns_none_for_missing_ref() {
        let store = Arc::new(
            Store::connect("sqlite::memory:")
                .await
                .expect("connect in-memory store"),
        );
        let vault = Arc::new(Vault::new([7u8; 32]));
        let resolver = StoreCredentialResolver::new(store, vault);

        let got = resolver
            .resolve("missing")
            .await
            .expect("resolve for missing ref must not error");
        assert!(got.is_none(), "missing credential ref must yield None");
    }

    /// Decrypting with a different key must fail closed — `Err(Lookup(_))` —
    /// not return garbage or panic.
    #[tokio::test]
    async fn resolver_wrong_key_fails_closed() {
        let store = Arc::new(
            Store::connect("sqlite::memory:")
                .await
                .expect("connect in-memory store"),
        );
        let good_vault = Arc::new(Vault::new([7u8; 32]));

        good_vault
            .store_secret(&store, "api_key", "API Key", "s3cr3t")
            .await
            .expect("store secret");

        // Build a resolver with a DIFFERENT key.
        let bad_vault = Arc::new(Vault::new([9u8; 32]));
        let resolver = StoreCredentialResolver::new(store, bad_vault);

        let result = resolver.resolve("api_key").await;
        assert!(
            matches!(result, Err(a2w_engine::CredentialError::Lookup(_))),
            "wrong-key decryption must return Err(CredentialError::Lookup(_)), got {result:?}"
        );
    }
}
