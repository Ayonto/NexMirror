//! Pairing (SPAKE2 password-authenticated key exchange), key derivation and AES-256-GCM sealing.
//!
//! First contact: the password is the 6-digit code shown on the host. SPAKE2 means an eavesdropper
//! learns nothing and an active attacker gets exactly one guess per code.
//! After pairing, both sides store a random 32-byte secret and use it as the SPAKE2 password.

use aes_gcm::{aead::AeadInPlace, Aes256Gcm, KeyInit, Nonce, Tag};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub type Pake = Spake2<Ed25519Group>;

pub fn pake_start(as_client: bool, password: &[u8], client_id: &str, host_id: &str) -> (Pake, Vec<u8>) {
    let pw = Password::new(password);
    let a = Identity::new(client_id.as_bytes());
    let b = Identity::new(host_id.as_bytes());
    if as_client {
        Spake2::<Ed25519Group>::start_a(&pw, &a, &b)
    } else {
        Spake2::<Ed25519Group>::start_b(&pw, &a, &b)
    }
}

pub fn pake_finish(p: Pake, peer_msg: &[u8]) -> Option<Vec<u8>> {
    p.finish(peer_msg).ok()
}

pub struct Keys {
    pub c2s: [u8; 32],
    pub s2c: [u8; 32],
    pub media: [u8; 32],
    pub punch: [u8; 32],
}

pub fn derive_keys(shared: &[u8]) -> Keys {
    let hk = Hkdf::<Sha256>::new(Some(b"NexMirror/1"), shared);
    let mut k = Keys { c2s: [0; 32], s2c: [0; 32], media: [0; 32], punch: [0; 32] };
    hk.expand(b"control c2s", &mut k.c2s).unwrap();
    hk.expand(b"control s2c", &mut k.s2c).unwrap();
    hk.expand(b"media s2c", &mut k.media).unwrap();
    hk.expand(b"punch c2s", &mut k.punch).unwrap();
    k
}

/// AES-256-GCM with a 64-bit sequence number as nonce. Each key is used in one direction only.
pub struct Sealer {
    cipher: Aes256Gcm,
}

pub const TAG_LEN: usize = 16;

impl Sealer {
    pub fn new(key: &[u8; 32]) -> Self {
        Self { cipher: Aes256Gcm::new(key.into()) }
    }

    fn nonce(seq: u64) -> Nonce<aes_gcm::aead::consts::U12> {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&seq.to_le_bytes());
        n.into()
    }

    /// Encrypts `data` in place and appends the tag.
    pub fn seal(&self, seq: u64, aad: &[u8], data: &mut Vec<u8>) {
        let tag = self.cipher.encrypt_in_place_detached(&Self::nonce(seq), aad, data).expect("seal");
        data.extend_from_slice(&tag);
    }

    /// Encrypts `buf[hdr_len..]` in place using `buf[..hdr_len]` as associated data; appends the tag.
    pub fn seal_tail(&self, seq: u64, buf: &mut Vec<u8>, hdr_len: usize) {
        let (aad, body) = buf.split_at_mut(hdr_len);
        let tag = self.cipher.encrypt_in_place_detached(&Self::nonce(seq), aad, body).expect("seal");
        buf.extend_from_slice(&tag);
    }

    /// Inverse of `seal_tail`; returns the plaintext length (plaintext starts at `hdr_len`).
    pub fn open_tail(&self, seq: u64, buf: &mut [u8], hdr_len: usize) -> Option<usize> {
        if buf.len() < hdr_len {
            return None;
        }
        let (aad, body) = buf.split_at_mut(hdr_len);
        self.open(seq, aad, body)
    }

    /// Decrypts `data` (ciphertext followed by tag) in place; returns plaintext length.
    pub fn open(&self, seq: u64, aad: &[u8], data: &mut [u8]) -> Option<usize> {
        if data.len() < TAG_LEN {
            return None;
        }
        let n = data.len() - TAG_LEN;
        let (body, tag) = data.split_at_mut(n);
        let tag = Tag::clone_from_slice(tag);
        self.cipher.decrypt_in_place_detached(&Self::nonce(seq), aad, body, &tag).ok()?;
        Some(n)
    }
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut v);
    v
}

pub fn random_pin() -> String {
    format!("{:06}", rand::thread_rng().next_u32() % 1_000_000)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// Encrypts with DPAPI (current user). Falls back to plain bytes if DPAPI is unavailable.
pub fn protect(data: &[u8]) -> Vec<u8> {
    use windows::Win32::{Foundation::*, Security::Cryptography::*};
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        if CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out).is_ok() {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(HLOCAL(out.pbData as _));
            let mut tagged = b"DP1".to_vec();
            tagged.extend(v);
            return tagged;
        }
    }
    data.to_vec()
}

pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::{Foundation::*, Security::Cryptography::*};
    let Some(blob) = data.strip_prefix(b"DP1") else { return Some(data.to_vec()) };
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: blob.len() as u32, pbData: blob.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out).ok()?;
        let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(HLOCAL(out.pbData as _));
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pake_roundtrip_and_mismatch() {
        let (a, ma) = pake_start(true, b"123456", "c", "h");
        let (b, mb) = pake_start(false, b"123456", "c", "h");
        assert_eq!(pake_finish(a, &mb), pake_finish(b, &ma));
        let (a, ma) = pake_start(true, b"123456", "c", "h");
        let (b, mb) = pake_start(false, b"654321", "c", "h");
        assert_ne!(pake_finish(a, &mb), pake_finish(b, &ma));
    }

    #[test]
    fn seal_open() {
        let s = Sealer::new(&[7; 32]);
        let mut d = b"hello".to_vec();
        s.seal(5, b"hdr", &mut d);
        assert!(s.open(5, b"hdr", &mut d.clone()).is_some());
        assert!(s.open(6, b"hdr", &mut d.clone()).is_none());
        assert!(s.open(5, b"hdX", &mut d).is_none());
    }
}
