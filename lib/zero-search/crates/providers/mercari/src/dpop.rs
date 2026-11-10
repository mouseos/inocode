use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};

use rand::rngs::OsRng;
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct DpopSigner {
    key: SigningKey,
    jwk: serde_json::Value,
    uuid: String,
}

impl DpopSigner {
    pub fn new() -> Self {
        let key = SigningKey::random(&mut OsRng);
        let verifying_key = key.verifying_key();
        let point = verifying_key.to_encoded_point(false);

        let x = URL_SAFE_NO_PAD.encode(point.x().unwrap());
        let y = URL_SAFE_NO_PAD.encode(point.y().unwrap());

        let jwk = json!({
            "crv": "P-256",
            "kty": "EC",
            "x": x,
            "y": y,
        });

        Self {
            key,
            jwk,
            uuid: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub fn proof(&self, method: &str, url: &str) -> String {
        let header = json!({
            "typ": "dpop+jwt",
            "alg": "ES256",
            "jwk": self.jwk,
        });

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let payload = json!({
            "iat": now,
            "jti": uuid::Uuid::new_v4().to_string(),
            "htu": url,
            "htm": method.to_uppercase(),
            "uuid": self.uuid,
        });

        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(&header).unwrap().as_bytes());
        let payload_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_string(&payload).unwrap().as_bytes());

        let signing_input = format!("{header_b64}.{payload_b64}");
        let signature: Signature = self.key.sign(signing_input.as_bytes());

        let sig_bytes = signature.to_bytes();
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig_bytes);

        format!("{signing_input}.{sig_b64}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_proof_format() {
        let signer = DpopSigner::new();
        let proof = signer.proof("POST", "https://api.mercari.jp/v2/entities:search");
        let parts: Vec<&str> = proof.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(!parts[0].is_empty());
        assert!(!parts[1].is_empty());
        assert!(!parts[2].is_empty());
    }
}
