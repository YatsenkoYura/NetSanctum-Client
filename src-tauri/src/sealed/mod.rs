//! Sealed offline packages, transfer v1.
//!
//! Byte-for-byte mirror of the node's `app/core/crypto/transfer.py`: the same
//! ChaCha20-Poly1305 with 12-byte nonces (NOT XChaCha — `credentials.rs` uses
//! the X variant for the local vault, and grabbing that import here is the
//! easiest way to break interop), the same HKDF-SHA256 resource keys, the same
//! Argon2id KEK with the same NFC passphrase spelling and the same AAD.
//!
//! Discipline, because this module is where the snowden promise lives or dies:
//! ciphertext may rest on disk; plaintext exists only in the returned `Vec`s,
//! decrypt calls happen only on the gateway serve path, and the sealed structs
//! below never derive `Debug` — a leaked struct dump must not carry key matter.
//! Anything wrong at open time (wrong passphrase, tampered bytes, forged
//! manifest costs) is one [`AppError::SealedUnlock`], with no distinction.

use std::ops::Deref;

use argon2::{Algorithm, Argon2, Params, Version};
use base64::{Engine, engine::general_purpose::URL_SAFE as BASE64URL};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use hkdf::Hkdf;
use sha2::Sha256;
use unicode_normalization::UnicodeNormalization;
use zeroize::Zeroizing;

use crate::error::{AppError, AppResult};

/// `NSSEAL\x01\x00\x00`: chunked envelope magic, transfer version 1.
pub const SEALED_MAGIC: &[u8; 9] = b"NSSEAL\x01\x00\x00";
/// magic(9) | file nonce(8) | chunk size u32be | plaintext length u64be | chunk count u32be.
pub const CHUNKED_HEADER_SIZE: usize = 9 + 8 + 4 + 8 + 4;
pub const NONCE_SIZE: usize = 12;
pub const FILE_NONCE_SIZE: usize = 8;
pub const KEY_BYTES: usize = 32;
pub const SALT_BYTES: usize = 16;
pub const TAG_BYTES: usize = 16;

/// Reader bounds, checked before any derivation or allocation. Mirror the
/// node's ceilings exactly: anything outside is a forged manifest, refused
/// without touching Argon2.
const M_COST_MIN: u32 = 32 * 1024;
const M_COST_MAX: u32 = 256 * 1024;
const T_COST_MAX: u32 = 10;
const PARALLELISM_MAX: u32 = 4;

/// Chunk-size window the reader accepts. The writer always uses 1 MiB.
const CHUNK_SIZE_MIN: u32 = 1024;
const CHUNK_SIZE_MAX: u32 = 64 * 1024 * 1024;

/// Ciphertext ceiling for a small (whole) envelope. The node seals at most
/// 32 MiB of snapshot plaintext; the blob adds a 12-byte nonce and a 16-byte
/// tag. Checked before the allocation, never after.
pub const MAX_SMALL_BLOB_BYTES: u64 = 32 * 1024 * 1024 + (NONCE_SIZE + TAG_BYTES) as u64;

/// RAM ceiling for opening a chunked envelope whole (tests and tiny files).
/// The gateway never uses this path — it streams ranges from disk.
const MAX_IN_MEMORY_PLAINTEXT: u64 = 64 * 1024 * 1024;

const WRAP_AAD_PREFIX: &[u8] = b"ns:sealed-pkg:wrap:v1";
const RESOURCE_AAD_PREFIX: &[u8] = b"ns:sealed-pkg:res:v1";

/// A package DEK. The bytes zeroize on drop; the `Debug` impl below prints a
/// redaction, never the key.
#[derive(Clone)]
pub struct PackageKey(Zeroizing<[u8; KEY_BYTES]>);

impl PackageKey {
    fn new(bytes: [u8; KEY_BYTES]) -> Self {
        Self(Zeroizing::new(bytes))
    }
}

impl Deref for PackageKey {
    type Target = [u8; KEY_BYTES];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Debug for PackageKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PackageKey([redacted])")
    }
}

/// The manifest `sealing` object, validated. Carries ciphertext and public
/// parameters only — no key matter, but still no `Debug`: encrypted secrets
/// do not belong in logs either.
pub struct Sealing {
    m_cost: u32,
    t_cost: u32,
    parallelism: u32,
    salt: [u8; SALT_BYTES],
    wrapped_dek: Vec<u8>,
}

/// Parse and validate a manifest `sealing` object. Passphrase-independent:
/// forged costs are rejected here, at download time, before any derivation.
pub fn parse_sealing(value: &serde_json::Value) -> AppResult<Sealing> {
    let malformed = || AppError::InvalidPackage("malformed sealed package descriptor".into());
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(malformed)?;
    if version != 1 {
        return Err(AppError::SealedUnlock);
    }
    let kdf = value.get("kdf").ok_or_else(malformed)?;
    if kdf.get("algorithm").and_then(serde_json::Value::as_str) != Some("argon2id") {
        return Err(AppError::SealedUnlock);
    }
    let cost = |name: &str| {
        kdf.get(name)
            .and_then(serde_json::Value::as_u64)
            .and_then(|cost| u32::try_from(cost).ok())
            .ok_or_else(malformed)
    };
    let sealing = Sealing {
        m_cost: cost("m_cost")?,
        t_cost: cost("t_cost")?,
        parallelism: cost("parallelism")?,
        salt: decode_b64_exact(
            kdf.get("salt")
                .or_else(|| value.get("salt"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(malformed)?,
        )?,
        wrapped_dek: decode_b64(
            value
                .get("wrapped_dek")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(malformed)?,
        )?,
    };
    check_cost(sealing.m_cost, sealing.t_cost, sealing.parallelism)?;
    // Exactly nonce(12) + DEK(32) + tag(16): anything else is truncation.
    if sealing.wrapped_dek.len() != NONCE_SIZE + KEY_BYTES + TAG_BYTES {
        return Err(AppError::SealedUnlock);
    }
    if sealing.salt.len() != SALT_BYTES {
        return Err(AppError::SealedUnlock);
    }
    Ok(sealing)
}

fn check_cost(m_cost: u32, t_cost: u32, parallelism: u32) -> AppResult<()> {
    if !(M_COST_MIN..=M_COST_MAX).contains(&m_cost)
        || !(1..=T_COST_MAX).contains(&t_cost)
        || !(1..=PARALLELISM_MAX).contains(&parallelism)
    {
        return Err(AppError::SealedUnlock);
    }
    Ok(())
}

fn decode_b64(value: &str) -> AppResult<Vec<u8>> {
    BASE64URL.decode(value).map_err(|_| AppError::SealedUnlock)
}

fn decode_b64_exact<const N: usize>(value: &str) -> AppResult<[u8; N]> {
    let decoded = decode_b64(value)?;
    decoded.try_into().map_err(|_| AppError::SealedUnlock)
}

/// One spelling of a passphrase: NFC, then UTF-8 — exactly like the node.
/// Without this, `café` composed on Linux and decomposed on macOS derive
/// different KEKs from what the user believes is one passphrase.
fn passphrase_bytes(passphrase: &str) -> Vec<u8> {
    passphrase.nfc().collect::<String>().into_bytes()
}

fn derive_kek(
    passphrase: &str,
    salt: &[u8; SALT_BYTES],
    m_cost: u32,
    t_cost: u32,
    parallelism: u32,
) -> AppResult<Zeroizing<[u8; KEY_BYTES]>> {
    if passphrase.is_empty() {
        return Err(AppError::SealedUnlock);
    }
    check_cost(m_cost, t_cost, parallelism)?;
    let params = Params::new(m_cost, t_cost, parallelism, Some(KEY_BYTES))
        .map_err(|_| AppError::SealedUnlock)?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0_u8; KEY_BYTES]);
    argon2
        .hash_password_into(&passphrase_bytes(passphrase), salt, key.as_mut())
        .map_err(|_| AppError::SealedUnlock)?;
    Ok(key)
}

/// `{"m_cost":M,"parallelism":P,"t_cost":T}` — sorted keys, compact
/// separators. The node builds this with `json.dumps(sort_keys=True)`; any
/// other spelling is a different AAD and a failed open.
fn canonical_cost(m_cost: u32, t_cost: u32, parallelism: u32) -> Vec<u8> {
    format!(r#"{{"m_cost":{m_cost},"parallelism":{parallelism},"t_cost":{t_cost}}}"#).into_bytes()
}

fn wrap_aad(package_id: &str, sealing: &Sealing) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(WRAP_AAD_PREFIX.len() + 1 + package_id.len() + 1 + 64 + 1 + SALT_BYTES);
    aad.extend_from_slice(WRAP_AAD_PREFIX);
    aad.push(b'|');
    aad.extend_from_slice(package_id.as_bytes());
    aad.push(b'|');
    aad.extend_from_slice(&canonical_cost(
        sealing.m_cost,
        sealing.t_cost,
        sealing.parallelism,
    ));
    aad.push(b'|');
    aad.extend_from_slice(&sealing.salt);
    aad
}

/// Recover the package DEK from a manifest `sealing` object. One error for
/// everything: wrong passphrase and tampered bytes are indistinguishable.
pub fn unwrap_package_dek(
    sealing: &Sealing,
    passphrase: &str,
    package_id: &str,
) -> AppResult<PackageKey> {
    if package_id.is_empty() {
        return Err(AppError::SealedUnlock);
    }
    let kek = derive_kek(
        passphrase,
        &sealing.salt,
        sealing.m_cost,
        sealing.t_cost,
        sealing.parallelism,
    )?;
    let aad = wrap_aad(package_id, sealing);
    let (nonce, ciphertext) = sealing.wrapped_dek.split_at(NONCE_SIZE);
    let cipher =
        ChaCha20Poly1305::new_from_slice(kek.as_ref()).map_err(|_| AppError::SealedUnlock)?;
    let dek = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| AppError::SealedUnlock)?;
    let bytes: [u8; KEY_BYTES] = dek.try_into().map_err(|_| AppError::SealedUnlock)?;
    Ok(PackageKey::new(bytes))
}

/// The key for one resource, derived — never stored, never transmitted.
/// `info` is the exact manifest URL string, query included: `normalize_url`
/// in `nsp.rs` strips query keys for lookup, and must never feed this.
pub fn resource_key(
    dek: &[u8; KEY_BYTES],
    package_id: &str,
    url: &str,
) -> AppResult<[u8; KEY_BYTES]> {
    if dek.len() != KEY_BYTES || package_id.is_empty() || url.is_empty() {
        return Err(AppError::SealedUnlock);
    }
    let mut salt = Vec::with_capacity(RESOURCE_AAD_PREFIX.len() + package_id.len());
    salt.extend_from_slice(RESOURCE_AAD_PREFIX);
    salt.extend_from_slice(package_id.as_bytes());
    let hkdf = Hkdf::<Sha256>::new(Some(&salt), dek);
    let mut key = [0_u8; KEY_BYTES];
    hkdf.expand(url.as_bytes(), &mut key)
        .map_err(|_| AppError::SealedUnlock)?;
    Ok(key)
}

fn small_aad(url: &str, plaintext_len: u64) -> Vec<u8> {
    let mut aad = Vec::with_capacity(RESOURCE_AAD_PREFIX.len() + 1 + url.len() + 1 + 8);
    aad.extend_from_slice(RESOURCE_AAD_PREFIX);
    aad.push(b'|');
    aad.extend_from_slice(url.as_bytes());
    aad.push(b'|');
    aad.extend_from_slice(&plaintext_len.to_be_bytes());
    aad
}

/// Open a small sealed resource whole (the items snapshot). The length is
/// authenticated: truncation fails the tag, it does not yield a short value.
pub fn open_small(key: &[u8; KEY_BYTES], url: &str, blob: &[u8]) -> AppResult<Vec<u8>> {
    if key.len() != KEY_BYTES || url.is_empty() {
        return Err(AppError::SealedUnlock);
    }
    if blob.len() < NONCE_SIZE + TAG_BYTES {
        return Err(AppError::SealedUnlock);
    }
    if blob.len() as u64 > MAX_SMALL_BLOB_BYTES {
        return Err(AppError::InvalidPackage(
            "sealed snapshot exceeds the offline ceiling".into(),
        ));
    }
    let plaintext_len = (blob.len() - NONCE_SIZE - TAG_BYTES) as u64;
    let (nonce, ciphertext) = blob.split_at(NONCE_SIZE);
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| AppError::SealedUnlock)?;
    cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad: &small_aad(url, plaintext_len),
            },
        )
        .map_err(|_| AppError::SealedUnlock)
}

/// A parsed chunked-envelope header. Sizes are range-checked and the count
/// must agree with the sizes; authenticity is enforced per chunk at open.
#[derive(Clone, Copy, Debug)]
pub struct ChunkHeader {
    pub file_nonce: [u8; FILE_NONCE_SIZE],
    pub chunk_size: u32,
    pub plain_len: u64,
    pub count: u32,
}
pub fn parse_chunked_header(blob: &[u8]) -> AppResult<ChunkHeader> {
    if blob.len() < CHUNKED_HEADER_SIZE || blob[..SEALED_MAGIC.len()] != *SEALED_MAGIC {
        return Err(AppError::SealedUnlock);
    }
    let file_nonce: [u8; FILE_NONCE_SIZE] =
        blob[9..17].try_into().map_err(|_| AppError::SealedUnlock)?;
    let chunk_size = u32::from_be_bytes(
        blob[17..21]
            .try_into()
            .map_err(|_| AppError::SealedUnlock)?,
    );
    let plain_len = u64::from_be_bytes(
        blob[21..29]
            .try_into()
            .map_err(|_| AppError::SealedUnlock)?,
    );
    let count = u32::from_be_bytes(
        blob[29..33]
            .try_into()
            .map_err(|_| AppError::SealedUnlock)?,
    );
    if !(CHUNK_SIZE_MIN..=CHUNK_SIZE_MAX).contains(&chunk_size) {
        return Err(AppError::SealedUnlock);
    }
    let expected = if plain_len == 0 {
        0
    } else {
        plain_len
            .checked_add(u64::from(chunk_size) - 1)
            .and_then(|total| total.checked_div(u64::from(chunk_size)))
            .and_then(|count| u32::try_from(count).ok())
            .ok_or(AppError::SealedUnlock)?
    };
    if count != expected {
        return Err(AppError::SealedUnlock);
    }
    Ok(ChunkHeader {
        file_nonce,
        chunk_size,
        plain_len,
        count,
    })
}

/// The exact ciphertext length a header promises. Anything else on disk is
/// truncation or trailing garbage — refused before any chunk is opened.
pub fn promised_ciphertext_len(header: &ChunkHeader) -> AppResult<u64> {
    let chunk_size = u64::from(header.chunk_size);
    let full = u64::from(header.count)
        .checked_mul(chunk_size + TAG_BYTES as u64)
        .ok_or(AppError::SealedUnlock)?;
    let tail_saving = if header.plain_len == 0 {
        0
    } else {
        let last_plain = header.plain_len - (u64::from(header.count) - 1) * chunk_size;
        chunk_size - last_plain
    };
    (CHUNKED_HEADER_SIZE as u64)
        .checked_add(full)
        .and_then(|total| total.checked_sub(tail_saving))
        .ok_or(AppError::SealedUnlock)
}

fn chunk_aad(url: &str, header: &ChunkHeader) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(SEALED_MAGIC.len() + 1 + FILE_NONCE_SIZE + url.len() + 4 + 8 + 4);
    aad.extend_from_slice(SEALED_MAGIC);
    aad.push(0x00);
    aad.extend_from_slice(&header.file_nonce);
    aad.extend_from_slice(url.as_bytes());
    aad.extend_from_slice(&header.chunk_size.to_be_bytes());
    aad.extend_from_slice(&header.plain_len.to_be_bytes());
    aad.extend_from_slice(&header.count.to_be_bytes());
    aad
}

/// Open one stored chunk (`chunk_plain + 16` bytes) of an envelope whose
/// header was already parsed. The gateway's streaming path: each chunk is
/// read, tag-verified and yielded alone, so a gigabyte video never sits whole
/// in RAM on either end.
pub fn open_stored_chunk(
    key: &[u8; KEY_BYTES],
    url: &str,
    header: &ChunkHeader,
    index: u32,
    stored: &[u8],
) -> AppResult<Vec<u8>> {
    if key.len() != KEY_BYTES || url.is_empty() {
        return Err(AppError::SealedUnlock);
    }
    if header.count == 0 || u64::from(index) >= u64::from(header.count) {
        return Err(AppError::SealedUnlock);
    }
    let chunk_size = u64::from(header.chunk_size);
    let is_last = (u64::from(index) + 1) * chunk_size >= header.plain_len;
    let expected_plain = if is_last {
        header.plain_len - u64::from(index) * chunk_size
    } else {
        chunk_size
    };
    if stored.len() as u64 != expected_plain + TAG_BYTES as u64 {
        return Err(AppError::SealedUnlock);
    }
    let aad = chunk_aad(url, header);
    open_chunk(key, &aad, header, index, stored)
}

fn open_chunk(
    key: &[u8; KEY_BYTES],
    aad_base: &[u8],
    header: &ChunkHeader,
    index: u32,
    stored: &[u8],
) -> AppResult<Vec<u8>> {
    let index_bytes = index.to_be_bytes();
    let mut nonce = [0_u8; NONCE_SIZE];
    nonce[..FILE_NONCE_SIZE].copy_from_slice(&header.file_nonce);
    nonce[FILE_NONCE_SIZE..].copy_from_slice(&index_bytes);
    let mut aad = Vec::with_capacity(aad_base.len() + 4);
    aad.extend_from_slice(aad_base);
    aad.extend_from_slice(&index_bytes);
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| AppError::SealedUnlock)?;
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: stored,
                aad: &aad,
            },
        )
        .map_err(|_| AppError::SealedUnlock)
}

/// Yield plaintext `[start, start+length)` from a chunked envelope held in
/// RAM. Only the covered chunks are opened.
//
// Same note as `open_chunked_whole`: in-memory API for tests and tiny files,
// not the serving path.
#[allow(dead_code)]
pub fn open_chunked_range(
    key: &[u8; KEY_BYTES],
    url: &str,
    blob: &[u8],
    start: u64,
    length: u64,
) -> AppResult<Vec<u8>> {
    if key.len() != KEY_BYTES || url.is_empty() {
        return Err(AppError::SealedUnlock);
    }
    if length == 0 {
        return Ok(Vec::new());
    }
    let header = parse_chunked_header(blob)?;
    if blob.len() as u64 != promised_ciphertext_len(&header)? {
        return Err(AppError::SealedUnlock);
    }
    let aad = chunk_aad(url, &header);
    let wanted_end = start
        .checked_add(length)
        .map(|end| end.min(header.plain_len))
        .ok_or(AppError::SealedUnlock)?;
    if header.plain_len != 0 && start >= header.plain_len {
        return Err(AppError::SealedUnlock);
    }
    if start >= wanted_end {
        return Ok(Vec::new());
    }
    let chunk_size = u64::from(header.chunk_size);
    let mut out = Vec::new();
    let mut index = u32::try_from(start / chunk_size).map_err(|_| AppError::SealedUnlock)?;
    let mut position = u64::from(index) * chunk_size;
    while position < wanted_end {
        let is_last = (u64::from(index) + 1) * chunk_size >= header.plain_len;
        let chunk_plain = if is_last && header.plain_len != 0 {
            header.plain_len - u64::from(index) * chunk_size
        } else {
            chunk_size
        };
        let stored_at =
            CHUNKED_HEADER_SIZE as u64 + u64::from(index) * (chunk_size + TAG_BYTES as u64);
        let stored_end = stored_at
            .checked_add(chunk_plain + TAG_BYTES as u64)
            .ok_or(AppError::SealedUnlock)?;
        let stored = blob
            .get(stored_at as usize..stored_end as usize)
            .ok_or(AppError::SealedUnlock)?;
        let plaintext = open_chunk(key, &aad, &header, index, stored)?;
        if plaintext.len() as u64 != chunk_plain {
            return Err(AppError::SealedUnlock);
        }
        let trim = start.saturating_sub(position) as usize;
        let keep = (wanted_end - position.max(start)) as usize;
        out.extend_from_slice(&plaintext[trim..trim + keep.min(plaintext.len() - trim)]);
        position += chunk_plain;
        index = index.checked_add(1).ok_or(AppError::SealedUnlock)?;
    }
    Ok(out)
}

/// Open a whole chunked envelope held in RAM. Refuses anything above the
/// in-memory ceiling — the gateway streams from disk instead.
//
// Kept beside the streaming path as the module's in-memory API and exercised
// by the node-vector tests; production serving never calls it.
#[allow(dead_code)]
pub fn open_chunked_whole(key: &[u8; KEY_BYTES], url: &str, blob: &[u8]) -> AppResult<Vec<u8>> {
    let header = parse_chunked_header(blob)?;
    if header.plain_len > MAX_IN_MEMORY_PLAINTEXT {
        return Err(AppError::InvalidPackage(
            "sealed media is too large to open whole".into(),
        ));
    }
    open_chunked_range(key, url, blob, 0, header.plain_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_decode(even_hex: &str) -> Vec<u8> {
        let bytes = even_hex.as_bytes();
        let mut out = Vec::with_capacity(bytes.len() / 2);
        let mut index = 0;
        while index + 1 < bytes.len() {
            let pair = &even_hex[index..index + 2];
            if let Ok(byte) = u8::from_str_radix(pair, 16) {
                out.push(byte);
            }
            index += 2;
        }
        out
    }

    const VECTORS: &str = include_str!("vectors.json");

    fn vectors() -> serde_json::Value {
        serde_json::from_str(VECTORS).expect("vectors.json must parse")
    }

    fn hex_value(value: &serde_json::Value, key: &str) -> Vec<u8> {
        hex_decode(value.get(key).and_then(serde_json::Value::as_str).unwrap())
    }

    #[test]
    fn unwraps_package_dek_from_node_vectors() {
        let vectors = vectors();
        let wrap = vectors.get("wrap").unwrap();
        let package_id = vectors.get("package_id").unwrap().as_str().unwrap();
        let sealing = parse_sealing(wrap.get("fragment").unwrap()).unwrap();
        let passphrase = wrap.get("passphrase").unwrap().as_str().unwrap();
        let dek = unwrap_package_dek(&sealing, passphrase, package_id).unwrap();
        assert_eq!(hex_value(&vectors, "dek_hex"), dek.to_vec());
    }

    #[test]
    fn derives_identical_resource_keys() {
        let vectors = vectors();
        let dek: [u8; KEY_BYTES] = hex_value(&vectors, "dek_hex").try_into().unwrap();
        let package_id = vectors.get("package_id").unwrap().as_str().unwrap();
        for section in ["small", "chunk"] {
            let entry = vectors.get(section).unwrap();
            let url = entry.get("url").unwrap().as_str().unwrap();
            assert_eq!(
                hex_value(entry, "key_hex"),
                resource_key(&dek, package_id, url).unwrap().to_vec()
            );
        }
    }

    #[test]
    fn opens_small_envelope_from_node_vectors() {
        let vectors = vectors();
        let entry = vectors.get("small").unwrap();
        let key: [u8; KEY_BYTES] = hex_value(entry, "key_hex").try_into().unwrap();
        let url = entry.get("url").unwrap().as_str().unwrap();
        let blob = hex_decode(entry.get("blob_hex").unwrap().as_str().unwrap());
        assert_eq!(
            hex_value(entry, "plaintext_hex"),
            open_small(&key, url, &blob).unwrap()
        );
    }

    #[test]
    fn opens_chunked_ranges_from_node_vectors() {
        let vectors = vectors();
        let entry = vectors.get("chunk").unwrap();
        let key: [u8; KEY_BYTES] = hex_value(entry, "key_hex").try_into().unwrap();
        let url = entry.get("url").unwrap().as_str().unwrap();
        let blob = hex_decode(entry.get("blob_hex").unwrap().as_str().unwrap());
        let plain_len = entry.get("plain_len").unwrap().as_u64().unwrap();
        assert_eq!(
            blob.len() as u64,
            promised_ciphertext_len(&parse_chunked_header(&blob).unwrap()).unwrap()
        );
        assert_eq!(plain_len, parse_chunked_header(&blob).unwrap().plain_len);
        let range = entry.get("range").unwrap().as_array().unwrap();
        let (start, length) = (range[0].as_u64().unwrap(), range[1].as_u64().unwrap());
        assert_eq!(
            hex_value(entry, "range_hex"),
            open_chunked_range(&key, url, &blob, start, length).unwrap()
        );
        // The whole span, streaming chunk by chunk, reassembles exactly.
        assert_eq!(
            open_chunked_whole(&key, url, &blob).unwrap().len() as u64,
            plain_len
        );
        assert_eq!(
            open_chunked_range(&key, url, &blob, 0, plain_len).unwrap(),
            open_chunked_whole(&key, url, &blob).unwrap()
        );
    }

    #[test]
    fn nfc_variant_of_passphrase_opens() {
        let vectors = vectors();
        let entry = vectors.get("nfc").unwrap();
        let package_id = vectors.get("package_id").unwrap().as_str().unwrap();
        let sealing = parse_sealing(entry.get("fragment").unwrap()).unwrap();
        // Sealed under NFD, opened with the NFC spelling: one passphrase.
        let dek = unwrap_package_dek(
            &sealing,
            entry.get("nfc").unwrap().as_str().unwrap(),
            package_id,
        )
        .unwrap();
        assert_eq!(hex_value(&vectors, "dek_hex"), dek.to_vec());
    }

    #[test]
    fn every_failure_is_one_indistinguishable_error() {
        let vectors = vectors();
        let wrap = vectors.get("wrap").unwrap();
        let package_id = vectors.get("package_id").unwrap().as_str().unwrap();
        let fragment = wrap.get("fragment").unwrap();
        let sealing = parse_sealing(fragment).unwrap();
        let wrongs = [
            // Wrong passphrase.
            unwrap_package_dek(&sealing, "wrong passphrase here", package_id),
            // Wrong package binding.
            unwrap_package_dek(
                &sealing,
                wrap.get("passphrase").unwrap().as_str().unwrap(),
                "vault_sealed_8",
            ),
        ];
        for wrong in wrongs {
            assert!(matches!(wrong, Err(AppError::SealedUnlock)), "{wrong:?}");
        }
        // Tampered wrapped bytes.
        let mut tampered = fragment.clone();
        let mut wrapped = tampered
            .get("wrapped_dek")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();
        wrapped.replace_range(..1, if &wrapped[..1] == "A" { "B" } else { "A" });
        tampered["wrapped_dek"] = serde_json::Value::String(wrapped);
        let tampered_sealing = parse_sealing(&tampered).unwrap();
        assert!(matches!(
            unwrap_package_dek(
                &tampered_sealing,
                wrap.get("passphrase").unwrap().as_str().unwrap(),
                package_id
            ),
            Err(AppError::SealedUnlock)
        ));
        // Tampered resource bytes and wrong resource URL.
        let entry = vectors.get("small").unwrap();
        let key: [u8; KEY_BYTES] = hex_value(entry, "key_hex").try_into().unwrap();
        let url = entry.get("url").unwrap().as_str().unwrap();
        let mut blob = hex_decode(entry.get("blob_hex").unwrap().as_str().unwrap());
        blob[20] ^= 1;
        assert!(matches!(
            open_small(&key, url, &blob),
            Err(AppError::SealedUnlock)
        ));
        let blob = hex_decode(entry.get("blob_hex").unwrap().as_str().unwrap());
        assert!(matches!(
            open_small(
                &key,
                "/api/vault/sealed/7/items?package_id=vault_sealed_8",
                &blob
            ),
            Err(AppError::SealedUnlock)
        ));
        // A normalized URL (query stripped, like nsp.rs does for lookup) is a
        // different key, not a shortcut: derivation must use the exact string.
        assert!(matches!(
            open_small(&key, "/api/vault/sealed/7/items", &blob),
            Err(AppError::SealedUnlock)
        ));
    }

    #[test]
    fn rejects_forged_costs_and_counts_without_deriving() {
        let vectors = vectors();
        let fragment = vectors
            .get("wrap")
            .unwrap()
            .get("fragment")
            .unwrap()
            .clone();
        let mut evil = fragment.clone();
        evil["kdf"]["m_cost"] = serde_json::Value::from(1024 * 1024);
        assert!(matches!(parse_sealing(&evil), Err(AppError::SealedUnlock)));
        let mut evil = fragment.clone();
        evil["kdf"]["parallelism"] = serde_json::Value::from(16);
        assert!(matches!(parse_sealing(&evil), Err(AppError::SealedUnlock)));
        let mut evil = fragment;
        evil["version"] = serde_json::Value::from(2);
        assert!(matches!(parse_sealing(&evil), Err(AppError::SealedUnlock)));

        let entry = vectors.get("chunk").unwrap();
        let mut blob = hex_decode(entry.get("blob_hex").unwrap().as_str().unwrap());
        // A header whose count disagrees with its sizes is refused.
        blob[31] ^= 1;
        assert!(matches!(
            parse_chunked_header(&blob),
            Err(AppError::SealedUnlock)
        ));
        // Trailing garbage after the promised bytes is refused.
        let mut blob = hex_decode(entry.get("blob_hex").unwrap().as_str().unwrap());
        blob.push(0);
        let key: [u8; KEY_BYTES] = hex_value(entry, "key_hex").try_into().unwrap();
        let url = entry.get("url").unwrap().as_str().unwrap();
        assert!(matches!(
            open_chunked_range(&key, url, &blob, 0, 100),
            Err(AppError::SealedUnlock)
        ));
        // Truncation is refused, not shortened.
        blob.pop();
        blob.pop();
        assert!(matches!(
            open_chunked_range(&key, url, &blob, 0, 100),
            Err(AppError::SealedUnlock)
        ));
    }

    #[test]
    fn package_key_hides_in_debug() {
        let key = PackageKey::new([7_u8; KEY_BYTES]);
        assert_eq!(format!("{key:?}"), "PackageKey([redacted])");
    }
}
