//! Opt-in Windows integration probe. Uses the production service and Tauri
//! main-thread sampler, with an in-memory engine and its own native edit box.
//! No accounts, owner settings, clipboard conversion, autostart or WebViews.

#[cfg(windows)]
#[allow(dead_code)]
mod service;

#[cfg(windows)]
fn refresh_tray_icon(_: &tauri::AppHandle) {}

#[cfg(not(windows))]
fn main() {
    eprintln!("This probe requires Windows.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    use rekey_core::{config::Config, engine::Engine, model::Models};
    use rekey_hook::platform;
    use std::io::BufRead;
    use std::os::windows::process::CommandExt;
    use std::sync::{
        atomic::{AtomicI32, Ordering},
        Arc, Mutex,
    };
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    assert_eq!(
        std::env::var("MARKEY_NATIVE_INPUT_TEST").as_deref(),
        Ok("1"),
        "Set MARKEY_NATIVE_INPUT_TEST=1 to run the isolated interactive probe"
    );
    if std::env::args().any(|arg| arg == "--edit-target") {
        edit_target();
        return;
    }
    let status = Arc::new(AtomicI32::new(1));
    let result = status.clone();
    let mut context = tauri::generate_context!();
    context.config_mut().identifier = "com.marmos.markey.input-probe".into();
    context.config_mut().app.windows.clear();
    let app = tauri::Builder::default().setup(move |app| {
        let models = Models::load_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rekey-core/data/models"))?;
        if models.is_empty() { return Err("real language models required".into()); }
        let engine = Arc::new(Mutex::new(Engine::new(models, Config::default())));
        let previous = unsafe { GetForegroundWindow().0 as isize };
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .arg("--edit-target").creation_flags(0x08000000)
            .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).spawn()?;
        let mut line = String::new();
        std::io::BufReader::new(child.stdout.take().ok_or("helper stdout missing")?).read_line(&mut line)?;
        let target: isize = line.trim().parse()?;
        let hwnd = HWND(target as *mut _);
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)); }
        if pid != child.id() { let _ = child.kill(); return Err("helper window PID mismatch".into()); }
        service::restrict_probe_to_window(target);
        let runtime = service::spawn(app.handle(), engine.clone(), Arc::new(|_| {}))?;
        let handle = app.handle().clone();
        let result = result.clone();
        std::thread::spawn(move || {
            let mut finish = |code, message: &'static str| {
                unsafe { let _ = PostMessageW(Some(HWND(target as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)); }
                let _ = child.wait();
                eprintln!("{message}");
                result.store(code, Ordering::SeqCst);
                let quit = handle.clone();
                let _ = handle.run_on_main_thread(move || unsafe {
                    if platform::foreground_window() == target {
                        let _ = SetForegroundWindow(HWND(previous as *mut _));
                    }
                    quit.exit(code);
                });
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !runtime.running.load(Ordering::Relaxed) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if !runtime.running.load(Ordering::Relaxed) { finish(1, "FAIL: service hook did not start"); return; }
            std::thread::sleep(Duration::from_millis(250));
            let focus_deadline = Instant::now() + Duration::from_secs(45);
            while platform::foreground_window() != target && Instant::now() < focus_deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if platform::foreground_window() != target { finish(75, "SKIP: own probe window did not acquire focus; no input sent"); return; }
            let (ready, selected) = std::sync::mpsc::sync_channel(1);
            let _ = handle.run_on_main_thread(move || {
                let _ = ready.send(platform::with_injection_target(target, || platform::select_layout("us")) == Some(true));
            });
            if selected.recv_timeout(Duration::from_secs(1)) != Ok(true) {
                finish(1, "FAIL: could not activate US layout in the owned window"); return;
            }
            // Let the real sampler publish the test window's layout.
            std::thread::sleep(Duration::from_millis(200));
            let modifiers = [VK_LCONTROL, VK_RCONTROL, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN, VK_LSHIFT, VK_RSHIFT];
            if modifiers.iter().any(|vk| unsafe { GetAsyncKeyState(vk.0 as i32) < 0 }) {
                finish(75, "SKIP: physical modifier held; no input sent"); return;
            }
            let mut inputs = Vec::new();
            for vk in b"GHBDTN " {
                for up in [false, true] {
                    inputs.push(INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(*vk as u16), wScan: unsafe { MapVirtualKeyW(*vk as u32, MAPVK_VK_TO_VSC) as u16 },
                        dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
                        time: 0, dwExtraInfo: 0x5052_4F42,
                    } } });
                }
            }
            if platform::foreground_window() != target { finish(75, "SKIP: probe lost focus; no input sent"); return; }
            let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
            if sent != inputs.len() as u32 { finish(1, "FAIL: Windows rejected scripted input"); return; }
            let deadline = Instant::now() + Duration::from_secs(4);
            let mut corrected = false;
            while Instant::now() < deadline {
                if platform::foreground_window() != target { finish(75, "SKIP: probe lost focus during correction"); return; }
                let mut text = [0u16; 128];
                let mut length = 0usize;
                let read = unsafe { SendMessageTimeoutW(HWND(target as *mut _), WM_GETTEXT,
                    WPARAM(text.len()), LPARAM(text.as_mut_ptr() as isize), SMTO_ABORTIFHUNG, 200, Some(&mut length)) };
                corrected = read.0 != 0 && length < text.len() && String::from_utf16_lossy(&text[..length]) == "привет ";
                if corrected { break; }
                std::thread::sleep(Duration::from_millis(20));
            }
            let health = runtime.health.snapshot();
            let current = platform::current_context();
            eprintln!("DIAGNOSTIC: hook_events={} skipped={} engine_corrections={} applied={} secure={} us_layout={} corrected={}",
                health.events, health.skipped_busy, engine.lock().map(|engine| engine.stats().corrections).unwrap_or(0),
                runtime.applied.load(Ordering::Relaxed), current.secure_input, current.layout.as_deref() == Some("us"), corrected);
            // Report only generated-scenario result, never captured text.
            if corrected && runtime.applied.load(Ordering::Relaxed) == 1 {
                finish(0, "PASS: real Tauri context sampler + KeyRouter + hook + injection corrected the owned window");
            } else {
                finish(1, "FAIL: full service did not correct the scripted word");
            }
        });
        Ok(())
    }).build(context).expect("build isolated probe runtime");
    app.run_return(|_, _| {});
    std::process::exit(status.load(Ordering::SeqCst));
}

#[cfg(windows)]
fn edit_target() {
    use std::io::Write;
    use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::w!("EDIT"),
            windows::core::w!(""),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            80,
            80,
            560,
            200,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        println!("{}", hwnd.0 as isize);
        std::io::stdout().flush().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while IsWindow(Some(hwnd)).as_bool() && std::time::Instant::now() < deadline {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let _ = DestroyWindow(hwnd);
    }
}
