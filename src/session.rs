use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tokio::{process::Command, sync::Mutex, time::sleep};
use uuid::Uuid;

const SESSION_TTL: Duration = Duration::from_secs(300);
const ACTIVE_TARGET_TIMEOUT: Duration = Duration::from_millis(200);
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
    owner: Option<String>,
    expires: Instant,
    paste_pending: bool,
    hidden: bool,
    paste_task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(task) = &self.paste_task {
            task.abort();
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
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
    pub async fn begin(&self, owner: Option<&str>) -> SessionView {
        self.remove_expired().await;
        let id = format!("session-{}", Uuid::new_v4());
        let target = active_target().await;
        let target_available = target.is_some();
        self.sessions.lock().await.insert(
            id.clone(),
            Session {
                target,
                owner: owner.map(str::to_owned),
                expires: Instant::now() + SESSION_TTL,
                paste_pending: false,
                hidden: false,
                paste_task: None,
            },
        );
        view(id, target_available, "active")
    }

    async fn with_session<T>(
        &self,
        id: &str,
        owner: Option<&str>,
        use_session: impl FnOnce(&mut Session) -> T,
    ) -> Result<T, &'static str> {
        let mut sessions = self.sessions.lock().await;
        let session = sessions
            .get_mut(id)
            .filter(|session| session.owner.as_deref() == owner && !session.hidden)
            .ok_or("Paste session is unknown or expired")?;
        if session.expires <= Instant::now() {
            sessions.remove(id);
            return Err("Paste session expired");
        }
        Ok(use_session(session))
    }

    pub async fn validate_paste(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> Result<bool, &'static str> {
        self.with_session(id, owner, |session| session.target.is_some())
            .await
    }

    /// Arm automatic paste only after the requested clipboard publication has
    /// succeeded. A session that disappears in the meantime degrades safely to
    /// copy-only behavior.
    pub async fn arm_paste(&self, id: &str, owner: Option<&str>) -> bool {
        self.with_session(id, owner, |session| {
            session.paste_pending = session.target.is_some();
            session.paste_pending
        })
        .await
        .unwrap_or(false)
    }

    pub async fn hidden(&self, id: &str, owner: Option<&str>) -> Result<SessionView, &'static str> {
        self.hide_with(id, owner, |target| tokio::spawn(paste_after_hidden(target)))
            .await
    }

    async fn hide_with(
        &self,
        id: &str,
        owner: Option<&str>,
        paste: impl FnOnce(Target) -> tokio::task::JoinHandle<()>,
    ) -> Result<SessionView, &'static str> {
        self.with_session(id, owner, |session| {
            // Keep the delayed task owned until it finishes or its owner releases
            // the session. Repeated hidden acknowledgements must never replay it.
            session.hidden = true;
            if session.paste_pending
                && let Some(target) = session.target.clone()
            {
                session.paste_task = Some(paste(target));
            }
            view(id.into(), session.target.is_some(), "hidden")
        })
        .await
    }

    pub async fn end(&self, id: &str, owner: Option<&str>) -> SessionView {
        let mut sessions = self.sessions.lock().await;
        let available = if sessions
            .get(id)
            .is_some_and(|session| session.owner.as_deref() == owner)
        {
            sessions
                .remove(id)
                .is_some_and(|session| session.target.is_some())
        } else {
            false
        };
        view(id.into(), available, "ended")
    }

    pub(crate) async fn release_owner(&self, owner: &str) {
        self.sessions
            .lock()
            .await
            .retain(|_, session| session.owner.as_deref() != Some(owner));
    }

    pub(crate) async fn owners(&self) -> Vec<String> {
        self.remove_expired().await;
        self.sessions
            .lock()
            .await
            .values()
            .filter_map(|session| session.owner.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) async fn test_target(&self) -> String {
        self.test_target_owned(None).await
    }

    #[cfg(test)]
    pub(crate) async fn test_target_owned(&self, owner: Option<&str>) -> String {
        let id = format!("targeted-{}", Uuid::new_v4());
        self.sessions.lock().await.insert(
            id.clone(),
            Session {
                target: Some(Target {
                    address: "0x123".into(),
                    class: "firefox".into(),
                }),
                owner: owner.map(str::to_owned),
                expires: Instant::now() + Duration::from_secs(30),
                paste_pending: false,
                hidden: false,
                paste_task: None,
            },
        );
        id
    }

    #[cfg(test)]
    pub(crate) async fn test_is_armed(&self, id: &str) -> bool {
        self.sessions.lock().await[id].paste_pending
    }

    async fn remove_expired(&self) {
        self.sessions.lock().await.retain(|_, session| {
            session.expires > Instant::now()
                && (!session.hidden
                    || session
                        .paste_task
                        .as_ref()
                        .is_some_and(|task| !task.is_finished()))
        });
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
    let mut command = Command::new("hyprctl");
    command.args(["-j", "activewindow"]).kill_on_drop(true);
    let output = tokio::time::timeout(ACTIVE_TARGET_TIMEOUT, command.output())
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
    command.kill_on_drop(true);
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
                .kill_on_drop(true)
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
    use super::{SessionManager, valid_window_address};

    async fn expired(manager: &SessionManager) -> String {
        let id = manager.test_target().await;
        assert!(manager.validate_paste(&id, None).await.unwrap());
        manager.sessions.lock().await.get_mut(&id).unwrap().expires = std::time::Instant::now();
        id
    }

    #[tokio::test]
    async fn foreign_owners_cannot_consume_sessions_and_release_cancels_delayed_paste() {
        let manager = SessionManager::default();
        let owner = Some(":1.1");
        let id = manager.test_target_owned(owner).await;
        let peer = manager.test_target_owned(Some(":1.2")).await;
        for foreign in [None, Some(":1.2")] {
            assert!(manager.validate_paste(&id, foreign).await.is_err());
            assert!(!manager.arm_paste(&id, foreign).await);
            assert!(manager.hidden(&id, foreign).await.is_err());
            assert!(!manager.end(&id, foreign).await.target_available);
            assert!(manager.validate_paste(&id, owner).await.unwrap());
        }
        assert!(manager.arm_paste(&id, owner).await);
        let (cancelled, cancellation) = tokio::sync::oneshot::channel::<()>();
        // Test the production task ownership without invoking any hardware helper.
        manager
            .hide_with(&id, owner, |_| {
                tokio::spawn(async move {
                    let _sender = cancelled;
                    std::future::pending::<()>().await;
                })
            })
            .await
            .unwrap();
        assert!(
            manager.hidden(&id, owner).await.is_err(),
            "hidden must be single-use"
        );
        assert!(!manager.arm_paste(&id, owner).await);
        manager.release_owner(":1.1").await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), cancellation)
                .await
                .unwrap()
                .is_err()
        );
        assert!(manager.validate_paste(&peer, Some(":1.2")).await.unwrap());
        assert!(manager.validate_paste(&id, owner).await.is_err());
    }

    #[tokio::test]
    async fn invalid_targets_and_expired_or_ended_sessions_cannot_trigger_paste() {
        assert!(!valid_window_address("0x0"));
        assert!(!valid_window_address("0x123'; os.execute('false')"));
        let manager = SessionManager::default();
        let id = expired(&manager).await;
        assert!(manager.hidden(&id, None).await.is_err());
        assert!(!manager.arm_paste(&id, None).await);
        let id = manager.test_target().await;
        assert!(manager.end(&id, None).await.target_available);
        assert!(!manager.arm_paste(&id, None).await);
        assert!(manager.hidden(&id, None).await.is_err());
        let id = expired(&manager).await;
        assert!(!manager.arm_paste(&id, None).await);
        assert!(manager.validate_paste(&id, None).await.is_err());
        let id = expired(&manager).await;
        let error = manager.validate_paste(&id, None).await.unwrap_err();
        assert_eq!(error, "Paste session expired");
    }
}
