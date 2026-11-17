use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// 256 random bits from the OS, as URL-safe base64.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random source works");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// SHA-256 of a token, as stored in the database.
pub fn hash_token(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_256_bit_values() {
        let (first, second) = (random_token(), random_token());
        assert_ne!(first, second);
        assert_eq!(URL_SAFE_NO_PAD.decode(&first).unwrap().len(), 32);
        assert_ne!(hash_token(&first), first);
        assert_eq!(hash_token(&first), hash_token(&first));
    }
}
