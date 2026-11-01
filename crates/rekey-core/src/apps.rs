//! Per-application quirks: what some apps need done differently.
//!
//! Identifiers are macOS bundle ids or Windows executable names, compared
//! without regard to case, like the exclusion list.

use std::time::Duration;

/// The pause between injected keystrokes for most apps.
pub const DEFAULT_KEYSTROKE_GAP: Duration = Duration::from_millis(3);

/// The pause in browsers and Electron chat apps. Their text fields run in a
/// renderer process that can fall behind a burst of synthetic keys and drop
/// some of them — a backspace too few, or a lost letter of the replacement.
pub const SLOW_KEYSTROKE_GAP: Duration = Duration::from_millis(8);

/// Browsers and Electron apps whose text fields are web pages.
const WEB_TEXT_APPS: &[&str] = &[
    "com.google.Chrome",
    "com.google.Chrome.canary",
    "com.apple.Safari",
    "com.apple.SafariTechnologyPreview",
    "org.mozilla.firefox",
    "company.thebrowser.Browser",
    "com.microsoft.edgemac",
    "com.brave.Browser",
    "com.operasoftware.Opera",
    "com.vivaldi.Vivaldi",
    "ru.yandex.desktop.yandex-browser",
    "com.tinyspeck.slackmacgap",
    "com.hnc.Discord",
    "net.whatsapp.WhatsApp",
    "com.microsoft.teams2",
    "notion.id",
    "md.obsidian",
    "chrome.exe",
    "msedge.exe",
    "firefox.exe",
    "browser.exe",
    "brave.exe",
    "opera.exe",
    "vivaldi.exe",
    "slack.exe",
    "discord.exe",
    "ms-teams.exe",
    "whatsapp.exe",
    "notion.exe",
    "obsidian.exe",
];

/// Apps where Return must never be held back and replayed. Outlook treats
/// the replayed Return differently from a typed one (it can send the message
/// or lose the line break), so a word there is never corrected on Return.
const NO_HELD_RETURN_APPS: &[&str] = &["com.microsoft.Outlook", "outlook.exe", "olk.exe"];

fn listed(list: &[&str], app: Option<&str>) -> bool {
    app.is_some_and(|app| list.iter().any(|a| a.eq_ignore_ascii_case(app)))
}

/// How long to wait between injected keystrokes in `app`.
pub fn keystroke_gap(app: Option<&str>) -> Duration {
    if listed(WEB_TEXT_APPS, app) {
        SLOW_KEYSTROKE_GAP
    } else {
        DEFAULT_KEYSTROKE_GAP
    }
}

/// Whether Return may be held back to correct the word before it in `app`.
pub fn holds_return(app: Option<&str>) -> bool {
    !listed(NO_HELD_RETURN_APPS, app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsers_and_web_chats_get_a_slower_pace() {
        assert_eq!(keystroke_gap(Some("com.google.Chrome")), SLOW_KEYSTROKE_GAP);
        assert_eq!(keystroke_gap(Some("CHROME.EXE")), SLOW_KEYSTROKE_GAP);
        assert_eq!(
            keystroke_gap(Some("ru.keepcoder.Telegram")),
            DEFAULT_KEYSTROKE_GAP
        );
        assert_eq!(keystroke_gap(None), DEFAULT_KEYSTROKE_GAP);
    }

    #[test]
    fn outlook_never_holds_return() {
        assert!(!holds_return(Some("com.microsoft.Outlook")));
        assert!(!holds_return(Some("OUTLOOK.EXE")));
        assert!(holds_return(Some("com.apple.mail")));
        assert!(holds_return(None));
    }
}
