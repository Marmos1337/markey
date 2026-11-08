//! Where the MarMic ID session is kept: a sealed file in
//! `config_dir()/secrets/`.
//!
//! macOS: AES-256-GCM with a random key in a file beside it. Windows: DPAPI for
//! the current user. Not the Keychain, even though the Anthropic key lives
//! there ([`crate::secrets`]): builds are signed ad hoc, so every update is a
//! new identity to macOS and the Keychain would ask «Разрешить» after each one,
//! and this session is read at every launch, unlike a key the user pasted once.
//!
//! The key beside the file does not protect against something running as the
//! user. It keeps the tokens out of backups (the folder is excluded from Time
//! Machine) and out of anything that only reads the file.
//!
//! Elsewhere the session lasts until MarKey quits.

use std::sync::Arc;

/// One blob of secret bytes.
pub trait SealedStore: Send + Sync {
    /// What was saved. `None` for nothing, and equally for anything damaged or
    /// unreadable: that means signed out, never a failure to start.
    fn load(&self) -> Option<Vec<u8>>;
    /// Replace what is stored.
    fn save(&self, plain: &[u8]) -> Result<(), String>;
    /// Remove it; not an error when there is nothing.
    fn clear(&self) -> Result<(), String>;
}

/// The platform's sealed file.
pub fn system() -> Arc<dyn SealedStore> {
    #[cfg(target_os = "macos")]
    {
        let dir = secrets_dir();
        Arc::new(file::FileStore::new(
            dir.clone(),
            macos::KeyFile::new(dir, macos::exclude_from_backup),
        ))
    }
    #[cfg(target_os = "windows")]
    {
        Arc::new(file::FileStore::new(secrets_dir(), windows::Dpapi))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Arc::new(MemoryStore::default())
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn secrets_dir() -> std::path::PathBuf {
    crate::settings::config_dir().join("secrets")
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod file {
    use super::SealedStore;
    use crate::private_files::{atomic_write, ensure_private_dir};
    use std::path::{Path, PathBuf};

    const MAGIC: &[u8] = b"MKSEAL1\0";
    const FILE_NAME: &str = "account.sealed";
    /// A session with its largest avatar is well under this; anything bigger
    /// is not a file of ours.
    const MAX_FILE: u64 = 4 * 1024 * 1024;

    /// Turns the secret into something safe to leave on disk, and back.
    pub trait Cipher: Send + Sync {
        fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String>;
        /// The plain bytes, or what kind of thing is wrong: "key", "format"
        /// or "decrypt", for the log. Never anything from the file.
        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, &'static str>;
    }

    /// Reads a plain file: never a link that leads somewhere else.
    pub fn read_regular(path: &Path) -> Option<Vec<u8>> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_FILE {
            return None;
        }
        std::fs::read(path).ok()
    }

    pub struct FileStore<C> {
        dir: PathBuf,
        cipher: C,
    }

    impl<C: Cipher> FileStore<C> {
        pub fn new(dir: PathBuf, cipher: C) -> FileStore<C> {
            FileStore { dir, cipher }
        }

        fn path(&self) -> PathBuf {
            self.dir.join(FILE_NAME)
        }

        /// `Ok(None)` when there is simply no file; an error is a file that
        /// is there and cannot be used.
        pub fn read(&self) -> Result<Option<Vec<u8>>, &'static str> {
            let path = self.path();
            if std::fs::symlink_metadata(&path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                return Ok(None);
            }
            let raw = read_regular(&path).ok_or("format")?;
            let body = raw.strip_prefix(MAGIC).ok_or("format")?;
            self.cipher.open(body).map(Some)
        }
    }

    impl<C: Cipher> SealedStore for FileStore<C> {
        fn load(&self) -> Option<Vec<u8>> {
            match self.read() {
                Ok(found) => found,
                Err(class) => {
                    log::warn!("the stored account session cannot be read ({class})");
                    None
                }
            }
        }

        fn save(&self, plain: &[u8]) -> Result<(), String> {
            ensure_private_dir(&self.dir).map_err(|e| e.to_string())?;
            let mut out = MAGIC.to_vec();
            out.extend(self.cipher.seal(plain)?);
            atomic_write(&self.path(), &out).map_err(|e| e.to_string())
        }

        fn clear(&self) -> Result<(), String> {
            match std::fs::remove_file(self.path()) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::file::{read_regular, Cipher};
    use crate::private_files::{atomic_write, ensure_private_dir};
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes256Gcm, Nonce};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    const KEY_FILE: &str = "account.key";
    const KEY_LEN: usize = 32;
    const NONCE_LEN: usize = 12;
    /// Binds the ciphertext to its purpose.
    const AAD: &[u8] = b"markey-account\0v1";

    pub struct KeyFile {
        dir: PathBuf,
        exclude: fn(&Path),
        /// Two first saves at once would each make a key, and whatever was
        /// sealed with the losing one could never be opened.
        creating: Mutex<()>,
    }

    /// The key sits beside the sealed file, so the two must never land in a
    /// backup together. Sticky: it stays with the folder.
    pub fn exclude_from_backup(dir: &Path) {
        let done = std::process::Command::new("/usr/bin/tmutil")
            .arg("addexclusion")
            .arg(dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !done.is_ok_and(|status| status.success()) {
            log::warn!("could not exclude the secrets folder from Time Machine");
        }
    }

    impl KeyFile {
        pub fn new(dir: PathBuf, exclude: fn(&Path)) -> KeyFile {
            KeyFile {
                dir,
                exclude,
                creating: Mutex::new(()),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.join(KEY_FILE)
        }

        fn read(&self) -> Option<Vec<u8>> {
            read_regular(&self.path()).filter(|key| key.len() == KEY_LEN)
        }

        fn read_or_create(&self) -> Result<Vec<u8>, String> {
            let _only_one = self.creating.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(key) = self.read() {
                return Ok(key);
            }
            let mut key = vec![0u8; KEY_LEN];
            getrandom::fill(&mut key).map_err(|e| e.to_string())?;
            ensure_private_dir(&self.dir).map_err(|e| e.to_string())?;
            atomic_write(&self.path(), &key).map_err(|e| e.to_string())?;
            (self.exclude)(&self.dir);
            Ok(key)
        }
    }

    impl Cipher for KeyFile {
        fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
            let key = self.read_or_create()?;
            let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| e.to_string())?;
            let mut nonce = [0u8; NONCE_LEN];
            getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
            let body = cipher
                .encrypt(
                    Nonce::from_slice(&nonce),
                    Payload {
                        msg: plain,
                        aad: AAD,
                    },
                )
                .map_err(|e| e.to_string())?;
            let mut out = nonce.to_vec();
            out.extend(body);
            Ok(out)
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, &'static str> {
            if sealed.len() < NONCE_LEN {
                return Err("format");
            }
            let (nonce, body) = sealed.split_at(NONCE_LEN);
            let key = self.read().ok_or("key")?;
            Aes256Gcm::new_from_slice(&key)
                .map_err(|_| "key")?
                .decrypt(
                    Nonce::from_slice(nonce),
                    Payload {
                        msg: body,
                        aad: AAD,
                    },
                )
                .map_err(|_| "decrypt")
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::file::Cipher;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// Mixed in, so another program's DPAPI blob cannot be passed off as ours.
    const ENTROPY: &[u8] = b"markey-account-v1";

    pub struct Dpapi;

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            // The API takes a mutable pointer but only reads inputs.
            pbData: bytes.as_ptr().cast_mut(),
        }
    }

    /// Copy out what DPAPI allocated, and free it.
    ///
    /// # Safety
    /// `output` must have been filled in by a successful DPAPI call.
    unsafe fn take(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: per this function's contract, pbData points at cbData bytes.
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        // SAFETY: DPAPI allocates its output with LocalAlloc.
        unsafe { LocalFree(Some(HLOCAL(output.pbData.cast()))) };
        bytes
    }

    impl Cipher for Dpapi {
        fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
            let input = blob(plain);
            let entropy = blob(ENTROPY);
            let mut output = CRYPT_INTEGER_BLOB::default();
            // SAFETY: the input blobs borrow slices that outlive the call, and
            // `output` is a valid place for the result.
            unsafe {
                CryptProtectData(
                    &input,
                    PCWSTR::null(),
                    Some(&entropy),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
            .map_err(|e| e.to_string())?;
            // SAFETY: the call above succeeded.
            Ok(unsafe { take(output) })
        }

        fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, &'static str> {
            let input = blob(sealed);
            let entropy = blob(ENTROPY);
            let mut output = CRYPT_INTEGER_BLOB::default();
            // SAFETY: as in `seal`.
            unsafe {
                CryptUnprotectData(
                    &input,
                    None,
                    Some(&entropy),
                    None,
                    None,
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
            .map_err(|_| "decrypt")?;
            // SAFETY: the call above succeeded.
            Ok(unsafe { take(output) })
        }
    }
}

/// Held in memory only: for tests, and for platforms with no sealed file.
#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
#[derive(Default)]
pub struct MemoryStore {
    pub value: std::sync::Mutex<Option<Vec<u8>>>,
    #[cfg(test)]
    pub fail: std::sync::atomic::AtomicBool,
}

#[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
impl SealedStore for MemoryStore {
    fn load(&self) -> Option<Vec<u8>> {
        self.value.lock().ok()?.clone()
    }

    fn save(&self, plain: &[u8]) -> Result<(), String> {
        #[cfg(test)]
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("disk full".into());
        }
        *self.value.lock().map_err(|e| e.to_string())? = Some(plain.to_vec());
        Ok(())
    }

    fn clear(&self) -> Result<(), String> {
        *self.value.lock().map_err(|e| e.to_string())? = None;
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const SECRET: &[u8] = b"refresh-token-do-not-leak";

    fn store(dir: &Path) -> file::FileStore<macos::KeyFile> {
        fn no_backup_change(_: &Path) {}
        file::FileStore::new(
            dir.to_path_buf(),
            macos::KeyFile::new(dir.to_path_buf(), no_backup_change),
        )
    }

    fn sealed(dir: &Path) -> PathBuf {
        dir.join("account.sealed")
    }

    #[test]
    fn what_is_saved_comes_back() {
        let dir = crate::testing::temp_dir("sealed-roundtrip");
        let vault = store(&dir);
        assert_eq!(vault.load(), None);
        vault.save(SECRET).unwrap();
        assert_eq!(vault.load().as_deref(), Some(SECRET));
        // A new instance, as after a restart, with a new value.
        let again = store(&dir);
        assert_eq!(again.load().as_deref(), Some(SECRET));
        again.save(b"second").unwrap();
        assert_eq!(store(&dir).load().as_deref(), Some(&b"second"[..]));
        again.clear().unwrap();
        assert_eq!(again.load(), None);
        again.clear().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_on_disk_is_readable_or_open_to_others() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testing::temp_dir("sealed-private");
        store(&dir).save(SECRET).unwrap();
        let raw = std::fs::read(sealed(&dir)).unwrap();
        assert!(!raw.windows(SECRET.len()).any(|w| w == SECRET));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&sealed(&dir)), 0o600);
        assert_eq!(mode(&dir.join("account.key")), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_files_read_as_nothing() {
        let dir = crate::testing::temp_dir("sealed-corrupt");
        let vault = store(&dir);
        vault.save(SECRET).unwrap();
        let good = std::fs::read(sealed(&dir)).unwrap();

        // One flipped bit anywhere: the header, the nonce, the body, the tag.
        for index in [0, 8, 20, good.len() - 1] {
            let mut bad = good.clone();
            bad[index] ^= 1;
            std::fs::write(sealed(&dir), &bad).unwrap();
            assert_eq!(vault.load(), None, "flip at {index}");
        }
        for bad in [&b""[..], &good[..8], &good[..15], b"not ours at all"] {
            std::fs::write(sealed(&dir), bad).unwrap();
            assert_eq!(vault.load(), None);
        }

        // A key that is the wrong size, or not the one that sealed it.
        std::fs::write(sealed(&dir), &good).unwrap();
        std::fs::write(dir.join("account.key"), [7u8; 16]).unwrap();
        assert_eq!(vault.load(), None);
        std::fs::write(dir.join("account.key"), [7u8; 32]).unwrap();
        assert_eq!(vault.load(), None);
        std::fs::remove_file(dir.join("account.key")).unwrap();
        assert_eq!(vault.load(), None);
        // And saving afterwards simply starts over.
        vault.save(b"fresh").unwrap();
        assert_eq!(vault.load().as_deref(), Some(&b"fresh"[..]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_file_says_what_kind_of_wrong_it_is() {
        let dir = crate::testing::temp_dir("sealed-classes");
        let vault = store(&dir);
        assert_eq!(vault.read(), Ok(None), "no file is not a problem");
        vault.save(SECRET).unwrap();
        let good = std::fs::read(sealed(&dir)).unwrap();

        std::fs::write(sealed(&dir), b"not ours at all").unwrap();
        assert_eq!(vault.read(), Err("format"));
        std::fs::write(sealed(&dir), &good[..12]).unwrap();
        assert_eq!(vault.read(), Err("format"), "too short for a nonce");

        let mut flipped = good.clone();
        *flipped.last_mut().unwrap() ^= 1;
        std::fs::write(sealed(&dir), &flipped).unwrap();
        assert_eq!(vault.read(), Err("decrypt"));

        std::fs::write(sealed(&dir), &good).unwrap();
        std::fs::remove_file(dir.join("account.key")).unwrap();
        assert_eq!(vault.read(), Err("key"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saves_racing_for_the_first_key_all_stay_readable() {
        let dir = crate::testing::temp_dir("sealed-race");
        let vault = Arc::new(store(&dir));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let vault = vault.clone();
                std::thread::spawn(move || vault.save(SECRET).unwrap())
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(vault.load().as_deref(), Some(SECRET));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_is_not_followed() {
        let dir = crate::testing::temp_dir("sealed-link");
        let vault = store(&dir);
        vault.save(SECRET).unwrap();
        let moved = dir.join("moved");
        std::fs::rename(sealed(&dir), &moved).unwrap();
        std::os::unix::fs::symlink(&moved, sealed(&dir)).unwrap();
        assert_eq!(vault.load(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_key_is_made_once() {
        let dir = crate::testing::temp_dir("sealed-key");
        let vault = store(&dir);
        vault.save(b"one").unwrap();
        let key = std::fs::read(dir.join("account.key")).unwrap();
        vault.save(b"two").unwrap();
        assert_eq!(std::fs::read(dir.join("account.key")).unwrap(), key);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
