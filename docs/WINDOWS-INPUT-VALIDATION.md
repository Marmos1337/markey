# Windows input changes

Mouse observation now uses asynchronous Raw Input on a dedicated message loop.
Only clicks and the first drag movement reach the keyboard/engine thread. The
previous global low-level mouse hook ran synchronously for every mouse packet,
including high-polling-rate movement in games. The keyboard hook is still needed
to hold a word boundary during correction.

The observer saves the process's previous mouse registration (including Tao's),
restores it on shutdown, and leaves a newer registrant alone. It does not request
`RIDEV_NOLEGACY`, capture the mouse, or register for raw keyboard input.

Known game executables, including CS2 and Dota 2, are added to default exclusions
and migrated once for existing profiles. Users can remove or add exclusions in
the existing settings. In excluded applications, MarKey bypasses the keyboard
engine and pauses UI Automation polling. It also passes input through when the
engine lock is unavailable, including when an earlier correction is still queued.

Queued corrections retain the originating HWND. A different foreground window
cancels their injection, and this guard travels to Tauri's main thread for layout
operations. Changing between two windows of the same executable invalidates the
old word buffer. Injected chords release only modifiers they pressed themselves;
normal physical key-up events continue through the hook.

## Checks

```powershell
cargo test --locked -p rekey-hook --lib
cargo test --locked --release -p rekey-core --lib
cargo test --locked --release -p rekey-app --bin markey
```

Generate language models first as described in the project README. The native
tests do not read saved owner settings or account credentials.

The opt-in hook test creates its own native edit window and injects the scripted
physical keys `ghbdtn `. The real keyboard callback translates those events,
the engine holds the space, and the injector produces `привет `. A stale target
is also checked. It aborts if its window does not acquire focus.

```powershell
$env:MARKEY_NATIVE_INPUT_TEST = '1'
cargo test --locked -p rekey-hook --lib windows::tests::native_correction_in_owned_edit_window -- --ignored --exact
```

The separate integration probe uses the production `service::spawn`, `KeyRouter`,
injector and Tauri main-thread context sampler. It has an in-memory engine and a
native edit window in a separate owned helper process. This matters because Tao's
raw keyboard registration can hide low-level hook events while a window of the
same process has focus. The helper never starts Tauri. The probe does not load
accounts, owner settings, autostart,
WebViews or clipboard conversion. The test-only feature limits routing to that
window; changing focus aborts the scenario. If Windows declines foreground
activation, select the probe window within 45 seconds; the probe never forces it.

```powershell
$env:MARKEY_NATIVE_INPUT_TEST = '1'
cargo run --locked --release -p rekey-app --features native-input-smoke --bin markey-input-smoke
```

## Observed results and limits

- A Windows hook-level smoke passed against both the original 0.3.1 source and
  the changed implementation. The reported earlier failure to correct text was
  not reproduced by that scenario; the smoke alone is not evidence of its cause.
- The production-service probe passed against both original 0.3.1 and changed
  implementations with the separate helper process:
  seven scripted keydown events, zero skipped events, one detected correction and
  one applied correction. The resulting text matched the scenario, and the active
  layout switched away from US. This also did not reproduce a global failure to
  correct text.
- The Windows verification run passed 16 hook tests (one interactive test ignored
  in the ordinary suite), 109 core tests and 190 application tests. The interactive
  hook test and the production-service probe were then run explicitly.
- A synthetic 80,000-packet idle-mouse test takes approximately 1 ms in the debug
  test build and produces no engine notifications. This measures event handling,
  not CS2 frame rate, physical mouse latency, or an anticheat environment.
- Unit regressions cover injected key balance and held modifiers, mouse drag and
  double-click behavior, registration restoration/handoff, excluded gameplay
  while the queue is busy or the engine is locked/poisoned, cross-thread HWND
  guards, and stale words after changing windows of the same executable.
- No evidence links the separately reported physical D-key latch to MarKey.
  Hardware release behavior and actual gameplay require a separate reproduction.

Microsoft documents the synchronous timeout and recommends Raw Input for
asynchronous observation in [LowLevelMouseProc](https://learn.microsoft.com/en-us/windows/win32/winmsg/lowlevelmouseproc).
Registration is process-wide, with only one target per device class; see
[RegisterRawInputDevices](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerrawinputdevices).
