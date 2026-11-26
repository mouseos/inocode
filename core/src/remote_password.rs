//! The password of remote access, kept only as a salted slow hash
//! (docs/evidence/remote-access.md): PBKDF2-HMAC-SHA256 written as
//! `pbkdf2-sha256$<iterations>$<salt>$<hash>`, salt and hash in unpadded
//! standard base64.

use base64::engine::general_purpose::STANDARD_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// OWASP's Password Storage Cheat Sheet gives 600,000 for PBKDF2-HMAC-SHA256.
const ITERATIONS: u32 = 600_000;
const SALT_LEN: usize = 16;
const HASH_LEN: usize = 32;
const SCHEME: &str = "pbkdf2-sha256";

/// The longest password taken, in bytes.
pub const MAX_LEN: usize = 1024;

/// Hashes `password` with a new random salt.
pub fn hash(password: &str) -> Result<String, String> {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).map_err(|error| error.to_string())?;
    let derived = pbkdf2(password.as_bytes(), &salt, ITERATIONS);
    Ok(format!(
        "{SCHEME}${ITERATIONS}${}${}",
        STANDARD_NO_PAD.encode(salt),
        STANDARD_NO_PAD.encode(derived)
    ))
}

/// Whether `password` is the one `stored` was made from. The comparison
/// takes a time that does not depend on where the hashes first differ.
pub fn verify(stored: &str, password: &str) -> bool {
    let Ok((iterations, salt, expected)) = parse(stored) else {
        return false;
    };
    let derived = pbkdf2(password.as_bytes(), &salt, iterations);
    derived
        .iter()
        .zip(&expected)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

/// Checks that `stored` is a hash this module writes.
pub fn check(stored: &str) -> Result<(), String> {
    parse(stored).map(|_| ())
}

fn parse(stored: &str) -> Result<(u32, Vec<u8>, [u8; HASH_LEN]), String> {
    let invalid = || format!("not a {SCHEME}$<iterations>$<salt>$<hash> value");
    let mut parts = stored.split('$');
    if parts.next() != Some(SCHEME) {
        return Err(invalid());
    }
    let iterations: u32 = parts
        .next()
        .and_then(|text| text.parse().ok())
        .filter(|count| *count > 0)
        .ok_or_else(invalid)?;
    let salt = parts
        .next()
        .and_then(|text| STANDARD_NO_PAD.decode(text).ok())
        .filter(|salt| !salt.is_empty())
        .ok_or_else(invalid)?;
    let hash: [u8; HASH_LEN] = parts
        .next()
        .and_then(|text| STANDARD_NO_PAD.decode(text).ok())
        .and_then(|hash| hash.try_into().ok())
        .ok_or_else(invalid)?;
    if parts.next().is_some() {
        return Err(invalid());
    }
    Ok((iterations, salt, hash))
}

/// PBKDF2 (RFC 8018, section 5.2) with HMAC-SHA256 for one block of output,
/// which is the whole of a 32-byte key.
fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32) -> [u8; HASH_LEN] {
    let key = Hmac::<Sha256>::new_from_slice(password).expect("HMAC takes a key of any length");
    let mut first = key.clone();
    first.update(salt);
    first.update(&1u32.to_be_bytes());
    let mut block: [u8; HASH_LEN] = first.finalize().into_bytes().into();
    let mut derived = block;
    for _ in 1..iterations {
        let mut next = key.clone();
        next.update(&block);
        block = next.finalize().into_bytes().into();
        for (out, byte) in derived.iter_mut().zip(block) {
            *out ^= byte;
        }
    }
    derived
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_derivation_matches_published_pbkdf2_hmac_sha256_vectors() {
        assert_eq!(
            hex(&pbkdf2(b"password", b"salt", 1)),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        assert_eq!(
            hex(&pbkdf2(b"password", b"salt", 2)),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }

    #[test]
    fn a_hash_verifies_its_password_only_and_has_a_fresh_salt() {
        let stored = hash("correct horse").expect("hash");
        assert!(stored.starts_with("pbkdf2-sha256$600000$"), "{stored}");
        assert!(!stored.contains("correct horse"));
        check(&stored).expect("well formed");
        assert!(verify(&stored, "correct horse"));
        assert!(!verify(&stored, "correct hors"));
        assert!(!verify(&stored, ""));
        assert_ne!(stored, hash("correct horse").expect("hash"));
    }

    #[test]
    fn malformed_values_are_refused_and_never_verify() {
        for stored in [
            "",
            "plain",
            "pbkdf2-sha256$0$c2FsdA$AAAA",
            "pbkdf2-sha256$1$$AAAA",
            "pbkdf2-sha256$1$c2FsdA$AAAA",
            "sha1$1$c2FsdA$Eg+2z/z4syxD5yJSVsT4N6hlSMkszDVICAWYfLcL4Xs",
            "pbkdf2-sha256$1$c2FsdA$Eg+2z/z4syxD5yJSVsT4N6hlSMkszDVICAWYfLcL4Xs$x",
        ] {
            assert!(check(stored).is_err(), "{stored}");
            assert!(!verify(stored, "password"), "{stored}");
        }
        let one = "pbkdf2-sha256$1$c2FsdA$Eg+2z/z4syxD5yJSVsT4N6hlSMkszDVICAWYfLcL4Xs";
        check(one).expect("the c=1 vector");
        assert!(verify(one, "password"));
    }
}
