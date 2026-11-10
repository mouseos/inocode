//! `ant_ct` generation — faithful port of Python `baidu_search.antct`.
//!
//! Key/IV: base64-decode the two 44-char `.rodata` literals from
//! libAntctEncrypt.so and take the first 16 bytes of each, exactly as
//! the APK does. Cipher: AES-128-CBC with PKCS#7 padding. Plaintext:
//! `t={seconds}&w={md5_hex_lower(query UTF-8)}`. Output is base64,
//! then URL-encoded with `quote_plus(safe='')` semantics.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;

pub const KEY_B64_LITERAL: &str = "aGVsbF9iYWlkX2JveGFwcABib3hhcHBfYmFpZF9oZWw=";
pub const IV_B64_LITERAL: &str = "Ym94YXBwX2JhaWRfaGVsbABPYmplY3Qgbm90IEluaXQ=";
pub const BLOCK: usize = 16;

fn key() -> [u8; 16] {
    let raw = B64.decode(KEY_B64_LITERAL).expect("key literal");
    let mut out = [0u8; 16];
    out.copy_from_slice(&raw[..16]);
    out
}
fn iv() -> [u8; 16] {
    let raw = B64.decode(IV_B64_LITERAL).expect("iv literal");
    let mut out = [0u8; 16];
    out.copy_from_slice(&raw[..16]);
    out
}

/// zk9.c.c(byte[], false): MD5 digest as lowercase hex.
pub fn md5_hex_lower(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

/// Plaintext assembly of bq0.m.h(). `calibrated_time` is in *seconds*.
pub fn build_plaintext(calibrated_time: i64, query: &str) -> String {
    let w = md5_hex_lower(query.as_bytes());
    format!("t={}&w={}", calibrated_time, w)
}

fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad_len = BLOCK - (data.len() % BLOCK);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat_n(pad_len as u8, pad_len));
    out
}

fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || !data.len().is_multiple_of(BLOCK) {
        return Err("invalid padded length".to_string());
    }
    let pad_len = data[data.len() - 1] as usize;
    if !(1..=BLOCK).contains(&pad_len) {
        return Err("invalid padding".to_string());
    }
    if data[data.len() - pad_len..] != vec![pad_len as u8; pad_len] {
        return Err("invalid padding bytes".to_string());
    }
    Ok(data[..data.len() - pad_len].to_vec())
}

/// EncryptDecryptUtils.encrypt: AES-128-CBC/PKCS#7, base64 output.
pub fn antct_encrypt(plaintext: &str) -> String {
    let cipher = Aes128::new(&key().into());
    let data = pkcs7_pad(plaintext.as_bytes());
    let mut out = Vec::with_capacity(data.len());
    let mut chain = iv();
    for chunk in data.as_chunks::<BLOCK>().0 {
        let mut block = [0u8; BLOCK];
        for i in 0..BLOCK {
            block[i] = chunk[i] ^ chain[i];
        }
        let mut ga = aes::Block::from(block);
        cipher.encrypt_block(&mut ga);
        chain = ga.into();
        out.extend_from_slice(&chain);
    }
    B64.encode(&out)
}

/// Verification helper: decrypt a base64 ant_ct back to plaintext.
pub fn antct_decrypt(b64_ciphertext: &str) -> Result<String, String> {
    let ct = B64
        .decode(b64_ciphertext)
        .map_err(|e| format!("base64 decode failed: {}", e))?;
    if ct.is_empty() || !ct.len().is_multiple_of(BLOCK) {
        return Err("invalid padded length".to_string());
    }
    let cipher = Aes128::new(&key().into());
    let mut out = Vec::with_capacity(ct.len());
    let mut chain = iv();
    for chunk in ct.as_chunks::<BLOCK>().0 {
        let mut ga = *aes::Block::from_slice(chunk);
        cipher.decrypt_block(&mut ga);
        let dec: [u8; BLOCK] = ga.into();
        let mut block = [0u8; BLOCK];
        for i in 0..BLOCK {
            block[i] = dec[i] ^ chain[i];
        }
        chain = *chunk;
        out.extend_from_slice(&block);
    }
    let pt = pkcs7_unpad(&out)?;
    String::from_utf8(pt).map_err(|e| format!("utf-8 decode failed: {}", e))
}

/// Python `urllib.parse.quote_plus(s, safe='')`: letters, digits and
/// `_.-~` pass through, space becomes `+`, everything else `%XX`.
pub fn quote_plus(s: &str) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// bq0.m.h() in full: plaintext -> JNI encrypt -> URLEncoder.encode.
pub fn make_ant_ct(calibrated_time_secs: i64, query: &str) -> String {
    let text = build_plaintext(calibrated_time_secs, query);
    let enc = antct_encrypt(&text);
    quote_plus(&enc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_and_md5() {
        assert_eq!(
            build_plaintext(1700000000, "weather"),
            "t=1700000000&w=aab92e69374e4c7b8c6741fe02e574b9"
        );
        assert_eq!(
            md5_hex_lower("天气".as_bytes()),
            "265f273cf1a7e9e3d0921d6b7f8462c4"
        );
        assert_eq!(md5_hex_lower(b""), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn decrypt_roundtrip() {
        for pt in [
            "t=1&w=abc",
            "t=1700000000&w=aab92e69374e4c7b8c6741fe02e574b9",
            "",
        ] {
            let enc = antct_encrypt(pt);
            assert_eq!(antct_decrypt(&enc).unwrap(), pt);
        }
        // make_ant_ct output (quote_plus form) percent-decodes to a
        // ciphertext that decrypts to the expected plaintext; the
        // t value is seconds, never milliseconds.
        let v = make_ant_ct(1761400000, "天气");
        let b64 = v
            .replace("%2B", "+")
            .replace("%2F", "/")
            .replace("%3D", "=");
        let pt = antct_decrypt(&b64).unwrap();
        assert!(pt.starts_with("t=1761400000&w="), "{}", pt);
        assert!(!pt.contains("1761400000123"));
    }
}
