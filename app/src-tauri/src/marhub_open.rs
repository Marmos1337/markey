//! Opening a `marhub://` link so that only the real MarHub can receive it.
//!
//! Any program can register the `marhub:` scheme, and the link carries a
//! sign-in attempt. So MarKey does not ask the system to handle the link: it
//! names MarHub itself, and when that fails it goes to the browser instead.

#[cfg(target_os = "macos")]
const HUB_BUNDLE_ID: &str = "ru.marhub.hub";
#[cfg(any(target_os = "macos", test))]
const HUB_APP: &str = "/Applications/MarHub.app";

/// Is where Launch Services found MarHub the one place it gets installed?
/// A bundle id can be copied by any app anywhere on the disk.
#[cfg(any(target_os = "macos", test))]
fn is_installed_hub(path: Option<&str>) -> bool {
    path.is_some_and(|path| path.trim_end_matches('/') == HUB_APP)
}

/// Hand `url` to MarHub. `silent` must not take focus or show a window. False
/// when MarHub is not there to open.
pub fn open_hub(url: &str, silent: bool) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::open(url, silent)
    }
    #[cfg(target_os = "windows")]
    {
        windows::open(url, silent)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (url, silent);
        false
    }
}

/// The executable named by a `shell\open\command` value, when it is MarHub
/// where the current-user installer puts it. Plain text handling so that it
/// can be tested anywhere; the paths are always Windows paths.
#[cfg(any(target_os = "windows", test))]
fn hub_executable(command: &str, local_app_data: &str) -> Option<String> {
    let command = command.trim();
    let path = match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        // Unquoted, a path with a space in it cannot be told from arguments.
        // That case refuses, and sign-in falls back to the browser.
        None => command.split_whitespace().next()?,
    };
    let path = path.replace('/', "\\");
    let root = local_app_data.replace('/', "\\");
    let root = root.trim_end_matches('\\');
    if root.is_empty() {
        return None;
    }
    let expected = format!("{root}\\MarHub\\MarHub.exe");
    (path.to_lowercase() == expected.to_lowercase()).then_some(path)
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{is_installed_hub, HUB_APP, HUB_BUNDLE_ID};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// `open` returns as soon as Launch Services has the request. If it ever
    /// hangs, MarKey goes on without MarHub rather than waiting with it.
    const LIMIT: Duration = Duration::from_secs(10);

    pub fn open(url: &str, silent: bool) -> bool {
        let found = rekey_hook::platform::application_path(HUB_BUNDLE_ID);
        if !is_installed_hub(found.as_deref()) {
            return false;
        }
        let mut command = Command::new("/usr/bin/open");
        if silent {
            // Without -g the app MarKey opens would take the foreground.
            command.arg("-g");
        }
        command
            .args(["-a", HUB_APP])
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        let started = Instant::now();
        loop {
            match child.try_wait() {
                // Non-zero: no application with that bundle id.
                Ok(Some(status)) => return status.success(),
                Ok(None) if started.elapsed() < LIMIT => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use super::hub_executable;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    /// More than any command line that registers a scheme.
    const MAX_VALUE: u32 = 8192;

    /// The default value of the per-user `marhub` scheme handler.
    fn registered_command() -> Option<String> {
        let key = w!(r"Software\Classes\marhub\shell\open\command");
        let mut size = 0u32;
        // SAFETY: `key` is a static wide string; with no data pointer the call
        // only writes the size.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key,
                PCWSTR::null(),
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut size),
            )
        };
        if status != ERROR_SUCCESS || size == 0 || size > MAX_VALUE {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        // SAFETY: `buffer` holds at least `size` bytes, and `size` tells the
        // call so.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key,
                PCWSTR::null(),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        let length = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
        String::from_utf16(&buffer[..length]).ok()
    }

    pub fn open(url: &str, silent: bool) -> bool {
        let Some(root) = dirs::data_local_dir() else {
            return false;
        };
        let Some(command) = registered_command() else {
            return false;
        };
        let Some(exe) = hub_executable(&command, &root.to_string_lossy()) else {
            log::warn!("the marhub: handler is not MarHub; not using it");
            return false;
        };
        let mut process = Command::new(exe);
        // The link is one argument, and there is no shell to reinterpret it.
        process
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if silent {
            process.creation_flags(CREATE_NO_WINDOW);
        }
        process.spawn().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = r"C:\Users\Степан\AppData\Local";

    #[test]
    fn on_macos_only_the_applications_folder_counts() {
        assert!(is_installed_hub(Some("/Applications/MarHub.app")));
        assert!(is_installed_hub(Some("/Applications/MarHub.app/")));
        for bad in [
            "/Users/someone/Applications/MarHub.app",
            "/Users/someone/Downloads/MarHub.app",
            "/Volumes/Evil/Applications/MarHub.app",
            "/Applications/MarHub.app.evil",
            "/Applications/Other.app",
            "",
        ] {
            assert!(!is_installed_hub(Some(bad)), "{bad}");
        }
        assert!(!is_installed_hub(None));
    }

    #[test]
    fn the_installed_marhub_is_recognised() {
        let path = r"C:\Users\Степан\AppData\Local\MarHub\MarHub.exe";
        let quoted = format!("\"{path}\" \"%1\"");
        assert_eq!(hub_executable(&quoted, ROOT).as_deref(), Some(path));
        assert_eq!(
            hub_executable(&format!("{path} \"%1\""), ROOT).as_deref(),
            Some(path)
        );
        // Windows paths ignore case, and the root may end in a slash.
        let upper = r#""C:\USERS\СТЕПАН\APPDATA\LOCAL\MARHUB\MARHUB.EXE" "%1""#;
        assert!(hub_executable(upper, ROOT).is_some());
        assert!(hub_executable(&quoted, &format!("{ROOT}\\")).is_some());
    }

    #[test]
    fn anything_else_claiming_the_scheme_is_refused() {
        for bad in [
            r#""C:\Users\Степан\AppData\Local\Evil\Evil.exe" "%1""#,
            r#""C:\Program Files\MarHub\MarHub.exe" "%1""#,
            r#""C:\Users\Степан\AppData\LocalLow\MarHub\MarHub.exe" "%1""#,
            r#""C:\Users\Степан\AppData\Local\..\..\Evil\MarHub.exe" "%1""#,
            r#""C:\Users\Степан\AppData\Local\MarHub\NotMarHub.exe" "%1""#,
            // Under the folder, but not the place the installer puts it.
            r#""C:\Users\Степан\AppData\Local\Other\MarHub\MarHub.exe" "%1""#,
            r#""C:\Users\Степан\AppData\Local\MarHub\sub\MarHub.exe" "%1""#,
            r#""C:\Users\Степан\AppData\Local\MarHub.exe" "%1""#,
            r#""C:\Users\Степан\AppData\Local\MarHub\MarHub.exe.bat" "%1""#,
            // A space in an unquoted path: where it ends is anyone's guess.
            r#"C:\Users\A B\AppData\Local\MarHub\MarHub.exe "%1""#,
            r#"cmd /c start MarHub.exe"#,
            "",
            "\"",
        ] {
            assert_eq!(hub_executable(bad, ROOT), None, "{bad}");
        }
        assert_eq!(
            hub_executable(r#""C:\MarHub.exe""#, ""),
            None,
            "no root to compare with"
        );
    }
}
