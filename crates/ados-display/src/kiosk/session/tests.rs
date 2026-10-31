use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;

fn props(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn wayland(uid: u32) -> DesktopSession {
    DesktopSession {
        uid,
        session_type: "wayland".into(),
        display: None,
        wayland_display: Some("wayland-0".into()),
    }
}

#[test]
fn parses_loginctl_output() {
    assert_eq!(
        parse_session_ids("  c1 1000 op seat0\n  4 0 root\n\n"),
        vec!["c1".to_string(), "4".to_string()]
    );
    let p = parse_props("Type=wayland\nActive=yes\nDisplay=\n");
    assert_eq!(p["Type"], "wayland");
    assert_eq!(p["Display"], "");
}

#[test]
fn an_active_wayland_session_is_detected() {
    let p = props(&[
        ("Type", "wayland"),
        ("State", "active"),
        ("Active", "yes"),
        ("Remote", "no"),
        ("User", "1000"),
        ("Display", ""),
    ]);
    let s = session_from_props(&p, |_| "wayland-1".into()).unwrap();
    assert_eq!(s.session_type, "wayland");
    assert_eq!(s.uid, 1000);
    assert_eq!(s.wayland_display.as_deref(), Some("wayland-1"));
}

#[test]
fn an_x11_session_carries_its_display() {
    let p = props(&[
        ("Type", "x11"),
        ("Active", "yes"),
        ("Remote", "no"),
        ("User", "1000"),
        ("Display", ":0"),
    ]);
    let s = session_from_props(&p, |_| unreachable!()).unwrap();
    assert_eq!(s.session_type, "x11");
    assert_eq!(s.display.as_deref(), Some(":0"));
}

#[test]
fn tty_remote_and_inactive_sessions_are_not_desktops() {
    let tty = props(&[
        ("Type", "tty"),
        ("Active", "yes"),
        ("Remote", "no"),
        ("User", "1000"),
    ]);
    assert_eq!(session_from_props(&tty, |_| String::new()), None);
    let inactive = props(&[
        ("Type", "wayland"),
        ("Active", "no"),
        ("State", "online"),
        ("Remote", "no"),
        ("User", "1000"),
    ]);
    assert_eq!(session_from_props(&inactive, |_| String::new()), None);
    let remote = props(&[
        ("Type", "x11"),
        ("Active", "yes"),
        ("Remote", "yes"),
        ("User", "1000"),
    ]);
    assert_eq!(session_from_props(&remote, |_| String::new()), None);
}

#[test]
fn wayland_sockets_skip_lock_files() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["wayland-1", "wayland-0", "wayland-0.lock", "pulse"] {
        std::fs::write(dir.path().join(name), "").unwrap();
    }
    assert_eq!(wayland_sockets(dir.path()), vec!["wayland-0", "wayland-1"]);
    assert!(wayland_sockets(&dir.path().join("absent")).is_empty());
}

#[test]
fn the_wayland_socket_must_exist_to_be_ready() {
    let dir = tempfile::tempdir().unwrap();
    let run_user = dir.path().join("run_user");
    std::fs::create_dir_all(run_user.join("4242")).unwrap();
    let s = wayland(4242);
    assert!(!session_socket_ready(&s, &run_user, dir.path()));
    std::fs::write(run_user.join("4242/wayland-0"), "").unwrap();
    assert!(session_socket_ready(&s, &run_user, dir.path()));
}

#[test]
fn the_x11_socket_must_exist_to_be_ready() {
    let dir = tempfile::tempdir().unwrap();
    let s = DesktopSession {
        uid: 1,
        session_type: "x11".into(),
        display: Some(":0".into()),
        wayland_display: None,
    };
    assert!(!session_socket_ready(&s, dir.path(), dir.path()));
    std::fs::write(dir.path().join("X0"), "").unwrap();
    assert!(session_socket_ready(&s, dir.path(), dir.path()));
}

#[test]
fn session_env_attaches_to_the_display_server() {
    let env: HashMap<String, String> = session_env(&DesktopSession {
        wayland_display: Some("wayland-1".into()),
        ..wayland(1000)
    })
    .into_iter()
    .collect();
    assert_eq!(env["XDG_RUNTIME_DIR"], "/run/user/1000");
    assert_eq!(env["WAYLAND_DISPLAY"], "wayland-1");
    assert!(!env.contains_key("DISPLAY"));

    let x11: HashMap<String, String> = session_env(&DesktopSession {
        uid: 1000,
        session_type: "x11".into(),
        display: Some(":0".into()),
        wayland_display: None,
    })
    .into_iter()
    .collect();
    assert_eq!(x11["DISPLAY"], ":0");
    assert_eq!(x11["XDG_RUNTIME_DIR"], "/run/user/1000");
}

#[cfg(target_os = "linux")]
#[test]
fn session_env_carries_home_for_the_session_user() {
    let uid = nix::unistd::getuid().as_raw();
    let env: HashMap<String, String> = session_env(&wayland(uid)).into_iter().collect();
    assert!(!env["HOME"].is_empty());
    assert!(!env["USER"].is_empty());
}

#[tokio::test]
async fn a_present_session_returns_at_once() {
    let ms = Duration::from_millis(10);
    let got =
        resolve_desktop_session_with(|| async { Some(wayland(1)) }, || async { false }, ms, ms)
            .await;
    assert_eq!(got, Some(wayland(1)));
}

#[tokio::test]
async fn a_headless_box_without_a_display_manager_uses_cage() {
    let ms = Duration::from_millis(10);
    let got = resolve_desktop_session_with(|| async { None }, || async { false }, ms, ms).await;
    assert_eq!(got, None);
}

#[tokio::test]
async fn an_active_display_manager_is_waited_for() {
    let calls = Arc::new(AtomicUsize::new(0));
    let detect = {
        let calls = calls.clone();
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            async move { (n >= 3).then(|| wayland(1)) }
        }
    };
    let got = resolve_desktop_session_with(
        detect,
        || async { true },
        Duration::from_secs(1),
        Duration::from_millis(10),
    )
    .await;
    assert_eq!(got, Some(wayland(1)));
}

#[tokio::test]
async fn a_session_that_never_comes_falls_back_to_cage() {
    let got = resolve_desktop_session_with(
        || async { None },
        || async { true },
        Duration::from_millis(50),
        Duration::from_millis(10),
    )
    .await;
    assert_eq!(got, None);
}
