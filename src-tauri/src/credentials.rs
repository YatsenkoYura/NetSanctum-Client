use std::fs;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};

const VAULT_VERSION: u8 = 1;
const SALT_LENGTH: usize = 16;
const NONCE_LENGTH: usize = 24;
const KEY_LENGTH: usize = 32;
const MEMORY_KIB: u32 = 65_536;
const ITERATIONS: u32 = 3;
const PARALLELISM: u32 = 1;
const MIN_PASSWORD_CHARS: usize = 10;
const ASSOCIATED_DATA: &[u8] = b"Netsanctum Client credential vault v1";
const LEGACY_ASSOCIATED_DATA: &[u8] = b"NetSanctum Desktop credential vault v1";

#[derive(Debug, Deserialize, Serialize)]
struct VaultEnvelope {
    version: u8,
    kdf: KdfDescriptor,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct KdfDescriptor {
    algorithm: String,
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("credentials.vault"),
        }
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    pub fn read(&self, password: &str) -> AppResult<Zeroizing<String>> {
        if !self.exists() {
            return Err(AppError::VaultMissing);
        }
        let payload = fs::read(&self.path).map_err(|error| AppError::Storage(error.to_string()))?;
        if payload.len() > 64 * 1024 {
            return Err(AppError::Storage(
                "файл credentials.vault слишком большой".into(),
            ));
        }
        let envelope: VaultEnvelope = serde_json::from_slice(&payload)
            .map_err(|error| AppError::Storage(format!("повреждён credentials.vault: {error}")))?;
        validate_envelope(&envelope)?;

        let salt = decode_exact::<SALT_LENGTH>(&envelope.salt, "salt")?;
        let nonce = decode_exact::<NONCE_LENGTH>(&envelope.nonce, "nonce")?;
        let ciphertext = BASE64
            .decode(&envelope.ciphertext)
            .map_err(|_| AppError::Storage("некорректный ciphertext в credentials.vault".into()))?;
        if ciphertext.len() > 16 * 1024 {
            return Err(AppError::Storage(
                "ciphertext в credentials.vault слишком большой".into(),
            ));
        }

        let key = derive_key(password, &salt, &envelope.kdf)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| AppError::Internal("не удалось создать vault cipher".into()))?;
        let decrypted = cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: ASSOCIATED_DATA,
                },
            )
            .or_else(|_| {
                cipher.decrypt(
                    XNonce::from_slice(&nonce),
                    Payload {
                        msg: &ciphertext,
                        aad: LEGACY_ASSOCIATED_DATA,
                    },
                )
            })
            .map_err(|_| AppError::InvalidVaultPassword)?;
        let plaintext = Zeroizing::new(decrypted);
        let token = std::str::from_utf8(plaintext.as_ref())
            .map_err(|_| AppError::Storage("vault содержит некорректный UTF-8".into()))?
            .to_owned();
        if token.is_empty() {
            return Err(AppError::Storage("vault содержит пустой токен".into()));
        }
        Ok(Zeroizing::new(token))
    }

    pub fn save(&self, secret: &str, password: &str) -> AppResult<()> {
        validate_new_password(password)?;
        if secret.is_empty() {
            return Err(AppError::EmptyMasterToken);
        }

        let mut salt = [0_u8; SALT_LENGTH];
        let mut nonce = [0_u8; NONCE_LENGTH];
        getrandom::fill(&mut salt).map_err(|error| AppError::Internal(error.to_string()))?;
        getrandom::fill(&mut nonce).map_err(|error| AppError::Internal(error.to_string()))?;
        let kdf = KdfDescriptor {
            algorithm: "argon2id".into(),
            memory_kib: MEMORY_KIB,
            iterations: ITERATIONS,
            parallelism: PARALLELISM,
        };
        let key = derive_key(password, &salt, &kdf)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| AppError::Internal("не удалось создать vault cipher".into()))?;
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: secret.as_bytes(),
                    aad: ASSOCIATED_DATA,
                },
            )
            .map_err(|_| AppError::Internal("не удалось зашифровать credentials vault".into()))?;
        let envelope = VaultEnvelope {
            version: VAULT_VERSION,
            kdf,
            salt: BASE64.encode(salt),
            nonce: BASE64.encode(nonce),
            ciphertext: BASE64.encode(ciphertext),
        };
        let payload = serde_json::to_vec_pretty(&envelope)
            .map_err(|error| AppError::Storage(error.to_string()))?;
        crate::atomic_file::write(&self.path, &payload)
    }

    pub fn validate_new_password(&self, password: &str) -> AppResult<()> {
        validate_new_password(password)
    }

    pub fn clear(&self) -> AppResult<()> {
        if self.path.exists() {
            fs::remove_file(&self.path).map_err(|error| AppError::Storage(error.to_string()))?;
        }
        Ok(())
    }
}

fn validate_new_password(password: &str) -> AppResult<()> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(AppError::WeakVaultPassword);
    }
    Ok(())
}

fn validate_envelope(envelope: &VaultEnvelope) -> AppResult<()> {
    if envelope.version != VAULT_VERSION || envelope.kdf.algorithm != "argon2id" {
        return Err(AppError::Storage(
            "неподдерживаемая версия credentials.vault".into(),
        ));
    }
    if !(32_768..=262_144).contains(&envelope.kdf.memory_kib)
        || !(1..=10).contains(&envelope.kdf.iterations)
        || !(1..=4).contains(&envelope.kdf.parallelism)
    {
        return Err(AppError::Storage(
            "небезопасные или чрезмерные параметры vault KDF".into(),
        ));
    }
    Ok(())
}

fn derive_key(
    password: &str,
    salt: &[u8; SALT_LENGTH],
    descriptor: &KdfDescriptor,
) -> AppResult<Zeroizing<[u8; KEY_LENGTH]>> {
    let params = Params::new(
        descriptor.memory_kib,
        descriptor.iterations,
        descriptor.parallelism,
        Some(KEY_LENGTH),
    )
    .map_err(|error| AppError::Storage(error.to_string()))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; KEY_LENGTH]);
    argon2
        .hash_password_into(password.as_bytes(), salt, key.as_mut())
        .map_err(|error| AppError::Storage(error.to_string()))?;
    Ok(key)
}

fn decode_exact<const N: usize>(encoded: &str, field: &str) -> AppResult<[u8; N]> {
    let decoded = BASE64
        .decode(encoded)
        .map_err(|_| AppError::Storage(format!("некорректный {field} в credentials.vault")))?;
    decoded
        .try_into()
        .map_err(|_| AppError::Storage(format!("неверная длина {field} в credentials.vault")))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::CredentialStore;
    use crate::error::AppError;

    #[test]
    fn vault_round_trip_and_wrong_password_rejection() {
        let directory = tempdir().unwrap();
        let vault = CredentialStore::new(directory.path());
        vault
            .save("master-token-value", "correct horse battery")
            .unwrap();

        assert_eq!(
            vault.read("correct horse battery").unwrap().as_str(),
            "master-token-value"
        );
        assert!(matches!(
            vault.read("incorrect password"),
            Err(AppError::InvalidVaultPassword)
        ));
    }

    #[test]
    fn rejects_short_vault_password() {
        let directory = tempdir().unwrap();
        let vault = CredentialStore::new(directory.path());
        assert!(matches!(
            vault.save("master-token-value", "short"),
            Err(AppError::WeakVaultPassword)
        ));
    }

    #[test]
    fn decrypts_legacy_vault_with_previous_associated_data() {
        use super::{
            BASE64, KdfDescriptor, LEGACY_ASSOCIATED_DATA, NONCE_LENGTH, SALT_LENGTH,
            VAULT_VERSION, VaultEnvelope, derive_key,
        };
        use base64::Engine;
        use chacha20poly1305::{
            KeyInit, XChaCha20Poly1305, XNonce,
            aead::{Aead, Payload},
        };

        let directory = tempdir().unwrap();
        let salt = [1_u8; SALT_LENGTH];
        let nonce = [2_u8; NONCE_LENGTH];
        let kdf = KdfDescriptor {
            algorithm: "argon2id".into(),
            memory_kib: 32_768,
            iterations: 1,
            parallelism: 1,
        };
        let key = derive_key("legacy-password-1234", &salt, &kdf).unwrap();
        let cipher = XChaCha20Poly1305::new_from_slice(key.as_ref()).unwrap();
        let ciphertext = cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: b"legacy-token-data",
                    aad: LEGACY_ASSOCIATED_DATA,
                },
            )
            .unwrap();
        let envelope = VaultEnvelope {
            version: VAULT_VERSION,
            kdf,
            salt: BASE64.encode(salt),
            nonce: BASE64.encode(nonce),
            ciphertext: BASE64.encode(ciphertext),
        };
        let payload = serde_json::to_vec_pretty(&envelope).unwrap();
        std::fs::write(directory.path().join("credentials.vault"), payload).unwrap();

        let vault = CredentialStore::new(directory.path());
        assert_eq!(
            vault.read("legacy-password-1234").unwrap().as_str(),
            "legacy-token-data"
        );
    }
}
