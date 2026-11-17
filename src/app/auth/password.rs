use std::sync::OnceLock;

use argon2::{Algorithm, Argon2, Params, PasswordHasher, PasswordVerifier, Version};

/// Argon2id memory in KiB (64 MiB), passes and lanes.
pub const MEMORY_KIB: u32 = 65_536;
pub const PASSES: u32 = 3;
pub const LANES: u32 = 1;
pub const MIN_LENGTH: usize = 12;
/// Longer input only costs hashing time.
pub const MAX_LENGTH: usize = 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PasswordError {
    #[error("the password must have at least {MIN_LENGTH} characters")]
    TooShort,
    #[error("the password must have at most {MAX_LENGTH} characters")]
    TooLong,
    #[error("the passwords do not match")]
    Mismatch,
    #[error("cannot hash the password: {0}")]
    Hash(String),
}

fn argon2() -> Argon2<'static> {
    let params = Params::new(MEMORY_KIB, PASSES, LANES, None).expect("valid Argon2 parameters");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Checks the length rules for a new password.
pub fn check_new(password: &str) -> Result<(), PasswordError> {
    match password.chars().count() {
        length if length < MIN_LENGTH => Err(PasswordError::TooShort),
        length if length > MAX_LENGTH => Err(PasswordError::TooLong),
        _ => Ok(()),
    }
}

/// An Argon2id PHC string with a random salt. Slow on purpose: call it off the async threads.
pub fn hash(password: &str) -> Result<String, PasswordError> {
    argon2()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| PasswordError::Hash(error.to_string()))
}

/// Whether `password` matches a stored PHC string. Uses the parameters stored in the string.
pub fn verify(password: &str, stored: &str) -> bool {
    argon2()
        .verify_password(password.as_bytes(), stored)
        .is_ok()
}

/// A hash of a random password, checked when the email is unknown so that the answer takes as
/// long as for a real user.
pub fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash(&super::token::random_token()).expect("hashing a random token works"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_argon2id_with_the_chosen_parameters_and_a_random_salt() {
        let first = hash("correct horse battery").unwrap();
        let second = hash("correct horse battery").unwrap();
        assert!(
            first.starts_with("$argon2id$v=19$m=65536,t=3,p=1$"),
            "{first}"
        );
        assert_ne!(first, second, "each hash gets its own salt");
        assert!(!first.contains("correct horse"));

        assert!(verify("correct horse battery", &first));
        assert!(!verify("correct horse batter", &first));
        assert!(!verify("correct horse battery", "not a hash"));
        assert!(!verify("anything", dummy_hash()));
    }

    #[test]
    fn new_passwords_need_twelve_characters() {
        assert_eq!(check_new("short"), Err(PasswordError::TooShort));
        assert_eq!(check_new("elevenchars"), Err(PasswordError::TooShort));
        assert_eq!(check_new("twelve chars"), Ok(()));
        assert_eq!(
            check_new(&"x".repeat(MAX_LENGTH + 1)),
            Err(PasswordError::TooLong)
        );
    }
}
