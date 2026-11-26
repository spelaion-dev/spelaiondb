//! The encrypted connections file's format (ADR-088): sealing a plaintext with the user's
//! password and opening it again. No file system and no keychain here: `connection_transfer.rs`
//! decides what goes in and where it goes.
//!
//! ```text
//! 8   magic "SPDBCONN"
//! 1   format version (1)
//! 4   Argon2id memory in KiB, u32 little-endian
//! 4   Argon2id iterations, u32 little-endian
//! 4   Argon2id lanes, u32 little-endian
//! 16  salt
//! 12  nonce
//! ..  AES-256-GCM ciphertext and its 16-byte tag
//! ```
//!
//! Every byte before the ciphertext is the cipher's associated data, so a change to any of them
//! fails authentication as a change to the ciphertext does. GCM cannot tell a wrong password
//! from a changed file: both are [`ConnectionsFileError::WrongPasswordOrChanged`].

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::{Algorithm, Argon2, Params, Version};
use spelaion_driver_api::Secret;
use zeroize::Zeroizing;

/// What every connections file starts with.
pub const CONNECTIONS_FILE_MAGIC: &[u8; 8] = b"SPDBCONN";
/// The format version this build writes and the only one it reads.
pub const CONNECTIONS_FILE_VERSION: u8 = 1;
/// The largest connections file an import reads.
pub const CONNECTIONS_FILE_MAX: usize = 32 * 1024 * 1024;
/// The shortest password an export accepts, in characters.
pub const EXPORT_PASSWORD_MIN: usize = 12;

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const KEY_LEN: usize = 32;
const HEADER_LEN: usize = 8 + 1 + 4 + 4 + 4 + SALT_LEN + NONCE_LEN;

/// Argon2id's cost: memory in KiB, iterations and lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfCost {
    pub memory_kib: u32,
    pub iterations: u32,
    pub lanes: u32,
}

impl KdfCost {
    /// What an export uses: RFC 9106's second recommended setting.
    pub const EXPORT: KdfCost = KdfCost {
        memory_kib: 64 * 1024,
        iterations: 3,
        lanes: 4,
    };

    /// Whether an import may spend this much: so a file cannot make it use unbounded memory or
    /// time (ADR-088, point 4).
    fn acceptable(self) -> bool {
        (19 * 1024..=1024 * 1024).contains(&self.memory_kib)
            && (1..=10).contains(&self.iterations)
            && (1..=16).contains(&self.lanes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionsFileError {
    #[error("this is not a SpelaionDB connections file, or a newer version made it")]
    NotAConnectionsFile,
    #[error("the password is wrong, or the file was changed after it was exported")]
    WrongPasswordOrChanged,
    #[error("the password must have at least {EXPORT_PASSWORD_MIN} characters")]
    PasswordTooShort,
    #[error("the connections file is larger than {} MiB", CONNECTIONS_FILE_MAX / 1024 / 1024)]
    TooLarge,
    /// The system gave no random bytes, or the key could not be derived: nothing was written.
    #[error("could not encrypt the file: {0}")]
    Encrypt(String),
}

fn derive_key(
    password: &Secret,
    salt: &[u8],
    cost: KdfCost,
) -> Result<Zeroizing<[u8; KEY_LEN]>, String> {
    let params = Params::new(cost.memory_kib, cost.iterations, cost.lanes, Some(KEY_LEN))
        .map_err(|e| e.to_string())?;
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(password.expose().as_bytes(), salt, key.as_mut())
        .map_err(|e| e.to_string())?;
    Ok(key)
}

fn header(cost: KdfCost, salt: &[u8; SALT_LEN], nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(CONNECTIONS_FILE_MAGIC);
    header.push(CONNECTIONS_FILE_VERSION);
    header.extend_from_slice(&cost.memory_kib.to_le_bytes());
    header.extend_from_slice(&cost.iterations.to_le_bytes());
    header.extend_from_slice(&cost.lanes.to_le_bytes());
    header.extend_from_slice(salt);
    header.extend_from_slice(nonce);
    header
}

/// Encrypts `plaintext` with `password` into a whole connections file, with a new salt and
/// nonce from the operating system. A password shorter than [`EXPORT_PASSWORD_MIN`] characters
/// is refused.
pub fn seal(plaintext: &[u8], password: &Secret) -> Result<Vec<u8>, ConnectionsFileError> {
    if password.expose().chars().count() < EXPORT_PASSWORD_MIN {
        return Err(ConnectionsFileError::PasswordTooShort);
    }
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut salt).map_err(|e| ConnectionsFileError::Encrypt(e.to_string()))?;
    getrandom::fill(&mut nonce).map_err(|e| ConnectionsFileError::Encrypt(e.to_string()))?;
    seal_with(plaintext, password, KdfCost::EXPORT, salt, nonce)
}

/// [`seal`] with the cost, salt and nonce given (tests, which need cheaper costs and fixed
/// bytes). Never used to write a file a user keeps.
pub(crate) fn seal_with(
    plaintext: &[u8],
    password: &Secret,
    cost: KdfCost,
    salt: [u8; SALT_LEN],
    nonce: [u8; NONCE_LEN],
) -> Result<Vec<u8>, ConnectionsFileError> {
    let key = derive_key(password, &salt, cost).map_err(ConnectionsFileError::Encrypt)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())
        .map_err(|e| ConnectionsFileError::Encrypt(e.to_string()))?;
    let mut file = header(cost, &salt, &nonce);
    let sealed = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &file,
            },
        )
        .map_err(|e| ConnectionsFileError::Encrypt(e.to_string()))?;
    file.extend_from_slice(&sealed);
    Ok(file)
}

/// Decrypts a whole connections file with `password`. Everything is checked before the key is
/// derived (the magic, the version, the cost within bounds, the length), and nothing of the
/// plaintext is returned unless the whole file authenticates.
pub fn open(file: &[u8], password: &Secret) -> Result<Zeroizing<Vec<u8>>, ConnectionsFileError> {
    open_within(file, password, KdfCost::acceptable)
}

fn open_within(
    file: &[u8],
    password: &Secret,
    acceptable: impl Fn(KdfCost) -> bool,
) -> Result<Zeroizing<Vec<u8>>, ConnectionsFileError> {
    if file.len() > CONNECTIONS_FILE_MAX {
        return Err(ConnectionsFileError::TooLarge);
    }
    if file.len() < HEADER_LEN + TAG_LEN
        || &file[..8] != CONNECTIONS_FILE_MAGIC
        || file[8] != CONNECTIONS_FILE_VERSION
    {
        return Err(ConnectionsFileError::NotAConnectionsFile);
    }
    let word = |at: usize| u32::from_le_bytes([file[at], file[at + 1], file[at + 2], file[at + 3]]);
    let cost = KdfCost {
        memory_kib: word(9),
        iterations: word(13),
        lanes: word(17),
    };
    if !acceptable(cost) {
        return Err(ConnectionsFileError::NotAConnectionsFile);
    }
    let (header, sealed) = file.split_at(HEADER_LEN);
    let salt = &header[21..21 + SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&header[21 + SALT_LEN..]);
    // A key that cannot be derived from these bytes means they were not ours.
    let key =
        derive_key(password, salt, cost).map_err(|_| ConnectionsFileError::NotAConnectionsFile)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref())
        .map_err(|_| ConnectionsFileError::NotAConnectionsFile)?;
    cipher
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: sealed,
                aad: header,
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| ConnectionsFileError::WrongPasswordOrChanged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap enough for tests; the export's own cost is tested once.
    const TEST_COST: KdfCost = KdfCost {
        memory_kib: 64,
        iterations: 1,
        lanes: 1,
    };

    fn password(text: &str) -> Secret {
        Secret::new(text.to_string())
    }

    fn sealed(plaintext: &[u8]) -> Vec<u8> {
        seal_with(
            plaintext,
            &password("correct horse battery"),
            TEST_COST,
            [7; SALT_LEN],
            [9; NONCE_LEN],
        )
        .unwrap()
    }

    fn open_test(file: &[u8], with: &str) -> Result<Zeroizing<Vec<u8>>, ConnectionsFileError> {
        open_within(file, &password(with), |_| true)
    }

    #[test]
    fn a_sealed_file_opens_with_its_password_and_holds_no_plaintext() {
        let plaintext = b"{\"connections\":[{\"secrets\":{\"password\":\"hunter2\"}}]}";
        let file = sealed(plaintext);
        assert_eq!(&file[..8], CONNECTIONS_FILE_MAGIC);
        assert!(!file.windows(7).any(|w| w == b"hunter2"));
        assert_eq!(
            open_test(&file, "correct horse battery")
                .unwrap()
                .as_slice(),
            plaintext
        );
    }

    #[test]
    fn the_export_cost_round_trips_and_is_new_every_time() {
        let secret = password("a long enough password");
        let first = seal(b"same text", &secret).unwrap();
        let second = seal(b"same text", &secret).unwrap();
        assert_ne!(first, second, "a new salt and nonce every export");
        assert_eq!(open(&first, &secret).unwrap().as_slice(), b"same text");
    }

    #[test]
    fn a_short_password_is_refused_on_export() {
        assert_eq!(
            seal(b"x", &password("elevenchars")),
            Err(ConnectionsFileError::PasswordTooShort)
        );
    }

    #[test]
    fn a_wrong_password_fails_cleanly() {
        let file = sealed(b"secret things");
        for wrong in ["correct horse batterY", "", "correct horse battery "] {
            assert_eq!(
                open_test(&file, wrong).map(|_| ()),
                Err(ConnectionsFileError::WrongPasswordOrChanged),
                "{wrong:?}"
            );
        }
    }

    #[test]
    fn changing_any_byte_of_the_salt_nonce_ciphertext_or_tag_fails_as_a_wrong_password() {
        let file = sealed(b"secret things");
        for at in 21..file.len() {
            let mut changed = file.clone();
            changed[at] ^= 0x01;
            assert_eq!(
                open_test(&changed, "correct horse battery").map(|_| ()),
                Err(ConnectionsFileError::WrongPasswordOrChanged),
                "byte {at}"
            );
        }
    }

    #[test]
    fn a_changed_cost_is_refused_or_fails_authentication() {
        let file = sealed(b"secret things");
        // Bounds as small as the test's cost, so a flipped high byte is refused instead of
        // deriving over gigabytes; a flipped low byte derives another key and fails.
        let small =
            |cost: KdfCost| cost.memory_kib <= 256 && cost.iterations <= 2 && cost.lanes <= 2;
        for at in 9..21 {
            let mut changed = file.clone();
            changed[at] ^= 0x01;
            assert!(
                open_within(&changed, &password("correct horse battery"), small).is_err(),
                "byte {at}"
            );
        }
    }

    #[test]
    fn a_header_that_is_not_ours_is_not_a_connections_file() {
        let file = sealed(b"secret things");
        let mut magic = file.clone();
        magic[0] = b'X';
        let mut version = file.clone();
        version[8] = 2;
        for bad in [
            magic,
            version,
            file[..HEADER_LEN + TAG_LEN - 1].to_vec(),
            Vec::new(),
            b"SPDBCONN".to_vec(),
        ] {
            assert_eq!(
                open_test(&bad, "correct horse battery").map(|_| ()),
                Err(ConnectionsFileError::NotAConnectionsFile)
            );
        }
    }

    #[test]
    fn an_import_refuses_a_cost_outside_its_bounds_before_deriving_anything() {
        for cost in [
            KdfCost {
                memory_kib: 4 * 1024 * 1024,
                ..KdfCost::EXPORT
            },
            KdfCost {
                memory_kib: 8,
                ..KdfCost::EXPORT
            },
            KdfCost {
                iterations: 1_000,
                ..KdfCost::EXPORT
            },
            KdfCost {
                iterations: 0,
                ..KdfCost::EXPORT
            },
            KdfCost {
                lanes: 64,
                ..KdfCost::EXPORT
            },
        ] {
            let file = header(cost, &[0; SALT_LEN], &[0; NONCE_LEN])
                .into_iter()
                .chain([0u8; 32])
                .collect::<Vec<u8>>();
            // Through the real bounds: a huge memory cost would take seconds and a gigabyte
            // if it were derived.
            assert_eq!(
                open(&file, &password("whatever it is")).map(|_| ()),
                Err(ConnectionsFileError::NotAConnectionsFile),
                "{cost:?}"
            );
        }
        assert!(KdfCost::EXPORT.acceptable());
    }

    #[test]
    fn a_file_too_large_is_refused_before_anything_else() {
        let file = vec![0u8; CONNECTIONS_FILE_MAX + 1];
        assert_eq!(
            open(&file, &password("whatever it is")).map(|_| ()),
            Err(ConnectionsFileError::TooLarge)
        );
    }
}
