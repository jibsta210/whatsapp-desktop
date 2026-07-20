//! Crypto primitives for the Google Messages relay channel.
//!
//! - [`aesctr`] — AES-CTR + HMAC-SHA256 for RPC payload `encryptedData`
//! - [`aesgcm`] — chunked AES-GCM for media upload/download
//! - [`hkdf_derive`] — HKDF-SHA256 for UKEY2 key derivation
//! - [`ecdsa`] — P-256 ECDSA key generation, JWK serialization, ECDH
//!
//! Mirrors `pkg/libgm/crypto/` from mautrix-gmessages.

pub mod aesctr {
    //! AES-CTR + HMAC-SHA256 envelope used for all RPC `encryptedData` fields.
    //!
    //! Wire format (matches Go reference exactly):
    //! `ciphertext || iv (16) || HMAC-SHA256(ciphertext || iv) (32)`
    use aes::Aes256;
    use aes::cipher::{KeyIvInit, StreamCipher};
    use ctr::Ctr64BE;
    use elliptic_curve::subtle::ConstantTimeEq;
    use hmac::{Hmac, Mac};
    use rand::TryRng;
    use serde::{Deserialize, Serialize};
    use sha2::Sha256;

    use crate::{Error, Result};

    type Aes256Ctr = Ctr64BE<Aes256>;
    type HmacSha256 = Hmac<Sha256>;

    /// AES-256-CTR + HMAC-SHA256 keypair stored in [`AuthData`](crate::AuthData).
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct AesCtrHelper {
        #[serde(with = "crate::crypto::serde_bytes")]
        pub aes_key: Vec<u8>,
        #[serde(with = "crate::crypto::serde_bytes")]
        pub hmac_key: Vec<u8>,
    }

    impl AesCtrHelper {
        /// Generate fresh 32-byte AES + 32-byte HMAC keys.
        pub fn new_random() -> Self {
            Self {
                aes_key: super::random_bytes(32),
                hmac_key: super::random_bytes(32),
            }
        }

        pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
            if self.aes_key.len() != 32 {
                return Err(Error::Crypto(format!(
                    "aes key must be 32 bytes, got {}",
                    self.aes_key.len()
                )));
            }
            // 16-byte IV.
            let mut iv = [0u8; 16];
            rand::rngs::SysRng
                .try_fill_bytes(&mut iv)
                .map_err(|e| Error::Crypto(format!("rng: {e}")))?;

            // Encrypt.
            let mut buf = plaintext.to_vec();
            let mut cipher = Aes256Ctr::new(self.aes_key.as_slice().into(), &iv.into());
            cipher.apply_keystream(&mut buf);

            // Append IV.
            buf.extend_from_slice(&iv);

            // HMAC over (ciphertext || iv).
            let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.hmac_key)
                .map_err(|e| Error::Crypto(format!("hmac key: {e}")))?;
            mac.update(&buf);
            buf.extend_from_slice(&mac.finalize().into_bytes());

            Ok(buf)
        }

        pub fn decrypt(&self, envelope: &[u8]) -> Result<Vec<u8>> {
            if envelope.len() < 48 {
                return Err(Error::Crypto("envelope too short (<48 bytes)".into()));
            }
            // Split tail HMAC.
            let (rest, expected_mac) = envelope.split_at(envelope.len() - 32);
            let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.hmac_key)
                .map_err(|e| Error::Crypto(format!("hmac key: {e}")))?;
            mac.update(rest);
            let computed = mac.finalize().into_bytes();
            if computed.ct_eq(expected_mac).unwrap_u8() != 1 {
                return Err(Error::Crypto("HMAC mismatch".into()));
            }
            // Split IV (16 bytes from the end of `rest`).
            let (ct, iv) = rest.split_at(rest.len() - 16);
            let mut buf = ct.to_vec();
            let mut cipher = Aes256Ctr::new(self.aes_key.as_slice().into(), iv.into());
            cipher.apply_keystream(&mut buf);
            Ok(buf)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn roundtrip() {
            let h = AesCtrHelper::new_random();
            let pt = b"hello world this is a test of the encrypted broadcast system";
            let ct = h.encrypt(pt).unwrap();
            assert_ne!(&ct[..pt.len()], &pt[..]);
            let pt2 = h.decrypt(&ct).unwrap();
            assert_eq!(pt2, pt);
        }

        #[test]
        fn rejects_tampered_hmac() {
            let h = AesCtrHelper::new_random();
            let mut ct = h.encrypt(b"hello").unwrap();
            let last = ct.len() - 1;
            ct[last] ^= 0x01;
            assert!(h.decrypt(&ct).is_err());
        }

        #[test]
        fn rejects_short_input() {
            let h = AesCtrHelper::new_random();
            assert!(h.decrypt(&[0u8; 10]).is_err());
        }
    }
}

pub mod aesgcm {
    //! Chunked AES-256-GCM encryption for media.
    //!
    //! Wire format:
    //! - byte 0 = 0x00 (header signature)
    //! - byte 1 = log2(chunk_size) — Go uses `1 << 15 = 32768`, so this is `15`
    //! - then a sequence of `nonce (12) || ciphertext || tag (16)` chunks
    //!
    //! AAD per chunk = `[is_last:1] || u32be(chunk_index)` (5 bytes).
    use aes_gcm::Aes256Gcm;
    use aes_gcm::aead::{AeadInPlace, KeyInit, Nonce};
    use rand::TryRng;

    use crate::{Error, Result};

    const OUTGOING_CHUNK_SIZE: usize = 1 << 15;
    const NONCE_LEN: usize = 12;
    const TAG_LEN: usize = 16;
    const CHUNK_OVERHEAD: usize = NONCE_LEN + TAG_LEN;

    fn calculate_aad(index: u32, is_last: bool) -> [u8; 5] {
        let mut aad = [0u8; 5];
        aad[0] = if is_last { 1 } else { 0 };
        aad[1..5].copy_from_slice(&index.to_be_bytes());
        aad
    }

    pub fn encrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        if key.len() != 32 {
            return Err(Error::Crypto(format!(
                "aes-gcm key must be 32, got {}",
                key.len()
            )));
        }
        let cipher = Aes256Gcm::new(key.into());
        let mut out = Vec::with_capacity(2 + data.len() + 2 * CHUNK_OVERHEAD);
        out.push(0x00);
        out.push(15); // log2(32768)

        let chunk_size = OUTGOING_CHUNK_SIZE - CHUNK_OVERHEAD;
        let mut chunk_index: u32 = 0;
        let mut i = 0;
        while i < data.len() {
            let end = (i + chunk_size).min(data.len());
            let is_last = end == data.len();
            let aad = calculate_aad(chunk_index, is_last);

            let mut nonce_bytes = [0u8; NONCE_LEN];
            rand::rngs::SysRng
                .try_fill_bytes(&mut nonce_bytes)
                .map_err(|e| Error::Crypto(format!("rng: {e}")))?;
            let nonce = Nonce::<Aes256Gcm>::from_slice(&nonce_bytes);

            let mut buf = data[i..end].to_vec();
            let tag = cipher
                .encrypt_in_place_detached(nonce, &aad, &mut buf)
                .map_err(|e| Error::Crypto(format!("gcm encrypt: {e}")))?;

            out.extend_from_slice(&nonce_bytes);
            out.extend_from_slice(&buf);
            out.extend_from_slice(&tag);

            chunk_index += 1;
            i = end;
        }

        Ok(out)
    }

    pub fn decrypt(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }
        if key.len() != 32 {
            return Err(Error::Crypto(format!(
                "aes-gcm key must be 32, got {}",
                key.len()
            )));
        }
        if data[0] != 0x00 {
            return Err(Error::Crypto(format!(
                "invalid header byte: {:#x}",
                data[0]
            )));
        }
        let chunk_size: usize = 1usize << data[1];
        let cipher = Aes256Gcm::new(key.into());
        let mut out = Vec::with_capacity(data.len());
        let body = &data[2..];

        let mut chunk_index: u32 = 0;
        let mut i = 0;
        while i < body.len() {
            let end = (i + chunk_size).min(body.len());
            let is_last = end == body.len();
            let aad = calculate_aad(chunk_index, is_last);

            let chunk = &body[i..end];
            if chunk.len() < CHUNK_OVERHEAD {
                return Err(Error::Crypto(format!(
                    "chunk #{chunk_index} too short ({} bytes)",
                    chunk.len()
                )));
            }
            let nonce = Nonce::<Aes256Gcm>::from_slice(&chunk[..NONCE_LEN]);
            let ct_and_tag_end = chunk.len() - TAG_LEN;
            let mut buf = chunk[NONCE_LEN..ct_and_tag_end].to_vec();
            let tag = aes_gcm::Tag::from_slice(&chunk[ct_and_tag_end..]);
            cipher
                .decrypt_in_place_detached(nonce, &aad, &mut buf, tag)
                .map_err(|e| Error::Crypto(format!("gcm decrypt chunk #{chunk_index}: {e}")))?;
            out.extend_from_slice(&buf);

            chunk_index += 1;
            i = end;
        }

        Ok(out)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn roundtrip_small() {
            let key = vec![0x55u8; 32];
            let pt = b"hello, world";
            let ct = encrypt(&key, pt).unwrap();
            let back = decrypt(&key, &ct).unwrap();
            assert_eq!(back, pt);
        }

        #[test]
        fn roundtrip_multi_chunk() {
            let key = vec![0xAAu8; 32];
            let pt: Vec<u8> = (0..(OUTGOING_CHUNK_SIZE * 3 / 2) as u32)
                .map(|i| i as u8)
                .collect();
            let ct = encrypt(&key, &pt).unwrap();
            let back = decrypt(&key, &ct).unwrap();
            assert_eq!(back, pt);
        }
    }
}

pub mod hkdf_derive {
    //! HKDF-SHA256 helper for UKEY2 key derivation.
    use hkdf::Hkdf;
    use sha2::Sha256;

    use crate::{Error, Result};

    /// HKDF-Extract-then-Expand. `salt`/`info` are passed through directly.
    pub fn derive(ikm: &[u8], salt: &[u8], info: &[u8], len: usize) -> Result<Vec<u8>> {
        let hk = Hkdf::<Sha256>::new(if salt.is_empty() { None } else { Some(salt) }, ikm);
        let mut out = vec![0u8; len];
        hk.expand(info, &mut out)
            .map_err(|e| Error::Crypto(format!("hkdf expand: {e}")))?;
        Ok(out)
    }
}

pub mod ecdsa {
    //! P-256 ECDSA key handling. Mirrors `pkg/libgm/crypto/ecdsa.go`.
    //!
    //! Stored as a JWK-like struct so it round-trips through JSON cleanly.
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use elliptic_curve::sec1::ToEncodedPoint;
    use p256::SecretKey;
    use p256::ecdsa::{SigningKey, VerifyingKey};
    use rand::TryRng;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::{Error, Result};

    /// JWK (JSON Web Key) representation of a P-256 ECDSA private key.
    /// Matches the `JWK` struct in the Go reference: `kty="EC"`, `crv="P-256"`,
    /// `d`/`x`/`y` are base64url (no-pad) of the unsigned big-endian scalars
    /// or coordinates.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    pub struct JwkPair {
        #[serde(rename = "kty")]
        pub key_type: String,
        #[serde(rename = "crv")]
        pub curve: String,
        #[serde(rename = "d", with = "raw_url_b64")]
        pub d: Vec<u8>,
        #[serde(rename = "x", with = "raw_url_b64")]
        pub x: Vec<u8>,
        #[serde(rename = "y", with = "raw_url_b64")]
        pub y: Vec<u8>,
    }

    mod raw_url_b64 {
        use super::*;
        pub fn serialize<S: Serializer>(b: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
            s.serialize_str(&URL_SAFE_NO_PAD.encode(b))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> std::result::Result<Vec<u8>, D::Error> {
            let s = String::deserialize(d)?;
            URL_SAFE_NO_PAD.decode(s).map_err(serde::de::Error::custom)
        }
    }

    impl JwkPair {
        /// Generate a fresh P-256 keypair.
        pub fn generate() -> Result<Self> {
            // Try up to a few times in case the random scalar lands on zero
            // (vanishingly unlikely on P-256, but `SecretKey::from_slice`
            // rejects it).
            for _ in 0..4 {
                let mut bytes = [0u8; 32];
                rand::rngs::SysRng
                    .try_fill_bytes(&mut bytes)
                    .map_err(|e| Error::Crypto(format!("rng: {e}")))?;
                if let Ok(secret) = SecretKey::from_slice(&bytes) {
                    let public = secret.public_key();
                    let pt = public.to_encoded_point(false);
                    let x = pt
                        .x()
                        .ok_or_else(|| Error::Crypto("missing x".into()))?
                        .to_vec();
                    let y = pt
                        .y()
                        .ok_or_else(|| Error::Crypto("missing y".into()))?
                        .to_vec();
                    return Ok(Self {
                        key_type: "EC".into(),
                        curve: "P-256".into(),
                        d: secret.to_bytes().to_vec(),
                        x,
                        y,
                    });
                }
            }
            Err(Error::Crypto("could not generate p256 secret".into()))
        }

        /// Reconstruct the [`SigningKey`] (private signing key).
        pub fn signing_key(&self) -> Result<SigningKey> {
            SigningKey::from_slice(&self.d).map_err(|e| Error::Crypto(format!("signing key: {e}")))
        }

        /// Reconstruct the [`VerifyingKey`] (public side).
        pub fn verifying_key(&self) -> Result<VerifyingKey> {
            let signing = self.signing_key()?;
            Ok(*signing.verifying_key())
        }

        /// Return the SEC1 uncompressed encoding of the public key
        /// (`0x04 || x || y`).
        pub fn public_key_sec1_uncompressed(&self) -> Vec<u8> {
            let mut out = Vec::with_capacity(1 + self.x.len() + self.y.len());
            out.push(0x04);
            out.extend_from_slice(&self.x);
            out.extend_from_slice(&self.y);
            out
        }

        /// DER-encoded SubjectPublicKeyInfo (PKIX) of the public key.
        /// Equivalent to Go's `x509.MarshalPKIXPublicKey` for P-256.
        ///
        /// The wire format is fixed for P-256: a 26-byte ASN.1
        /// AlgorithmIdentifier prefix followed by the 65-byte uncompressed
        /// SEC1 point (`0x04 || X || Y`).
        pub fn public_key_pkix_der(&self) -> Result<Vec<u8>> {
            const P256_SPKI_PREFIX: [u8; 26] = [
                0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06,
                0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
            ];
            if self.x.len() != 32 || self.y.len() != 32 {
                return Err(Error::Crypto(format!(
                    "P-256 public key coordinates must be 32 bytes (got x={}, y={})",
                    self.x.len(),
                    self.y.len()
                )));
            }
            let mut out = Vec::with_capacity(91);
            out.extend_from_slice(&P256_SPKI_PREFIX);
            out.push(0x04);
            out.extend_from_slice(&self.x);
            out.extend_from_slice(&self.y);
            Ok(out)
        }
    }

    /// Sign `data` with SHA-256 then ASN.1-DER-encode the (r,s) signature.
    /// Equivalent to Go's `ecdsa.SignASN1(rand, key, sha256(data))`.
    pub fn sign_asn1_sha256(jwk: &JwkPair, data: &[u8]) -> Result<Vec<u8>> {
        use p256::ecdsa::Signature;
        use p256::ecdsa::signature::Signer;
        use sha2::{Digest, Sha256};
        let signing = jwk.signing_key()?;
        let digest = Sha256::digest(data);
        let sig: Signature = signing.sign(&digest);
        Ok(sig.to_der().as_bytes().to_vec())
    }

    /// One-shot ECDH: produce a shared secret given our [`SecretKey`] and
    /// the peer's SEC1-encoded public point.
    pub fn ecdh_shared_secret(our_secret: &SecretKey, peer_sec1: &[u8]) -> Result<Vec<u8>> {
        use p256::PublicKey;
        let peer_pk = PublicKey::from_sec1_bytes(peer_sec1)
            .map_err(|e| Error::Crypto(format!("peer pk: {e}")))?;
        // p256 only exposes ECDH via EphemeralSecret unless we drop to the
        // diffie_hellman primitive — use it directly.
        let shared = elliptic_curve::ecdh::diffie_hellman(
            our_secret.to_nonzero_scalar(),
            peer_pk.as_affine(),
        );
        Ok(shared.raw_secret_bytes().to_vec())
    }

    /// Reconstruct a [`SecretKey`] from a [`JwkPair`].
    pub fn jwk_to_secret_key(j: &JwkPair) -> Result<SecretKey> {
        SecretKey::from_slice(&j.d).map_err(|e| Error::Crypto(format!("secret key from JWK: {e}")))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn jwk_roundtrip_json() {
            let kp = JwkPair::generate().unwrap();
            let s = serde_json::to_string(&kp).unwrap();
            let kp2: JwkPair = serde_json::from_str(&s).unwrap();
            assert_eq!(kp, kp2);
        }

        #[test]
        fn pkix_der_is_p256() {
            let kp = JwkPair::generate().unwrap();
            let der = kp.public_key_pkix_der().unwrap();
            // P-256 SubjectPublicKeyInfo is always 91 bytes.
            assert_eq!(der.len(), 91);
        }

        #[test]
        fn ecdh_matches_both_sides() {
            let a = JwkPair::generate().unwrap();
            let b = JwkPair::generate().unwrap();
            let sa = jwk_to_secret_key(&a).unwrap();
            let sb = jwk_to_secret_key(&b).unwrap();
            let a_pub = sa.public_key().to_encoded_point(false).as_bytes().to_vec();
            let b_pub = sb.public_key().to_encoded_point(false).as_bytes().to_vec();
            let s_ab = ecdh_shared_secret(&sa, &b_pub).unwrap();
            let s_ba = ecdh_shared_secret(&sb, &a_pub).unwrap();
            assert_eq!(s_ab, s_ba);
        }
    }
}

/// Module-private helpers.
mod serde_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(b))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

fn random_bytes(n: usize) -> Vec<u8> {
    use rand::TryRng;
    let mut out = vec![0u8; n];
    rand::rngs::SysRng
        .try_fill_bytes(&mut out)
        .expect("SysRng must succeed");
    out
}
