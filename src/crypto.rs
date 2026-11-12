//! Archive encryption: Argon2id turns the password into a key,
//! XChaCha20-Poly1305 encrypts and seals each block (data and index).
//!
//! Each block is encrypted with a random 24-byte nonce. The seal (Poly1305)
//! also covers the block's type, method, size and number: a block that is
//! modified, moved or swapped with another is rejected.

use anyhow::{anyhow, bail, Result};
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

use crate::format::{put_varint, BlockKind, Codec, Cursor};

const PARAMS_VERSION: u64 = 1;
const KDF_ARGON2ID: u64 = 1;
const CIPHER_XCHACHA20_POLY1305: u64 = 1;
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
/// Number used in the seal of an index block: its own position in the
/// archive, marked by the high bit so it never gets confused with a data
/// block number. A moved index layer is rejected.
pub fn index_number(offset: u64) -> u64 {
    (1 << 63) | offset
}

/// Default Argon2id: 64 MB of memory and 3 passes, about one second per
/// attempt. Unnoticeable for the user, ruinous for an attacker trying
/// billions of passwords.
const M_COST_KIB: u32 = 64 * 1024;
const T_COST: u32 = 3;
const P_COST: u32 = 4;

/// Encryption settings, stored in clear in the parameter block.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    pub m_cost_kib: u32,
    pub t_cost: u32,
    pub p_cost: u32,
    pub salt: [u8; 16],
    /// Check value: detects a wrong password without decrypting anything.
    pub check: [u8; 32],
}

impl Params {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        put_varint(&mut o, PARAMS_VERSION);
        put_varint(&mut o, KDF_ARGON2ID);
        put_varint(&mut o, u64::from(self.m_cost_kib));
        put_varint(&mut o, u64::from(self.t_cost));
        put_varint(&mut o, u64::from(self.p_cost));
        put_varint(&mut o, self.salt.len() as u64);
        o.extend_from_slice(&self.salt);
        put_varint(&mut o, CIPHER_XCHACHA20_POLY1305);
        o.extend_from_slice(&self.check);
        o
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        let mut c = Cursor::new(buf);
        if c.varint()? != PARAMS_VERSION {
            bail!("paramètres de chiffrement d'une version inconnue");
        }
        if c.varint()? != KDF_ARGON2ID {
            bail!("méthode de dérivation de clé inconnue");
        }
        let (m, t, p) = (c.varint()?, c.varint()?, c.varint()?);
        // Safety limits: a booby-trapped archive must not be able to demand more
        // than 1 GB of memory or minutes of computation to open.
        if !(8..=1024 * 1024).contains(&m) || !(1..=16).contains(&t) || !(1..=64).contains(&p) {
            bail!("paramètres de chiffrement hors limites");
        }
        if c.varint()? != 16 {
            bail!("sel de taille inattendue");
        }
        let salt: [u8; 16] = c.bytes(16)?.try_into().unwrap();
        if c.varint()? != CIPHER_XCHACHA20_POLY1305 {
            bail!("algorithme de chiffrement inconnu");
        }
        let check: [u8; 32] = c.bytes(32)?.try_into().unwrap();
        Ok(Self { m_cost_kib: m as u32, t_cost: t as u32, p_cost: p as u32, salt, check })
    }
}

#[derive(Clone)]
pub struct Crypto {
    aead: XChaCha20Poly1305,
}

/// Password -> (encryption key, check value).
fn derive(password: &str, params: &Params) -> Result<(Zeroizing<[u8; 32]>, [u8; 32])> {
    let argon_params = argon2::Params::new(params.m_cost_kib, params.t_cost, params.p_cost, Some(32))
        .map_err(|e| anyhow!("paramètres Argon2 invalides : {e}"))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, argon_params);
    let mut master = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password.as_bytes(), &params.salt, master.as_mut())
        .map_err(|e| anyhow!("dérivation de la clé impossible : {e}"))?;
    // Two independent subkeys: knowing the check value reveals nothing about
    // the encryption key.
    let key = Zeroizing::new(blake3::derive_key("CNZ v1 chiffrement des blocs", master.as_ref()));
    let check = blake3::derive_key("CNZ v1 verification du mot de passe", master.as_ref());
    Ok((key, check))
}

impl Crypto {
    /// New archive: random salt and key derived from the password.
    pub fn create(password: &str) -> Result<(Params, Self)> {
        if password.is_empty() {
            bail!("le mot de passe ne peut pas être vide");
        }
        let mut params = Params { m_cost_kib: M_COST_KIB, t_cost: T_COST, p_cost: P_COST, salt: [0; 16], check: [0; 32] };
        OsRng.fill_bytes(&mut params.salt);
        let (key, check) = derive(password, &params)?;
        params.check = check;
        Ok((params, Self { aead: XChaCha20Poly1305::new(Key::from_slice(key.as_ref())) }))
    }

    /// Existing archive: rejects a wrong password straight away.
    pub fn open(password: &str, params: &Params) -> Result<Self> {
        let (key, check) = derive(password, params)?;
        // Constant-time comparison (blake3::Hash).
        if blake3::Hash::from(check) != blake3::Hash::from(params.check) {
            bail!("mot de passe incorrect");
        }
        Ok(Self { aead: XChaCha20Poly1305::new(Key::from_slice(key.as_ref())) })
    }

    fn aad(kind: BlockKind, codec: Codec, raw_len: u64, number: u64) -> [u8; 18] {
        let mut a = [0u8; 18];
        a[0] = kind as u8;
        a[1] = codec as u8;
        a[2..10].copy_from_slice(&raw_len.to_le_bytes());
        a[10..18].copy_from_slice(&number.to_le_bytes());
        a
    }

    /// Encrypts and seals: returns nonce + encrypted data + seal.
    pub fn seal(&self, kind: BlockKind, codec: Codec, raw_len: u64, number: u64, plain: &[u8]) -> Result<Vec<u8>> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aad = Self::aad(kind, codec, raw_len, number);
        let sealed = self
            .aead
            .encrypt(&nonce, Payload { msg: plain, aad: &aad })
            .map_err(|_| anyhow!("échec du chiffrement"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Checks the seal and decrypts.
    pub fn unseal(&self, kind: BlockKind, codec: Codec, raw_len: u64, number: u64, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() < NONCE_LEN + TAG_LEN {
            bail!("bloc chiffré trop court");
        }
        let (nonce, sealed) = payload.split_at(NONCE_LEN);
        let aad = Self::aad(kind, codec, raw_len, number);
        self.aead
            .decrypt(XNonce::from_slice(nonce), Payload { msg: sealed, aad: &aad })
            .map_err(|_| anyhow!("sceau invalide : bloc altéré ou déplacé"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sceller_puis_ouvrir() {
        let (params, c) = Crypto::create("secret").unwrap();
        let sealed = c.seal(BlockKind::Data, Codec::Zstd, 5, 3, b"hello").unwrap();
        assert_eq!(c.unseal(BlockKind::Data, Codec::Zstd, 5, 3, &sealed).unwrap(), b"hello");
        // Moved block (different number): rejected.
        assert!(c.unseal(BlockKind::Data, Codec::Zstd, 5, 4, &sealed).is_err());
        // Parameters read back from their binary form, right and wrong password.
        let p = Params::decode(&params.encode()).unwrap();
        assert!(Crypto::open("secret", &p).is_ok());
        assert!(Crypto::open("Secret", &p).is_err());
    }
}
