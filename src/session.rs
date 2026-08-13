use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::Mutex, time::sleep};
use uuid::Uuid;

const SESSION_TTL: Duration = Duration::from_secs(300);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const SESSION_TTL_MS: u64 = 300_000;

#[derive(Debug, Serialize)]
pub struct SessionView {
    pub id: String,
    pub target_available: bool,
    pub paste_mode: &'static str,
    pub expires_in_ms: u64,
    pub state: &'static str,
}

#[derive(Debug)]
struct Session {
    target: Option<Target>,
    expires: Instant,
    paste_pending: bool,
}

#[derive(Debug, Deserialize)]
struct Target {
    address: String,
    #[serde(default)]
    class: String,
}

#[derive(Default)]
pub struct SessionManager {
    sessions: Mutex<HashMap<String, Session>>,
}

impl SessionManager {
    pub async fn begin(&self) -> SessionView {
        self.remove_expired().await;
        let id = format!("session-{}", Uuid::new_v4());
        let target = active_target().await;
        let target_available = target.is_some();
        self.sessions.lock().await.insert(
            id.clone(),
            Session {
                target,
                expires: Instant::now() + SESSION_TTL,
                paste_pending: false,
            },
        );
        view(id, target_available, "active")
    }

    pub async fn prepare_paste(&self, id: &str) -> Result<bool, &'static str> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .ok_or("Paste session is unknown or expired")?;
        if session.expires <= Instant::now() {
            sessions.remove(id);
            return Err("Paste session expired");
        }
        session.paste_pending = true;
        Ok(session.target.is_some())
    }

    pub async fn hidden(&self, id: &str) -> Result<SessionView, &'static str> {
        let session = self
            .sessions
            .lock()
            .await
            .remove(id)
            .ok_or("Paste session is unknown or expired")?;
        let available = session.target.is_some();
        if session.expires <= Instant::now() {
            return Err("Paste session expired");
        }
        if session.paste_pending
            && let Some(target) = session.target
        {
            tokio::spawn(paste_after_hidden(target));
        }
        Ok(view(id.into(), available, "hidden"))
    }

    pub async fn end(&self, id: &str) -> SessionView {
        let available = self
            .sessions
            .lock()
            .await
            .remove(id)
            .is_some_and(|session| session.target.is_some());
        view(id.into(), available, "ended")
    }

    async fn remove_expired(&self) {
        self.sessions
            .lock()
            .await
            .retain(|_, session| session.expires > Instant::now());
    }
}

fn view(id: String, target_available: bool, state: &'static str) -> SessionView {
    SessionView {
        id,
        target_available,
        paste_mode: if target_available {
            "copy-paste"
        } else {
            "copy-only"
        },
        expires_in_ms: SESSION_TTL_MS,
        state,
    }
}

async fn active_target() -> Option<Target> {
    let output = tokio::time::timeout(
        COMMAND_TIMEOUT,
        Command::new("hyprctl")
            .args(["-j", "activewindow"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut target: Target = serde_json::from_slice(&output.stdout).ok()?;
    target.address = target.address.trim().to_owned();
    valid_window_address(&target.address).then_some(target)
}

fn valid_window_address(address: &str) -> bool {
    address.strip_prefix("0x").is_some_and(|value| {
        !value.is_empty() && value.chars().all(|character| character.is_ascii_hexdigit())
    }) && address != "0x0"
}

async fn command_output(command: &mut Command) -> std::io::Result<std::process::Output> {
    tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "command timed out"))?
}

async fn hyprland_dispatch(lua: &str, legacy_dispatcher: &str, legacy_argument: &str) -> bool {
    if dispatch_succeeded(command_output(Command::new("hyprctl").args(["dispatch", lua])).await) {
        return true;
    }
    dispatch_succeeded(
        command_output(Command::new("hyprctl").args([
            "dispatch",
            legacy_dispatcher,
            legacy_argument,
        ]))
        .await,
    )
}

fn dispatch_succeeded(output: std::io::Result<std::process::Output>) -> bool {
    output.is_ok_and(|value| {
        value.status.success() && String::from_utf8_lossy(&value.stdout).trim() == "ok"
    })
}

async fn paste_after_hidden(target: Target) {
    sleep(Duration::from_millis(600)).await;
    let selector = format!("address:{}", target.address);
    let lua_focus = format!("hl.dsp.focus({{ window = '{selector}' }})");
    let _ = hyprland_dispatch(&lua_focus, "focuswindow", &selector).await;
    let modifiers = if is_terminal(&target.class) {
        "CTRL SHIFT"
    } else {
        "CTRL"
    };
    let legacy_shortcut = format!("{modifiers},V,{selector}");
    let lua_shortcut = format!(
        "hl.dsp.send_shortcut({{ mods = '{modifiers}', key = 'V', window = '{selector}' }})"
    );
    if !hyprland_dispatch(&lua_shortcut, "sendshortcut", &legacy_shortcut).await {
        let _ = tokio::time::timeout(
            COMMAND_TIMEOUT,
            Command::new("notify-send")
                .args([
                    "-a",
                    "Clipboard",
                    "Copied; paste manually",
                    "The original target is unavailable.",
                ])
                .status(),
        )
        .await;
    }
}

fn is_terminal(class: &str) -> bool {
    matches!(
        class.to_ascii_lowercase().as_str(),
        "com.mitchellh.ghostty"
            | "ghostty"
            | "alacritty"
            | "kitty"
            | "foot"
            | "footclient"
            | "org.wezfurlong.wezterm"
            | "org.gnome.terminal"
            | "org.gnome.console"
            | "org.kde.konsole"
            | "konsole"
            | "ptyxis"
    )
}

#[cfg(test)]
mod tests {
    use super::{SessionManager, is_terminal, valid_window_address};

    #[test]
    fn terminal_targets_use_terminal_paste_shortcut() {
        assert!(is_terminal("com.mitchellh.ghostty"));
        assert!(is_terminal("org.kde.konsole"));
        assert!(!is_terminal("firefox"));
    }

    #[test]
    fn paste_targets_require_hyprland_window_addresses() {
        assert!(valid_window_address("0x123abc"));
        assert!(!valid_window_address("0x0"));
        assert!(!valid_window_address("123abc"));
        assert!(!valid_window_address("0x123'; os.execute('false')"));
    }

    #[tokio::test]
    async fn stale_sessions_never_prepare_a_paste() {
        let result = SessionManager::default().prepare_paste("missing").await;
        assert!(result.is_err());
    }
}
