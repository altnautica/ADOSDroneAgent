//! Detection of a live desktop session to open the kiosk window in.
//!
//! When a display manager runs a graphical session, its compositor holds the
//! DRM master, so the kiosk opens a full-screen browser window inside that
//! session instead of starting cage. With no session the kiosk owns the
//! display through cage.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ados_proc::run::run;

/// Display managers whose presence means a desktop session is up or coming.
pub const DISPLAY_MANAGERS: &[&str] = &["sddm", "gdm", "gdm3", "lightdm", "lxdm", "greetd"];

/// How long to wait for a starting session before falling back to cage. The
/// kiosk starts at multi-user.target, before the desktop, so this is generous;
/// it returns as soon as the session is ready.
pub const SESSION_WAIT: Duration = Duration::from_secs(90);
pub const SESSION_POLL: Duration = Duration::from_millis(1500);

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The per-user runtime dir root (`/run/user/<uid>`).
pub const RUN_USER_ROOT: &str = "/run/user";
/// The X11 socket dir (`/tmp/.X11-unix/X<n>`).
pub const X11_SOCKET_DIR: &str = "/tmp/.X11-unix";

/// A running graphical login session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopSession {
    pub uid: u32,
    /// `"wayland"` or `"x11"`.
    pub session_type: String,
    /// X11 `DISPLAY` (`:0`); `None` for Wayland.
    pub display: Option<String>,
    /// Wayland socket name; `None` for X11.
    pub wayland_display: Option<String>,
}

/// Session ids from `loginctl list-sessions --no-legend` output.
pub fn parse_session_ids(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

/// `key=value` lines of `loginctl show-session` output.
pub fn parse_props(stdout: &str) -> HashMap<String, String> {
    stdout
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('=').unwrap_or((l, ""));
            let k = k.trim();
            (!k.is_empty()).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

/// The session a property map describes, when it is a local, active,
/// graphical one. `wayland_display` names the socket for a Wayland session.
pub fn session_from_props(
    props: &HashMap<String, String>,
    wayland_display: impl FnOnce(u32) -> String,
) -> Option<DesktopSession> {
    let get = |k: &str| props.get(k).map(String::as_str);
    let stype = get("Type")?;
    if stype != "wayland" && stype != "x11" {
        return None;
    }
    if get("Remote").unwrap_or("no") == "yes" {
        return None;
    }
    if get("Active") != Some("yes") && get("State") != Some("active") {
        return None;
    }
    let uid: u32 = get("User")?.parse().ok()?;
    Some(if stype == "wayland" {
        DesktopSession {
            uid,
            session_type: "wayland".into(),
            display: None,
            wayland_display: Some(wayland_display(uid)),
        }
    } else {
        DesktopSession {
            uid,
            session_type: "x11".into(),
            display: Some(
                get("Display")
                    .filter(|d| !d.is_empty())
                    .unwrap_or(":0")
                    .to_string(),
            ),
            wayland_display: None,
        }
    })
}

/// The Wayland socket names bound in `runtime_dir`, sorted.
pub fn wayland_sockets(runtime_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
        .collect();
    names.sort();
    names
}

/// The first Wayland socket in the user's runtime dir, else `wayland-0`.
pub fn wayland_display_for(uid: u32) -> String {
    wayland_sockets(&Path::new(RUN_USER_ROOT).join(uid.to_string()))
        .into_iter()
        .next()
        .unwrap_or_else(|| "wayland-0".to_string())
}

/// True when the session's display-server socket is bound. A session can read
/// active a moment before its socket exists, and a browser launched then fails
/// to connect.
pub fn session_socket_ready(session: &DesktopSession, run_user: &Path, x11_dir: &Path) -> bool {
    if session.session_type == "wayland" {
        let sock = session.wayland_display.as_deref().unwrap_or("wayland-0");
        return run_user.join(session.uid.to_string()).join(sock).exists();
    }
    let disp = session.display.as_deref().unwrap_or(":0");
    let n = disp
        .trim_start_matches(':')
        .split('.')
        .next()
        .unwrap_or("0");
    x11_dir.join(format!("X{n}")).exists()
}

/// The active local graphical session from `loginctl`, or `None` (no logind,
/// no session: the kiosk owns the display through cage).
pub async fn detect_desktop_session() -> Option<DesktopSession> {
    let list = run(
        "loginctl",
        &["list-sessions", "--no-legend"],
        &[],
        PROBE_TIMEOUT,
    )
    .await;
    if !list.success() {
        return None;
    }
    for id in parse_session_ids(list.stdout()) {
        let show = run(
            "loginctl",
            &[
                "show-session",
                &id,
                "-p",
                "Type",
                "-p",
                "State",
                "-p",
                "Active",
                "-p",
                "Remote",
                "-p",
                "User",
                "-p",
                "Display",
            ],
            &[],
            PROBE_TIMEOUT,
        )
        .await;
        if !show.success() {
            continue;
        }
        if let Some(session) = session_from_props(&parse_props(show.stdout()), wayland_display_for)
        {
            return Some(session);
        }
    }
    None
}

/// A detected session whose display-server socket is up, else `None`.
pub async fn detect_ready_session() -> Option<DesktopSession> {
    let session = detect_desktop_session().await?;
    session_socket_ready(
        &session,
        Path::new(RUN_USER_ROOT),
        Path::new(X11_SOCKET_DIR),
    )
    .then_some(session)
}

/// True when a display-manager unit is active (a desktop is up or coming up).
pub async fn display_manager_active() -> bool {
    for dm in DISPLAY_MANAGERS {
        let unit = format!("{dm}.service");
        let out = run("systemctl", &["is-active", &unit], &[], PROBE_TIMEOUT).await;
        if out.stdout().trim() == "active" {
            return true;
        }
    }
    false
}

/// A ready desktop session, waiting up to `wait` (polling every `poll`) when a
/// display manager is active but the session or its socket is not up yet (the
/// boot race). `None` on a headless box, or when the wait expires: cage then
/// owns the display.
pub async fn resolve_desktop_session_with<D, DF, M, MF>(
    mut detect: D,
    dm_active: M,
    wait: Duration,
    poll: Duration,
) -> Option<DesktopSession>
where
    D: FnMut() -> DF,
    DF: Future<Output = Option<DesktopSession>>,
    M: FnOnce() -> MF,
    MF: Future<Output = bool>,
{
    if let Some(session) = detect().await {
        return Some(session);
    }
    if !dm_active().await {
        return None;
    }
    tracing::info!(
        timeout_s = wait.as_secs_f64(),
        "kiosk_waiting_for_desktop_session"
    );
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        tokio::time::sleep(poll).await;
        if let Some(session) = detect().await {
            return Some(session);
        }
    }
    tracing::warn!("kiosk_desktop_session_timeout");
    None
}

/// [`resolve_desktop_session_with`] against the live system.
pub async fn resolve_desktop_session() -> Option<DesktopSession> {
    resolve_desktop_session_with(
        detect_ready_session,
        display_manager_active,
        SESSION_WAIT,
        SESSION_POLL,
    )
    .await
}

/// A user's home dir, name and primary gid from the passwd database.
pub fn passwd_entry(uid: u32) -> Option<(PathBuf, String, u32)> {
    #[cfg(target_os = "linux")]
    {
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)).ok()??;
        Some((user.dir, user.name, user.gid.as_raw()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = uid;
        None
    }
}

/// The user's X authority cookie, from the usual locations.
fn xauthority_for(uid: u32, home: Option<&Path>) -> Option<String> {
    let mut candidates: Vec<PathBuf> = home.map(|h| h.join(".Xauthority")).into_iter().collect();
    let run_user = Path::new(RUN_USER_ROOT).join(uid.to_string());
    candidates.push(run_user.join(".mutter-Xwaylandauth"));
    candidates.push(run_user.join("gdm/Xauthority"));
    candidates
        .into_iter()
        .find(|p| p.exists())
        .map(|p| p.to_string_lossy().into_owned())
}

/// The environment overlay that attaches a child to the session's display
/// server, with the session user's `HOME` / `USER` / `LOGNAME` (the window
/// runs as that user, never root, so it needs a writable home).
pub fn session_env(session: &DesktopSession) -> Vec<(String, String)> {
    let mut env = vec![(
        "XDG_RUNTIME_DIR".to_string(),
        format!("{RUN_USER_ROOT}/{}", session.uid),
    )];
    let home = passwd_entry(session.uid).map(|(home, name, _)| {
        env.push(("HOME".into(), home.to_string_lossy().into_owned()));
        env.push(("USER".into(), name.clone()));
        env.push(("LOGNAME".into(), name));
        home
    });
    if session.session_type == "wayland" {
        let sock = session.wayland_display.as_deref().unwrap_or("wayland-0");
        env.push(("WAYLAND_DISPLAY".into(), sock.to_string()));
    } else {
        env.push((
            "DISPLAY".into(),
            session.display.as_deref().unwrap_or(":0").to_string(),
        ));
        if let Some(xauth) = xauthority_for(session.uid, home.as_deref()) {
            env.push(("XAUTHORITY".into(), xauth));
        }
    }
    env
}

#[cfg(test)]
mod tests;
