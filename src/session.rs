use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;
use serde_json::Value;
use tokio::{process::Command, sync::Mutex, time::sleep};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize)]
pub struct SessionView {
    pub id: String,
    pub target_available: bool,
    pub paste_mode: &'static str,
    pub expires_in_ms: u64,
    pub state: &'static str,
}

#[derive(Debug, Clone)]
struct Session {
    target: Option<Target>,
    expires: Instant,
    paste_pending: bool,
}

#[derive(Debug, Clone)]
struct Target {
    address: String,
    class: String,
}

#[derive(Default)]
pub struct SessionManager {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
}

impl SessionManager {
    pub async fn begin(&self) -> SessionView {
        self.remove_expired().await;
        let id = format!("session-{}", Uuid::new_v4());
        let target = active_target().await;
        self.sessions.lock().await.insert(
            id.clone(),
            Session {
                target: target.clone(),
                expires: Instant::now() + Duration::from_secs(15),
                paste_pending: false,
            },
        );
        view(id, target.is_some(), "active")
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
        expires_in_ms: 15_000,
        state,
    }
}

async fn active_target() -> Option<Target> {
    let output = Command::new("hyprctl")
        .args(["-j", "activewindow"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value: Value = serde_json::from_slice(&output.stdout).ok()?;
    let address = value["address"].as_str()?.trim().to_owned();
    if address.is_empty() || address == "0x0" {
        return None;
    }
    Some(Target {
        address,
        class: value["class"].as_str().unwrap_or_default().to_owned(),
    })
}

async fn paste_after_hidden(target: Target) {
    sleep(Duration::from_millis(220)).await;
    let selector = format!("address:{}", target.address);
    let _ = Command::new("hyprctl")
        .args(["dispatch", "focuswindow", &selector])
        .status()
        .await;
    let shortcut = if is_terminal(&target.class) {
        "CTRL SHIFT,V"
    } else {
        "CTRL,V"
    };
    let status = Command::new("hyprctl")
        .args(["dispatch", "sendshortcut", shortcut, &selector])
        .status()
        .await;
    if !status.is_ok_and(|value| value.success()) {
        let _ = Command::new("notify-send")
            .args([
                "-a",
                "Clipboard",
                "Copied; paste manually",
                "The original target is unavailable.",
            ])
            .status()
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
    use super::{SessionManager, is_terminal};

    #[test]
    fn terminal_targets_use_terminal_paste_shortcut() {
        assert!(is_terminal("com.mitchellh.ghostty"));
        assert!(is_terminal("org.kde.konsole"));
        assert!(!is_terminal("firefox"));
    }

    #[tokio::test]
    async fn stale_sessions_never_prepare_a_paste() {
        assert!(
            SessionManager::default()
                .prepare_paste("missing")
                .await
                .is_err()
        );
    }
}
