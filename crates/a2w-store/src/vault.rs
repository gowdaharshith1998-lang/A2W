//! The credential vault: **AES-256-GCM envelope encryption** for secrets.
//!
//! A [`Vault`] holds a 32-byte master key. Each secret is encrypted under a
//! fresh random 12-byte nonce; the nonce and ciphertext are stored together in
//! the `credentials` table (see [`crate::Store`]). Decryption with the wrong
//! key — or against tampered ciphertext — **fails closed**: it returns a
//! [`StoreError`] rather than panicking or yielding plaintext.
//!
//! The vault is deliberately *separate* from the workflow IR and run history:
//! persisted workflows and run records never contain decrypted secrets.
//!
//! ## Key-material hygiene (audit-fix)
//! - The master key is wrapped in a [`Zeroizing`] newtype so it is wiped on
//!   drop (catches the swap / coredump exfil path).
//! - `aes-gcm` is built with the `zeroize` feature so the round-key schedule
//!   is wiped when the cipher instance is dropped.
//! - [`Vault::from_env`] rejects obviously weak keys (all-zero, all-equal byte)
//!   so a missing/empty `A2W_MASTER_KEY` in a Kubernetes secret does not
//!   silently encrypt under a publicly-guessable key.

use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, KeyInit, Nonce};
use base64::Engine as _;
use zeroize::{Zeroize, Zeroizing};

/// AES-256-GCM nonce length in bytes.
const NONCE_LEN: usize = 12;
/// Master key length in bytes.
const KEY_LEN: usize = 32;

use crate::{Store, StoreError};

/// An AES-256-GCM credential vault keyed by a 32-byte master key.
///
/// Clone is intentionally *not* derived to discourage copying key material
/// around; share a `&Vault` (or wrap in `Arc<Vault>`) instead.
pub struct Vault {
    /// The master key. `Zeroizing` wipes the buffer on drop so a coredump /
    /// memory-scrape attacker cannot recover it after the process exits.
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl Vault {
    /// Construct a vault from raw 32-byte key material, **rejecting obviously
    /// weak keys** (all-equal byte). This is the audit-3 fix for "Vault::new
    /// bypasses weak-key rejection".
    ///
    /// # Errors
    /// [`StoreError::Config`] when the key is obviously weak.
    pub fn try_new(key: [u8; KEY_LEN]) -> Result<Self, StoreError> {
        if is_weak_key(&key) {
            let mut k = key;
            k.zeroize();
            return Err(StoreError::Config(
                "master key is obviously weak (all-zero or single-byte repeating)".into(),
            ));
        }
        Ok(Self {
            key: Zeroizing::new(key),
        })
    }

    /// Construct a vault from raw 32-byte key material **without** the
    /// weak-key check. Reserved for tests that need a deterministic short
    /// fixture (`[7u8; 32]`); production code should use [`Vault::try_new`] or
    /// [`Vault::from_env`].
    #[must_use]
    #[doc(hidden)]
    pub fn new(key: [u8; KEY_LEN]) -> Self {
        Self {
            key: Zeroizing::new(key),
        }
    }

    /// Construct a vault from the `A2W_MASTER_KEY` environment variable, which
    /// must be base64 of **exactly** 32 bytes.
    ///
    /// **Weak-key rejection:** all-zero, all-`0xFF`, and any all-equal-byte
    /// 32-byte sequence is rejected with [`StoreError::Config`]. This catches
    /// the common deployment failure of an empty K8s secret or hand-written
    /// placeholder.
    ///
    /// # Errors
    /// Returns [`StoreError::Config`] if the variable is unset, not valid
    /// base64, does not decode to exactly 32 bytes, or is obviously weak.
    pub fn from_env() -> Result<Self, StoreError> {
        let raw = std::env::var("A2W_MASTER_KEY")
            .map_err(|_| StoreError::Config("A2W_MASTER_KEY is not set".to_string()))?;
        // Audit-2 fix: the env-var String is itself sensitive material (a
        // base64 encoding of the key, 1:1 recoverable). Wrap it in Zeroizing
        // so the buffer is wiped on every return path; ditto for the decoded
        // bytes and the temporary key array.
        let raw = Zeroizing::new(raw);
        let trimmed = raw.trim();
        let mut bytes = Zeroizing::new(base64::engine::general_purpose::STANDARD.decode(trimmed)?);

        if bytes.len() != KEY_LEN {
            let n = bytes.len();
            bytes.zeroize();
            return Err(StoreError::Config(format!(
                "A2W_MASTER_KEY must decode to exactly {KEY_LEN} bytes, got {n}"
            )));
        }

        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        key.copy_from_slice(&bytes);
        // Both `bytes` and `raw` zeroize on drop. `key` itself is wiped at
        // function-scope drop; the master copy moves into Vault::new and is
        // re-wrapped there.

        if is_weak_key(&key) {
            // key zeroizes on drop; explicit error.
            return Err(StoreError::Config(
                "A2W_MASTER_KEY is obviously weak (all-zero or single-byte \
                 repeating). Generate a strong key with `head -c 32 /dev/urandom \
                 | base64`."
                    .to_string(),
            ));
        }
        // Audit-2 fix: scrub the raw env var from the process environment
        // so a same-uid attacker (`cat /proc/$pid/environ`) cannot read it.
        // The Vault still holds the derived key in a Zeroizing wrapper.
        // SAFETY: process is single-threaded at this point in startup; we
        // only call this from main during init.
        std::env::remove_var("A2W_MASTER_KEY");

        Ok(Self::new(*key))
    }

    /// Borrow the key as the cipher expects.
    fn key_ref(&self) -> &Key<Aes256Gcm> {
        Key::<Aes256Gcm>::from_slice(self.key.as_ref())
    }

    /// List every stored credential as `(id, name, created_at)` triples, ordered
    /// by `id`. The plaintext secret is **never** returned by listing.
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a read failure.
    pub async fn list_credentials(store: &Store) -> Result<Vec<(String, String, i64)>, StoreError> {
        let rows: Vec<(String, String, i64)> =
            sqlx::query_as("SELECT id, name, created_at FROM credentials ORDER BY id")
                .fetch_all(&store.pool)
                .await?;
        Ok(rows)
    }

    /// Delete the credential under `id`. Deleting a missing id is a no-op (still
    /// `Ok`).
    ///
    /// # Errors
    /// [`StoreError::Sqlx`] on a write failure.
    pub async fn delete_credential(store: &Store, id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM credentials WHERE id = ?1")
            .bind(id)
            .execute(&store.pool)
            .await?;
        Ok(())
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
        let cipher = Aes256Gcm::new(self.key_ref());
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

    /// Load and decrypt the secret stored under `id`, returning a
    /// [`Zeroizing`]-wrapped plaintext String so the buffer is wiped when the
    /// caller drops it. Audit-3 fix for "decrypted plaintext returned as
    /// plain `String` — never zeroized after caller use".
    ///
    /// Returns `Ok(None)` if no row exists for `id`.
    ///
    /// # Errors
    /// Returns [`StoreError::Crypto`] if decryption fails. **Never** returns
    /// plaintext for a failed decryption.
    pub async fn get_secret_zeroizing(
        &self,
        store: &Store,
        id: &str,
    ) -> Result<Option<Zeroizing<String>>, StoreError> {
        match self.get_secret(store, id).await? {
            Some(s) => Ok(Some(Zeroizing::new(s))),
            None => Ok(None),
        }
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

        let cipher = Aes256Gcm::new(self.key_ref());
        let nonce = Nonce::<<Aes256Gcm as AeadCore>::NonceSize>::from(nonce_arr);
        let plaintext = cipher.decrypt(&nonce, ciphertext.as_slice()).map_err(|_| {
            StoreError::Crypto("decryption failed (wrong key or tampered data)".to_string())
        })?;

        let text = String::from_utf8(plaintext)
            .map_err(|e| StoreError::Crypto(format!("decrypted bytes are not valid UTF-8: {e}")))?;
        Ok(Some(text))
    }
}

/// Reject obviously weak master keys: all-zero, all-`0xFF`, or any all-equal
/// byte. (We do NOT attempt to detect every weak distribution — only the
/// failure modes seen in real-world misconfigurations.)
fn is_weak_key(key: &[u8; KEY_LEN]) -> bool {
    // All bytes identical → weak.
    let first = key[0];
    if key.iter().all(|b| *b == first) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_key_detector_catches_common_misconfigurations() {
        assert!(is_weak_key(&[0u8; 32]), "all-zero rejected");
        assert!(is_weak_key(&[0xFFu8; 32]), "all-0xff rejected");
        assert!(is_weak_key(&[0x41u8; 32]), "all-A rejected");
        let mut k = [0u8; 32];
        k[0] = 1;
        assert!(!is_weak_key(&k), "differing-byte key accepted");
    }

    #[test]
    fn from_env_rejects_all_zero_key_after_decode() {
        // Snapshot + restore so other tests sharing the process env are not
        // disturbed by this one.
        let saved = std::env::var("A2W_MASTER_KEY").ok();
        // 32 zero bytes base64-encoded: 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='
        std::env::set_var(
            "A2W_MASTER_KEY",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        );
        let r = Vault::from_env();
        assert!(
            matches!(r, Err(StoreError::Config(m)) if m.contains("weak")),
            "all-zero key must be rejected as weak"
        );
        match saved {
            Some(v) => std::env::set_var("A2W_MASTER_KEY", v),
            None => std::env::remove_var("A2W_MASTER_KEY"),
        }
    }
}
