//! Tokens: OS keychain or an encrypted file.
//!
//! The file is a fallback for machines without a keychain (Linux without Secret Service, containers).
//! The encryption key sits next to it in `secrets.key` with mode 0600, so the file protects
//! against accidental leaks (backups, copying `secrets.enc`), not against someone who can read
//! the user's home directory. This matches sf cli without a keychain.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

const SERVICE: &str = "opk";
const KEY_FILE: &str = "secrets.key";
const DATA_FILE: &str = "secrets.enc";
const NONCE_LEN: usize = 12;

/// Explicit store choice: `keychain` or `file`.
pub const STORE_ENV: &str = "ORGPUNK_SECRET_STORE";

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"***")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Keychain,
    File,
}

impl std::fmt::Display for StoreKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keychain => "keychain",
            Self::File => "file",
        })
    }
}

#[derive(Debug)]
pub struct SecretStore {
    kind: StoreKind,
    dir: PathBuf,
}

impl SecretStore {
    /// The keychain if it responds, otherwise an encrypted file in `dir`.
    /// `ORGPUNK_SECRET_STORE=keychain|file` forces a choice.
    pub fn open(dir: &Path) -> Result<Self> {
        let kind = match std::env::var(STORE_ENV).as_deref() {
            Ok("keychain") => StoreKind::Keychain,
            Ok("file") => StoreKind::File,
            Ok(other) => {
                return Err(Error::Secrets(format!(
                    "{STORE_ENV}={other}: expected keychain or file"
                )));
            }
            Err(_) if keychain_available() => StoreKind::Keychain,
            Err(_) => StoreKind::File,
        };
        Ok(Self::with_kind(kind, dir))
    }

    pub fn with_kind(kind: StoreKind, dir: &Path) -> Self {
        Self {
            kind,
            dir: dir.to_owned(),
        }
    }

    pub fn kind(&self) -> StoreKind {
        self.kind
    }

    pub fn get(&self, key: &str) -> Result<Option<Tokens>> {
        let raw = match self.kind {
            StoreKind::Keychain => match entry(key)?.get_password() {
                Ok(s) => Some(s),
                Err(keyring::Error::NoEntry) => None,
                Err(e) => return Err(Error::Secrets(e.to_string())),
            },
            StoreKind::File => self.read_file()?.remove(key),
        };
        raw.map(|s| serde_json::from_str(&s).map_err(|e| Error::Secrets(format!("{key}: {e}"))))
            .transpose()
    }

    pub fn put(&self, key: &str, tokens: &Tokens) -> Result<()> {
        let raw = serde_json::to_string(tokens).expect("Tokens serialize");
        match self.kind {
            StoreKind::Keychain => entry(key)?
                .set_password(&raw)
                .map_err(|e| Error::Secrets(e.to_string())),
            StoreKind::File => {
                let mut all = self.read_file()?;
                all.insert(key.to_owned(), raw);
                self.write_file(&all)
            }
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        match self.kind {
            StoreKind::Keychain => match entry(key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(Error::Secrets(e.to_string())),
            },
            StoreKind::File => {
                let mut all = self.read_file()?;
                if all.remove(key).is_some() {
                    self.write_file(&all)?;
                }
                Ok(())
            }
        }
    }

    fn cipher(&self, create: bool) -> Result<Option<ChaCha20Poly1305>> {
        let path = self.dir.join(KEY_FILE);
        let key = match fs::read(&path) {
            Ok(k) if k.len() == 32 => k,
            Ok(_) => return Err(file_err(&path, "corrupted encryption key")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                let key = ChaCha20Poly1305::generate_key(&mut OsRng).to_vec();
                write_private(&path, &key)?;
                key
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(file_err(&path, e)),
        };
        Ok(Some(ChaCha20Poly1305::new(Key::from_slice(&key))))
    }

    fn read_file(&self) -> Result<BTreeMap<String, String>> {
        let path = self.dir.join(DATA_FILE);
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(file_err(&path, e)),
        };
        let cipher = self
            .cipher(false)?
            .ok_or_else(|| file_err(&path, "missing secrets.key"))?;
        if data.len() < NONCE_LEN {
            return Err(file_err(&path, "corrupted file"));
        }
        let (nonce, body) = data.split_at(NONCE_LEN);
        let plain = cipher
            .decrypt(Nonce::from_slice(nonce), body)
            .map_err(|_| file_err(&path, "cannot decrypt: wrong key or corrupted file"))?;
        serde_json::from_slice(&plain).map_err(|e| file_err(&path, e))
    }

    fn write_file(&self, all: &BTreeMap<String, String>) -> Result<()> {
        let path = self.dir.join(DATA_FILE);
        let cipher = self.cipher(true)?.expect("create = true");
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let plain = serde_json::to_vec(all).expect("map serializes");
        let body = cipher
            .encrypt(&nonce, plain.as_slice())
            .map_err(|_| file_err(&path, "encryption failed"))?;
        let mut data = nonce.to_vec();
        data.extend_from_slice(&body);
        write_private(&path, &data)
    }
}

fn entry(key: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, key).map_err(|e| Error::Secrets(e.to_string()))
}

fn keychain_available() -> bool {
    match keyring::Entry::new(SERVICE, "__probe__") {
        Ok(e) => matches!(e.get_password(), Ok(_) | Err(keyring::Error::NoEntry)),
        Err(_) => false,
    }
}

fn file_err(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::File {
        path: path.to_owned(),
        message: e.to_string(),
    }
}

/// Writes via a 0600 temp file and rename, so the file is never world-readable.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().expect("path has a parent");
    fs::create_dir_all(dir).map_err(|e| file_err(dir, e))?;
    let tmp = path.with_extension("tmp");
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).map_err(|e| file_err(&tmp, e))?;
    f.write_all(data)
        .and_then(|()| f.sync_all())
        .map_err(|e| file_err(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| file_err(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(n: u8) -> Tokens {
        Tokens {
            access_token: format!("at{n}"),
            refresh_token: Some(format!("rt{n}")),
        }
    }

    #[test]
    fn file_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let s = SecretStore::with_kind(StoreKind::File, dir.path());
        assert_eq!(s.get("a").unwrap(), None);
        s.put("a", &tokens(1)).unwrap();
        s.put("b", &tokens(2)).unwrap();
        assert_eq!(s.get("a").unwrap(), Some(tokens(1)));
        s.delete("a").unwrap();
        assert_eq!(s.get("a").unwrap(), None);
        assert_eq!(s.get("b").unwrap(), Some(tokens(2)));
    }

    #[test]
    fn file_store_is_encrypted_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let s = SecretStore::with_kind(StoreKind::File, dir.path());
        s.put("a", &tokens(7)).unwrap();
        let raw = fs::read(dir.path().join(DATA_FILE)).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("rt7"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for f in [KEY_FILE, DATA_FILE] {
                let mode = fs::metadata(dir.path().join(f))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{f}");
            }
        }
    }

    #[test]
    fn wrong_key_is_an_error_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = SecretStore::with_kind(StoreKind::File, dir.path());
        s.put("a", &tokens(1)).unwrap();
        fs::write(dir.path().join(KEY_FILE), [0u8; 32]).unwrap();
        assert!(s.get("a").is_err());
    }

    #[test]
    fn debug_masks_tokens() {
        let dbg = format!("{:?}", tokens(1));
        assert!(!dbg.contains("at1") && !dbg.contains("rt1"));
    }
}
