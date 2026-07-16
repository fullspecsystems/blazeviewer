//! RAR5 encryption primitives: PBKDF2-HMAC-SHA256 key derivation and
//! AES-256-CBC decryption (task #103). Used by [`crate::rar`] for both
//! per-file encryption (`-p`) and full header encryption (`-hp`).
//!
//! RAR5's scheme is the *tractable* one — standard PBKDF2 + AES-CBC, no bespoke
//! cryptography — which is why RAR5 encryption is in scope where RAR4's custom
//! KDF is not. The derivation matches unrar's `crypt5.cpp`: from one password +
//! 16-byte salt + a cost exponent, a single PBKDF2 stream yields three values as
//! snapshots of the running XOR fold — the AES key at `2^count` iterations, a MAC
//! key at `+16`, and a password-check value at `+32`. All primitives are pure
//! Rust and already in the dependency tree (aes/cbc via `sevenz-rust2`, hmac/sha2
//! via the 7z + zip crypto), so this adds no build risk.

use std::collections::HashMap;

use aes::cipher::{array::Array, BlockModeDecrypt, KeyIvInit};
use aes::Aes256;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;
/// AES-256-CBC decryptor — held as running state while a solid group's per-member
/// runs are decrypted chunk by chunk.
pub(crate) type CbcDec = cbc::Decryptor<Aes256>;

/// Salt/IV width — RAR5 uses 128-bit salts and AES IVs.
pub(crate) const SIZE_SALT: usize = 16;
pub(crate) const SIZE_IV: usize = 16;
/// The stored password-check value folds the 32-byte PBKDF2 snapshot to 8 bytes.
pub(crate) const SIZE_PSWCHECK: usize = 8;

/// Ceiling on the KDF cost exponent. Real archives use ~15 (32 768 iterations);
/// unrar caps at 24. A hostile header could ask for 2^255 and hang the open, so
/// anything above this is refused as corrupt rather than derived.
pub(crate) const MAX_LG2_COUNT: u8 = 24;

/// The three values a RAR5 key derivation produces from one password + salt.
#[derive(Clone)]
pub(crate) struct Derived {
    /// AES-256 key for the file/header data.
    pub key: [u8; 32],
    /// MAC key for tweaked checksums (flag 0x02). Parsed and kept for
    /// completeness; the container currently skips CRC on MAC'd entries.
    #[allow(dead_code)]
    pub hash_key: [u8; 32],
    /// Password-check value, compared against the 8 bytes stored in the header
    /// to tell a wrong password from damaged data before any decrypt.
    pub psw_check: [u8; SIZE_PSWCHECK],
}

/// Derive the AES key, MAC key, and password-check value for `password` under
/// `salt` at cost `2^lg2_count`. Mirrors unrar's PBKDF2 snapshotting: one
/// running fold (block index 1), read off at `count`, `count + 16`, and
/// `count + 32` iterations. `lg2_count` must be `<= MAX_LG2_COUNT` (checked by
/// the caller).
pub(crate) fn derive(password: &[u8], salt: &[u8; SIZE_SALT], lg2_count: u8) -> Derived {
    let count: u64 = 1u64 << lg2_count.min(MAX_LG2_COUNT);

    // PBKDF2 block 1: HMAC over salt || big-endian block index (1).
    let mut salt_data = [0u8; SIZE_SALT + 4];
    salt_data[..SIZE_SALT].copy_from_slice(salt);
    salt_data[SIZE_SALT + 3] = 1;

    let base = HmacSha256::new_from_slice(password).expect("HMAC accepts any key length");
    let mut u = {
        let mut m = base.clone();
        m.update(&salt_data);
        m.finalize().into_bytes()
    };
    // Running XOR fold. `folded` = U1 ^ .. ^ U_i once `i` iterations are folded;
    // that fold at iteration N is exactly PBKDF2's one-block output for N iters.
    let mut folded = u;
    let mut key = [0u8; 32];
    let mut hash_key = [0u8; 32];
    let mut check_full = [0u8; 32];

    let mut i: u64 = 1;
    loop {
        if i == count {
            key.copy_from_slice(&folded);
        }
        if i == count + 16 {
            hash_key.copy_from_slice(&folded);
        }
        if i == count + 32 {
            check_full.copy_from_slice(&folded);
            break;
        }
        let mut m = base.clone();
        m.update(&u);
        u = m.finalize().into_bytes();
        for (f, x) in folded.iter_mut().zip(u.iter()) {
            *f ^= *x;
        }
        i += 1;
    }

    // Fold the 32-byte check snapshot down to 8 bytes (unrar's PswCheck).
    let mut psw_check = [0u8; SIZE_PSWCHECK];
    for (k, b) in check_full.iter().enumerate() {
        psw_check[k % SIZE_PSWCHECK] ^= *b;
    }
    Derived {
        key,
        hash_key,
        psw_check,
    }
}

/// Memoizes derivations by (salt, cost) for one archive open. A `-p` archive
/// uses a fresh salt per file, so hits are rare there; a `-hp` archive derives
/// the same header key for every block, and this makes all but the first free
/// (the same lever the 7z AES key cache found ~48x on many-entry encrypted
/// archives). The password is fixed per open, so it need not be part of the key.
#[derive(Default)]
pub(crate) struct KeyCache {
    map: HashMap<([u8; SIZE_SALT], u8), Derived>,
}

impl KeyCache {
    pub(crate) fn get(
        &mut self,
        password: &[u8],
        salt: &[u8; SIZE_SALT],
        lg2_count: u8,
    ) -> Derived {
        self.map
            .entry((*salt, lg2_count))
            .or_insert_with(|| derive(password, salt, lg2_count))
            .clone()
    }
}

/// A fresh AES-256-CBC decryptor. `data` fed to [`cbc_decrypt_blocks`] must be a
/// whole number of 16-byte blocks (RAR pads encrypted runs to the boundary).
pub(crate) fn new_cbc(key: &[u8; 32], iv: &[u8; SIZE_IV]) -> CbcDec {
    CbcDec::new(&Array::from(*key), &Array::from(*iv))
}

/// Decrypt whole 16-byte blocks in place, chaining CBC state through `dec`. A
/// trailing partial block (never expected from a padded RAR run) is left as-is.
pub(crate) fn cbc_decrypt_blocks(dec: &mut CbcDec, data: &mut [u8]) {
    for chunk in data.chunks_mut(16) {
        if chunk.len() < 16 {
            break;
        }
        let block: &mut Array<u8, _> = chunk.try_into().expect("16-byte block");
        dec.decrypt_block(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic_and_key_cache_agrees() {
        let salt = [7u8; SIZE_SALT];
        let a = derive(b"hunter2", &salt, 15);
        let b = derive(b"hunter2", &salt, 15);
        assert_eq!(a.key, b.key);
        assert_eq!(a.psw_check, b.psw_check);

        let mut cache = KeyCache::default();
        let c = cache.get(b"hunter2", &salt, 15);
        assert_eq!(a.key, c.key);
        // A different password diverges.
        let d = derive(b"hunter3", &salt, 15);
        assert_ne!(a.key, d.key);
        assert_ne!(a.psw_check, d.psw_check);
    }

    #[test]
    fn cbc_round_trips_against_a_known_encryptor() {
        // Encrypt with cbc's own encryptor, decrypt with ours: the two must be
        // inverses (guards the block wiring, key/iv order, and chaining).
        use aes::cipher::BlockModeEncrypt;
        type Enc = cbc::Encryptor<Aes256>;
        let key = [0x11u8; 32];
        let iv = [0x22u8; SIZE_IV];
        let mut buf = *b"sixteen bytes..!thirty-two bytes total----------"; // 48 bytes
        let plain = buf;
        let mut enc = Enc::new(&Array::from(key), &Array::from(iv));
        for chunk in buf.chunks_mut(16) {
            let block: &mut Array<u8, _> = chunk.try_into().unwrap();
            enc.encrypt_block(block);
        }
        assert_ne!(buf, plain);
        let mut dec = new_cbc(&key, &iv);
        cbc_decrypt_blocks(&mut dec, &mut buf);
        assert_eq!(buf, plain);
    }
}
