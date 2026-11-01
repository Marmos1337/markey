//! Where the Anthropic API key lives: the macOS Keychain or the Windows
//! Credential Manager, never the settings file.
//!
//! Older builds kept the key in `settings.json`, readable by any process of
//! the user. [`crate::settings`] moves it here on first launch and rewrites
//! the file without it.

/// One secret, stored by the OS.
pub trait SecretStore: Send + Sync {
    /// The stored key, or `None` when there is none.
    fn get(&self) -> Result<Option<String>, String>;
    /// Store `value`, replacing any previous one.
    fn set(&self, value: &str) -> Result<(), String>;
    /// Remove the key; not an error when there is none.
    fn delete(&self) -> Result<(), String>;
}

/// Keychain service / credential target name.
pub const SERVICE: &str = "com.marmos.markey";
/// Keychain account: what the secret is.
pub const ACCOUNT: &str = "anthropic-api-key";

/// The platform's own store.
pub fn system() -> Box<dyn SecretStore> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::Keychain)
    }
    #[cfg(target_os = "windows")]
    {
        Box::new(windows::CredentialManager)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Box::new(Unavailable)
    }
}

/// No OS store on this platform: the key cannot be kept, so the assist stays
/// off rather than writing the key to a file.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
struct Unavailable;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
impl SecretStore for Unavailable {
    fn get(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn set(&self, _value: &str) -> Result<(), String> {
        Err("no secure storage on this platform".into())
    }
    fn delete(&self) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{SecretStore, ACCOUNT, SERVICE};
    use core_foundation::base::{CFType, CFTypeRef, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::data::{CFData, CFDataRef};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::{CFString, CFStringRef};

    type OSStatus = i32;
    const ERR_SEC_SUCCESS: OSStatus = 0;
    const ERR_SEC_ITEM_NOT_FOUND: OSStatus = -25300;
    const ERR_SEC_DUPLICATE_ITEM: OSStatus = -25299;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecClass: CFStringRef;
        static kSecClassGenericPassword: CFStringRef;
        static kSecAttrService: CFStringRef;
        static kSecAttrAccount: CFStringRef;
        static kSecValueData: CFStringRef;
        static kSecReturnData: CFStringRef;
        static kSecMatchLimit: CFStringRef;
        static kSecMatchLimitOne: CFStringRef;
        static kSecAttrAccessible: CFStringRef;
        static kSecAttrAccessibleWhenUnlockedThisDeviceOnly: CFStringRef;
        fn SecItemCopyMatching(query: CFTypeRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemAdd(attributes: CFTypeRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemUpdate(query: CFTypeRef, attributes: CFTypeRef) -> OSStatus;
        fn SecItemDelete(query: CFTypeRef) -> OSStatus;
    }

    pub struct Keychain;

    fn s(r: CFStringRef) -> CFString {
        unsafe { CFString::wrap_under_get_rule(r) }
    }

    /// The pairs that identify MarKey's item.
    fn identity() -> Vec<(CFString, CFType)> {
        unsafe {
            vec![
                (s(kSecClass), s(kSecClassGenericPassword).as_CFType()),
                (s(kSecAttrService), CFString::new(SERVICE).as_CFType()),
                (s(kSecAttrAccount), CFString::new(ACCOUNT).as_CFType()),
            ]
        }
    }

    fn dict(pairs: Vec<(CFString, CFType)>) -> CFDictionary<CFString, CFType> {
        CFDictionary::from_CFType_pairs(&pairs)
    }

    fn failed(what: &str, status: OSStatus) -> String {
        format!("Keychain {what} failed (OSStatus {status})")
    }

    impl SecretStore for Keychain {
        fn get(&self) -> Result<Option<String>, String> {
            let mut pairs = identity();
            unsafe {
                pairs.push((s(kSecReturnData), CFBoolean::true_value().as_CFType()));
                pairs.push((s(kSecMatchLimit), s(kSecMatchLimitOne).as_CFType()));
            }
            let query = dict(pairs);
            let mut out: CFTypeRef = std::ptr::null();
            let status = unsafe { SecItemCopyMatching(query.as_CFTypeRef(), &mut out) };
            match status {
                ERR_SEC_SUCCESS if !out.is_null() => {
                    let data = unsafe { CFData::wrap_under_create_rule(out as CFDataRef) };
                    String::from_utf8(data.bytes().to_vec())
                        .map(Some)
                        .map_err(|_| "the stored key is not text".to_string())
                }
                ERR_SEC_SUCCESS | ERR_SEC_ITEM_NOT_FOUND => Ok(None),
                other => Err(failed("read", other)),
            }
        }

        fn set(&self, value: &str) -> Result<(), String> {
            let data = CFData::from_buffer(value.as_bytes());
            let mut attributes = identity();
            attributes.push((unsafe { s(kSecValueData) }, data.as_CFType()));
            // Readable only while the Mac is unlocked, and never synced or
            // restored onto another machine.
            attributes.push(unsafe {
                (
                    s(kSecAttrAccessible),
                    s(kSecAttrAccessibleWhenUnlockedThisDeviceOnly).as_CFType(),
                )
            });
            let status =
                unsafe { SecItemAdd(dict(attributes).as_CFTypeRef(), std::ptr::null_mut()) };
            match status {
                ERR_SEC_SUCCESS => Ok(()),
                ERR_SEC_DUPLICATE_ITEM => {
                    let change = dict(vec![(unsafe { s(kSecValueData) }, data.as_CFType())]);
                    let status = unsafe {
                        SecItemUpdate(dict(identity()).as_CFTypeRef(), change.as_CFTypeRef())
                    };
                    if status == ERR_SEC_SUCCESS {
                        Ok(())
                    } else {
                        Err(failed("update", status))
                    }
                }
                other => Err(failed("write", other)),
            }
        }

        fn delete(&self) -> Result<(), String> {
            match unsafe { SecItemDelete(dict(identity()).as_CFTypeRef()) } {
                ERR_SEC_SUCCESS | ERR_SEC_ITEM_NOT_FOUND => Ok(()),
                other => Err(failed("delete", other)),
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{SecretStore, ACCOUNT, SERVICE};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    pub struct CredentialManager;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn target() -> Vec<u16> {
        wide(&format!("{SERVICE}/{ACCOUNT}"))
    }

    fn not_found(e: &windows::core::Error) -> bool {
        e.code() == ERROR_NOT_FOUND.to_hresult()
    }

    impl SecretStore for CredentialManager {
        fn get(&self) -> Result<Option<String>, String> {
            let target = target();
            let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
            let read =
                unsafe { CredReadW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None, &mut cred) };
            if let Err(e) = read {
                return if not_found(&e) {
                    Ok(None)
                } else {
                    Err(e.to_string())
                };
            }
            let value = unsafe {
                let c = &*cred;
                let bytes =
                    std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize);
                let text = String::from_utf8(bytes.to_vec());
                CredFree(cred as *const _);
                text
            };
            value
                .map(Some)
                .map_err(|_| "the stored key is not text".to_string())
        }

        fn set(&self, value: &str) -> Result<(), String> {
            let mut target = target();
            let mut user = wide(ACCOUNT);
            let mut blob = value.as_bytes().to_vec();
            let cred = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: PWSTR(target.as_mut_ptr()),
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: blob.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                UserName: PWSTR(user.as_mut_ptr()),
                ..Default::default()
            };
            unsafe { CredWriteW(&cred, 0) }.map_err(|e| e.to_string())
        }

        fn delete(&self) -> Result<(), String> {
            let target = target();
            match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
                Ok(()) => Ok(()),
                Err(e) if not_found(&e) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        }
    }
}

/// An in-memory store for tests.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore {
    pub value: std::sync::Mutex<Option<String>>,
    pub fail: bool,
}

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn get(&self) -> Result<Option<String>, String> {
        Ok(self.value.lock().unwrap().clone())
    }
    fn set(&self, value: &str) -> Result<(), String> {
        if self.fail {
            return Err("locked".into());
        }
        *self.value.lock().unwrap() = Some(value.to_string());
        Ok(())
    }
    fn delete(&self) -> Result<(), String> {
        *self.value.lock().unwrap() = None;
        Ok(())
    }
}
