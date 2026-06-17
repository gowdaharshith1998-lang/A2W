//! The credential vault: **AES-256-GCM envelope encryption** for secrets.
//!
//! A [`Vault`] holds a 32-byte master key. Each secret is encrypted under a
//! fresh random 12-byte nonce; the nonce and ciphertext are stored together in
//! the `credentials` table (see [`crate::Store`]). Decryption with the wrong key
//! — or against tampered ciphertext — **fails closed**: it returns a
//! [`StoreError`] rather than panicking or yielding plaintext.
//!
//! The vault is deliberately *separate* from the workflow IR and run history:
//! persisted workflows and run records never contain decrypted secrets.

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, KeyInit, Nonce};
use base64::Engine as _;

/// AES-256-GCM nonce length in bytes.
const NONCE_LEN: usize = 12;

use crate::{Store, StoreError};

/// An AES-256-GCM credential vault keyed by a 32-byte master key.
///
/// Clone is intentionally *not* derived to discourage copying key material
/// around; share a `&Vault` instead.
pub struct Vault {
    key: Key<Aes256Gcm>,
}

impl Vault {
    /// Construct a vault from raw 32-byte key material.
    #[must_use]
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key: Key::<Aes256Gcm>::from(key),
        }
    }

    /// Construct a vault from the `A2W_MASTER_KEY` environment variable, which
    /// must be base64 of **exactly** 32 bytes.
    ///
    /// # Errors
    /// Returns [`StoreError::Config`] if the variable is unset, not valid
    /// base64, or does not decode to exactly 32 bytes.
    pub fn from_env() -> Result<Self, StoreError> {
        let raw = std::env::var("A2W_MASTER_KEY")
            .map_err(|_| StoreError::Config("A2W_MASTER_KEY is not set".to_string()))?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(raw.trim())?;
        let key: [u8; 32] = bytes.try_into().map_err(|v: Vec<u8>| {
            StoreError::Config(format!(
                "A2W_MASTER_KEY must decode to exactly 32 bytes, got {}",
                v.len()
            ))
        })?;
        Ok(Self::new(key))
    }

    /// Encrypt `plaintext` and upsert it into the `credentials` table under
    /// `id` (with display `name`), using a fresh random 12-byte nonce.
    ///
    /// # Errors
    /// Returns [`StoreError::Crypto`] if encryption fails, or
    /// [`StoreError::Sqlx`] if the write fails.
    pub async fn store_secret(
        &self,
        store: &Store,
        id: &str,
        name: &str,
        plaintext: &str,
    ) -> Result<(), StoreError> {
        let cipher = Aes256Gcm::new(&self.key);
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| StoreError::Crypto(format!("encryption failed: {e}")))?;

        let created_at = crate::now_unix_seconds();
        sqlx::query(
            "INSERT INTO credentials (id, name, nonce, ciphertext, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(id) DO UPDATE SET \
               name = excluded.name, \
               nonce = excluded.nonce, \
               ciphertext = excluded.ciphertext, \
               created_at = excluded.created_at",
        )
        .bind(id)
        .bind(name)
        .bind(nonce.as_slice())
        .bind(ciphertext.as_slice())
        .bind(created_at)
        .execute(&store.pool)
        .await?;
        Ok(())
    }

    /// Load and decrypt the secret stored under `id`.
    ///
    /// Returns `Ok(None)` if no row exists for `id`.
    ///
    /// # Errors
    /// Returns [`StoreError::Crypto`] if decryption fails — i.e. the master key
    /// is wrong or the stored nonce/ciphertext was tampered with. This **never**
    /// returns plaintext for a failed decryption.
    pub async fn get_secret(&self, store: &Store, id: &str) -> Result<Option<String>, StoreError> {
        let row: Option<(Vec<u8>, Vec<u8>)> =
            sqlx::query_as("SELECT nonce, ciphertext FROM credentials WHERE id = ?1")
                .bind(id)
                .fetch_optional(&store.pool)
                .await?;

        let Some((nonce_bytes, ciphertext)) = row else {
            return Ok(None);
        };

        let nonce_arr: [u8; NONCE_LEN] = nonce_bytes.as_slice().try_into().map_err(|_| {
            StoreError::Crypto(format!(
                "stored nonce has wrong length: {} (expected {NONCE_LEN})",
                nonce_bytes.len()
            ))
        })?;

        let cipher = Aes256Gcm::new(&self.key);
        let nonce = Nonce::<<Aes256Gcm as AeadCore>::NonceSize>::from(nonce_arr);
        let plaintext = cipher
            .decrypt(&nonce, ciphertext.as_slice())
            .map_err(|_| StoreError::Crypto("decryption failed (wrong key or tampered data)".to_string()))?;

        let text = String::from_utf8(plaintext)
            .map_err(|e| StoreError::Crypto(format!("decrypted bytes are not valid UTF-8: {e}")))?;
        Ok(Some(text))
    }
}
