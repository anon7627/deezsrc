//! Deezer stream decryption, as open-source Deezer clients do it.
//!
//! A stream is cut into 2048-byte chunks. Every third chunk (index 0, 3,
//! 6…) is encrypted with Blowfish-CBC, IV `00 01 02 … 07`, no padding; the
//! others are in clear, and so is a last chunk shorter than 2048 bytes.
//! Each chunk starts a new CBC chain, so any chunk can be decrypted on its
//! own: a stream can be read from any chunk boundary.
//!
//! The key depends on the track id only: with `h` the lowercase hex MD5 of
//! the id, `key[i] = h[i] ^ h[i + 16] ^ SECRET[i]`.

use blowfish::cipher::block_padding::NoPadding;
use blowfish::cipher::{BlockDecryptMut, InnerIvInit, KeyInit};
use blowfish::Blowfish;
use md5::{Digest, Md5};

pub const CHUNK: usize = 2048;
const IV: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
/// Public constant of the web player, the same for everyone.
const SECRET: &[u8; 16] = b"g4el58wc0zvf9na1";

/// Blowfish key of track `sng_id`.
pub fn track_key(sng_id: &str) -> [u8; 16] {
    let hex = format!("{:x}", Md5::digest(sng_id.as_bytes()));
    let h = hex.as_bytes();
    std::array::from_fn(|i| h[i] ^ h[i + 16] ^ SECRET[i])
}

/// Whether chunk `index` (0-based, from the start of the file) is encrypted.
pub fn is_encrypted(index: u64) -> bool {
    index.is_multiple_of(3)
}

/// Decrypts a stream as its bytes arrive, starting at a chunk boundary.
pub struct Decryptor {
    cipher: Blowfish,
    /// Index of the chunk `pending` belongs to.
    index: u64,
    pending: Vec<u8>,
}

impl Decryptor {
    /// `offset`: where the bytes fed start in the file, a multiple of
    /// [`CHUNK`].
    pub fn new(key: &[u8; 16], offset: u64) -> Self {
        debug_assert_eq!(offset % CHUNK as u64, 0);
        let cipher = Blowfish::new_from_slice(key).expect("16-byte Blowfish key");
        Decryptor { cipher, index: offset / CHUNK as u64, pending: Vec::with_capacity(CHUNK) }
    }

    fn decrypt_chunk(&self, chunk: &mut [u8]) {
        let dec = cbc::Decryptor::<Blowfish>::inner_iv_init(self.cipher.clone(), &IV.into());
        dec.decrypt_padded_mut::<NoPadding>(chunk).expect("2048 is a multiple of the block size");
    }

    /// Feed bytes; the clear bytes of every chunk completed so far are
    /// appended to `out`.
    pub fn feed(&mut self, mut data: &[u8], out: &mut Vec<u8>) {
        while !data.is_empty() {
            let take = (CHUNK - self.pending.len()).min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() == CHUNK {
                let mut chunk = std::mem::take(&mut self.pending);
                if is_encrypted(self.index) {
                    self.decrypt_chunk(&mut chunk);
                }
                out.extend_from_slice(&chunk);
                chunk.clear();
                self.pending = chunk;
                self.index += 1;
            }
        }
    }

    /// The end of the stream: a last, shorter chunk is in clear.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.pending);
    }
}

/// Encrypt a whole file the way Deezer serves it (tests and mock servers).
#[cfg(test)]
pub fn encrypt(key: &[u8; 16], plain: &[u8]) -> Vec<u8> {
    use blowfish::cipher::{BlockEncryptMut, KeyIvInit};
    let mut out = plain.to_vec();
    for (i, chunk) in out.chunks_mut(CHUNK).enumerate() {
        if chunk.len() == CHUNK && is_encrypted(i as u64) {
            let enc = cbc::Encryptor::<Blowfish>::new_from_slices(key, &IV).unwrap();
            enc.encrypt_padded_mut::<NoPadding>(chunk, CHUNK).unwrap();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn keys_follow_the_track_id() {
        // Computed with an independent implementation.
        assert_eq!(hex(&track_key("3135556")), "6c6c666b39662c37652575603c643439");
        assert_eq!(hex(&track_key("1")), "3464656e343a7d3a672c236a33696061");
        assert_ne!(track_key("1"), track_key("4"));
    }

    #[test]
    fn chunk_matches_a_reference_blowfish() {
        // `openssl enc -bf-cbc -K <key> -iv 0001020304050607 -nopad` on the
        // bytes `i % 251`.
        let plain: Vec<u8> = (0..CHUNK).map(|i| (i % 251) as u8).collect();
        let key = track_key("3135556");
        let enc = encrypt(&key, &plain);
        assert_eq!(hex(&enc[..16]), "367d4cd324ec3aa13d894f19198289f1");
        assert_eq!(hex(&enc[CHUNK - 16..]), "090b6589f3ea2fdd66c2448210ee8389");
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 % 256) as u8).collect()
    }

    #[test]
    fn round_trip_in_odd_pieces() {
        let key = track_key("4");
        let plain = sample(CHUNK * 7 + 1000);
        let enc = encrypt(&key, &plain);
        assert_ne!(enc[..CHUNK], plain[..CHUNK], "chunk 0 is encrypted");
        assert_eq!(enc[CHUNK..2 * CHUNK], plain[CHUNK..2 * CHUNK], "chunk 1 is not");
        assert_eq!(enc[7 * CHUNK..], plain[7 * CHUNK..], "a short last chunk is in clear");
        for piece in [1, 17, 2048, 3000, 100_000] {
            let mut d = Decryptor::new(&key, 0);
            let mut out = Vec::new();
            for part in enc.chunks(piece) {
                d.feed(part, &mut out);
            }
            d.finish(&mut out);
            assert!(out == plain, "pieces of {piece}");
        }
    }

    #[test]
    fn any_chunk_boundary_and_any_range() {
        let key = track_key("1");
        let plain = sample(CHUNK * 9 + 5);
        let enc = encrypt(&key, &plain);
        // What the relay does for `bytes=a-b`: fetch from the chunk holding
        // `a`, decrypt, drop what comes before `a`.
        for (a, b) in [(0, 10), (2047, 2048), (3 * CHUNK + 1, 6 * CHUNK + 3), (CHUNK * 9, CHUNK * 9 + 4), (5000, plain.len() - 1)] {
            let start = a - a % CHUNK;
            let mut d = Decryptor::new(&key, start as u64);
            let mut out = Vec::new();
            d.feed(&enc[start..], &mut out);
            d.finish(&mut out);
            assert!(out[a - start..=b - start] == plain[a..=b], "bytes {a}-{b}");
        }
    }
}
