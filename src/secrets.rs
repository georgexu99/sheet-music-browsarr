use aes_gcm::aead::{Aead, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, KeyInit};
use anyhow::Context;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use hkdf::Hkdf;
use sha2::Sha256;

/// Secret-at-rest envelope. The env var `BROWSARR_SECRET_KEY` is the only key
/// material that ever leaves this process; everything in the SQLite `settings`
/// table marked `encrypted=1` is decryptable only with that env var.
///
/// Format on disk: base64( nonce(12) || ciphertext+tag ).
#[derive(Clone)]
pub struct Secrets {
    cipher: Aes256Gcm,
}

impl Secrets {
    pub fn new(env_key: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            env_key.len() >= 16,
            "BROWSARR_SECRET_KEY must be at least 16 characters"
        );
        let hk = Hkdf::<Sha256>::new(None, env_key.as_bytes());
        let mut key = [0u8; 32];
        hk.expand(b"sheet-music-browsarr/secret-encryption", &mut key)
            .map_err(|e| anyhow::anyhow!("hkdf expand: {e}"))?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| anyhow::anyhow!("aes-gcm init: {e}"))?;
        Ok(Self { cipher })
    }

    pub fn encrypt(&self, plaintext: &str) -> anyhow::Result<String> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = self
            .cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| anyhow::anyhow!("encrypt: {e}"))?;
        let mut out = Vec::with_capacity(12 + ct.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(B64.encode(&out))
    }

    pub fn decrypt(&self, b64: &str) -> anyhow::Result<String> {
        let raw = B64.decode(b64).context("base64 decode")?;
        anyhow::ensure!(raw.len() > 12, "ciphertext too short");
        let (nonce_bytes, ct) = raw.split_at(12);
        let nonce = aes_gcm::Nonce::from_slice(nonce_bytes);
        let pt = self
            .cipher
            .decrypt(nonce, ct)
            .map_err(|e| anyhow::anyhow!("decrypt: {e}"))?;
        String::from_utf8(pt).context("utf-8 from decrypted bytes")
    }
}
