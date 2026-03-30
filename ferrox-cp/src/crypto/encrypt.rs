use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;

use crate::error::CpError;

const NONCE_LEN: usize = 12;

/// Encrypt `key_bytes` with AES-256-GCM.
///
/// The returned blob format is: `[12-byte random nonce][ciphertext+tag]`.
/// The plaintext bytes are never written to disk or logged.
pub fn encrypt_private_key(key_bytes: &[u8], encryption_key: &[u8; 32]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(encryption_key));

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, key_bytes)
        .expect("AES-256-GCM encryption should not fail on valid inputs");

    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    blob
}

/// Decrypt a blob produced by [`encrypt_private_key`].
///
/// Returns [`CpError::Decryption`] if the nonce is missing, the tag does not
/// authenticate (tampered blob), or any other decryption failure occurs.
pub fn decrypt_private_key(blob: &[u8], encryption_key: &[u8; 32]) -> Result<Vec<u8>, CpError> {
    if blob.len() < NONCE_LEN {
        return Err(CpError::Decryption(
            "blob is too short to contain a nonce".to_string(),
        ));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(encryption_key));
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher.decrypt(nonce, ciphertext).map_err(|_| {
        CpError::Decryption("authentication tag mismatch — blob may be tampered".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = test_key();
        let plaintext = b"super secret RSA private key bytes";
        let blob = encrypt_private_key(plaintext, &key);
        let recovered = decrypt_private_key(&blob, &key).expect("decryption must succeed");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn different_encryptions_of_same_plaintext_produce_different_blobs() {
        // Each call uses a fresh random nonce so the output must differ.
        let key = test_key();
        let plaintext = b"same plaintext";
        let blob1 = encrypt_private_key(plaintext, &key);
        let blob2 = encrypt_private_key(plaintext, &key);
        assert_ne!(blob1, blob2, "nonce must be randomised on every call");
    }

    #[test]
    fn tampered_blob_returns_error() {
        let key = test_key();
        let mut blob = encrypt_private_key(b"private key data", &key);
        // Flip a byte in the ciphertext portion (past the 12-byte nonce).
        blob[NONCE_LEN] ^= 0xFF;
        let result = decrypt_private_key(&blob, &key);
        assert!(
            result.is_err(),
            "tampered blob must not decrypt successfully"
        );
    }

    #[test]
    fn wrong_key_returns_error() {
        let key1 = test_key();
        let mut key2 = test_key();
        key2[0] ^= 0x01;
        let blob = encrypt_private_key(b"secret", &key1);
        let result = decrypt_private_key(&blob, &key2);
        assert!(result.is_err(), "wrong key must not decrypt successfully");
    }

    #[test]
    fn blob_too_short_returns_error() {
        let key = test_key();
        let short_blob = vec![0u8; NONCE_LEN - 1];
        let result = decrypt_private_key(&short_blob, &key);
        assert!(result.is_err());
    }
}
