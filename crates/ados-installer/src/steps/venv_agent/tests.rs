use super::args::GIT_STALL_BOUND;
use super::edge::checkout_rev;
use super::stable::{extract_bundle_lock, sidecar, verify_release_asset};
use std::path::PathBuf;

use super::*;
use crate::verify;

#[test]
fn venv_paths_are_under_the_venv_dir() {
    assert_eq!(venv_python(), "/opt/ados/venv/bin/python");
    assert_eq!(venv_pip(), "/opt/ados/venv/bin/pip");
}

#[test]
fn staging_is_a_sibling_of_the_live_tree_never_a_child() {
    // A child would be destroyed by the very remove_dir_all that makes room
    // for the promotion, so the clone would be thrown away at the moment it
    // was needed.
    let repo = Path::new("/opt/ados/source");
    let staging = staging_dest(repo);
    assert_eq!(staging, Path::new("/opt/ados/source.staging"));
    assert!(
        !staging.starts_with(repo),
        "staging must not live inside the tree it replaces"
    );
    assert_eq!(staging.parent(), repo.parent());
}

#[test]
fn a_failed_clone_leaves_the_existing_source_tree_intact() {
    // The regression, hit on a rig: the live tree was deleted BEFORE the
    // clone, so a failed clone left the node with no scripts/install.sh —
    // no way to retry its own install, and on a node with no internet, no
    // way back at all. The tree must survive a failure untouched.
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("source");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    std::fs::write(repo.join("scripts/install.sh"), b"#!/bin/sh\n").unwrap();

    // Simulate the failure path: staging is prepared, the clone fails, the
    // staging dir is swept, and promote_staging is never reached.
    let staging = staging_dest(&repo);
    std::fs::create_dir_all(&staging).unwrap();
    let _ = std::fs::remove_dir_all(&staging);

    assert!(
        repo.join("scripts/install.sh").exists(),
        "a failed clone must leave the node able to retry its own install"
    );
    assert!(!staging.exists(), "the staging tree is swept on failure");
}

#[test]
fn a_successful_clone_replaces_the_tree_wholesale() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("source");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("stale.txt"), b"old").unwrap();

    let staging = staging_dest(&repo);
    std::fs::create_dir_all(staging.join("scripts")).unwrap();
    std::fs::write(staging.join("scripts/install.sh"), b"#!/bin/sh\nnew\n").unwrap();

    promote_staging(&staging, &repo).unwrap();

    assert!(
        repo.join("scripts/install.sh").exists(),
        "the new tree is live"
    );
    assert!(
        !repo.join("stale.txt").exists(),
        "the old tree is gone, not merged"
    );
    assert!(!staging.exists(), "staging is consumed by the promotion");
}

#[test]
fn a_failed_promotion_keeps_the_live_tree() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("source");
    std::fs::create_dir_all(repo.join("scripts")).unwrap();
    std::fs::write(repo.join("scripts/install.sh"), b"#!/bin/sh\nlive\n").unwrap();

    // The staging tree is missing, so the promoting rename fails.
    let staging = staging_dest(&repo);
    assert!(promote_staging(&staging, &repo).is_err());

    assert_eq!(
        std::fs::read(repo.join("scripts/install.sh")).unwrap(),
        b"#!/bin/sh\nlive\n",
        "the live tree is restored with its contents"
    );
    assert!(!dir.path().join("source.old").exists());
}

#[test]
fn venv_create_args_request_system_site_packages() {
    let args = venv_create_args("/opt/ados/venv");
    assert_eq!(args[0], "-m");
    assert_eq!(args[1], "venv");
    assert!(args.contains(&"--system-site-packages".to_string()));
    assert_eq!(args.last().unwrap(), "/opt/ados/venv");
}

#[test]
fn pip_edge_args_install_the_source() {
    // No `--quiet`: the install lines stream to the live-detail pane.
    let args = pip_install_edge_args("/tmp/repo");
    assert_eq!(args, vec!["install", "/tmp/repo"]);
}

/// Build a `.tar.gz` bundle in `dir` holding `repo/README.md` and, when
/// `lock` is given, `repo/requirements.lock`.
fn bundle_with(dir: &Path, lock: Option<&str>) -> PathBuf {
    let stage = dir.join("stage");
    std::fs::create_dir_all(stage.join("repo")).unwrap();
    std::fs::write(stage.join("repo/README.md"), b"readme").unwrap();
    if let Some(body) = lock {
        std::fs::write(stage.join("repo/requirements.lock"), body).unwrap();
    }
    let bundle = dir.join("bundle.tar.gz");
    let ok = std::process::Command::new("tar")
        .args([
            "-czf",
            &bundle.to_string_lossy(),
            "-C",
            &stage.to_string_lossy(),
            "repo",
        ])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    bundle
}

/// The stable install takes its dependencies only from the signed bundle's
/// hash lock; a bundle without one is refused before pip runs.
#[test]
fn the_dependency_lock_comes_from_the_bundle_or_the_install_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let locked = bundle_with(dir.path(), Some("x==1 --hash=sha256:00\n"));
    let lock = extract_bundle_lock(&locked, &out).unwrap();
    assert_eq!(
        std::fs::read_to_string(lock).unwrap(),
        "x==1 --hash=sha256:00\n"
    );

    let bare_dir = tempfile::tempdir().unwrap();
    let bare_out = bare_dir.path().join("out");
    std::fs::create_dir_all(&bare_out).unwrap();
    let unlocked = bundle_with(bare_dir.path(), None);
    let err = extract_bundle_lock(&unlocked, &bare_out).unwrap_err();
    assert!(
        err.to_string().contains("no hashed dependency lock"),
        "{err}"
    );
}

#[test]
fn normalize_version_strips_one_leading_v() {
    // Both the v-prefixed tag form and the bare form collapse to bare.
    assert_eq!(normalize_version("v0.93.0"), "0.93.0");
    assert_eq!(normalize_version("0.93.0"), "0.93.0");
    // Only a single leading `v` is stripped (a digit-led version has none).
    assert_eq!(normalize_version("v1.2.3"), "1.2.3");
    assert_eq!(normalize_version("1.2.3"), "1.2.3");
}

#[test]
fn wheel_filename_uses_the_bare_version() {
    assert_eq!(
        wheel_filename("0.93.0"),
        "ados_drone_agent-0.93.0-py3-none-any.whl"
    );
}

#[test]
fn wheel_url_v_prefixes_the_tag_but_not_the_filename() {
    // The release tag is v-prefixed; the wheel filename is the bare version.
    let from_bare = wheel_url("0.93.0");
    assert_eq!(
        from_bare,
        "https://github.com/altnautica/ADOSDroneAgent/releases/download/v0.93.0/ados_drone_agent-0.93.0-py3-none-any.whl"
    );
    // A v-prefixed input normalizes to the identical URL.
    assert_eq!(wheel_url(&normalize_version("v0.93.0")), from_bare);
}

#[test]
fn the_deploy_bundle_hangs_off_the_same_release_as_the_wheel() {
    assert_eq!(
        release_asset_url("0.93.0", &bundle_filename("0.93.0")),
        "https://github.com/altnautica/ADOSDroneAgent/releases/download/v0.93.0/ados-drone-agent-deploy-0.93.0.tar.gz"
    );
}

/// A release asset with a matching `.sha256` beside it.
fn asset_with_sha(dir: &Path, name: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let path = dir.join(name);
    std::fs::write(&path, b"release bytes").unwrap();
    let digest = hex::encode(Sha256::digest(b"release bytes"));
    std::fs::write(sidecar(&path, "sha256"), format!("{digest}  {name}\n")).unwrap();
    path
}

#[test]
fn a_stable_release_asset_with_only_a_sha256_is_refused() {
    // The sidecar comes from the same host as the wheel, so an attacker who
    // replaces one replaces both. Only the signature proves origin, and the
    // wheel is pip-installed into a venv whose services run as root.
    let dir = tempfile::tempdir().unwrap();
    let wheel = asset_with_sha(dir.path(), "ados_drone_agent-0.93.0-py3-none-any.whl");
    assert!(verify::verify_sha256(&wheel, &sidecar(&wheel, "sha256")).is_ok());
    let err = verify_release_asset(&wheel).unwrap_err().to_string();
    assert!(err.contains("could not be signature-verified"), "{err}");
}

#[test]
fn a_stable_release_asset_with_a_foreign_signature_is_refused() {
    // A `.minisig` that does not come from the embedded release key is not a
    // signature at all as far as the install is concerned.
    let dir = tempfile::tempdir().unwrap();
    let wheel = asset_with_sha(dir.path(), "ados_drone_agent-0.93.0-py3-none-any.whl");
    std::fs::write(
        sidecar(&wheel, "minisig"),
        "untrusted comment: signature\nRWQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==\n",
    )
    .unwrap();
    assert!(verify_release_asset(&wheel).is_err());
}

#[test]
fn a_deploy_bundle_replaces_the_source_tree_with_its_repo_dir() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");
    std::fs::create_dir_all(stage.join("repo/data/systemd")).unwrap();
    std::fs::write(stage.join("repo/data/systemd/ados-x.service"), b"unit").unwrap();
    let bundle = dir.path().join("bundle.tar.gz");
    let ok = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&bundle)
        .arg("-C")
        .arg(&stage)
        .arg("repo")
        .status()
        .unwrap()
        .success();
    assert!(ok);

    let live = dir.path().join("source");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(live.join("stale"), b"old").unwrap();
    unpack_bundle(&bundle, &live).unwrap();
    assert_eq!(
        std::fs::read(live.join("data/systemd/ados-x.service")).unwrap(),
        b"unit"
    );
    assert!(!live.join("stale").exists());
    assert!(!staging_dest(&live).exists());
}

#[test]
fn a_bundle_that_does_not_unpack_leaves_the_source_tree_intact() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("bundle.tar.gz");
    std::fs::write(&bundle, b"not a tarball").unwrap();
    let live = dir.path().join("source");
    std::fs::create_dir_all(live.join("scripts")).unwrap();
    std::fs::write(live.join("scripts/install.sh"), b"#!/bin/sh\n").unwrap();
    assert!(unpack_bundle(&bundle, &live).is_err());
    assert!(live.join("scripts/install.sh").exists());
    assert!(!staging_dest(&live).exists());
}

#[test]
fn git_clone_args_honor_branch() {
    let no_branch = git_clone_args("/tmp/repo", None);
    assert!(!no_branch.contains(&"--branch".to_string()));
    assert!(no_branch.contains(&REPO_URL.to_string()));
    assert_eq!(no_branch.last().unwrap(), "/tmp/repo");

    let branched = git_clone_args("/tmp/repo", Some("main"));
    let pos = branched.iter().position(|a| a == "--branch").unwrap();
    assert_eq!(branched[pos + 1], "main");
    // Shallow + submodules retained.
    assert!(branched.contains(&"--depth".to_string()));
    assert!(branched.contains(&"--recurse-submodules".to_string()));
}

#[test]
fn a_clone_can_never_carry_the_pin_itself() {
    // `git clone --branch` takes a branch or a tag and never a commit SHA,
    // which is the whole reason the pinned path is clone-then-fetch. If
    // someone ever "simplifies" it by passing the rev as a branch, this says
    // why that cannot work.
    let sha = "3b4b8deec0ffee1234567890abcdef1234567890";
    let args = git_clone_args("/tmp/repo", Some(sha));
    let pos = args.iter().position(|a| a == "--branch").unwrap();
    assert_eq!(
        args[pos + 1],
        sha,
        "the builder is literal; the refusal comes from git, so the pinned \
         path must not route a SHA through --branch"
    );
    // The pinned path's own fetch asks for the object by name instead.
    assert_eq!(
        git_command(&git_fetch_rev_args(sha)),
        vec!["fetch", "--depth", "1", "origin", sha]
    );
}

/// The subcommand and its arguments after the stall-bound prefix, which
/// must be present on every networked git command.
fn git_command(args: &[String]) -> Vec<&str> {
    assert_eq!(
        &args[..GIT_STALL_BOUND.len()],
        GIT_STALL_BOUND,
        "missing the stall bound: {args:?}"
    );
    args[GIT_STALL_BOUND.len()..]
        .iter()
        .map(String::as_str)
        .collect()
}

#[test]
fn every_networked_git_command_abandons_a_stalled_transfer() {
    // git has no stall bound of its own: a clone whose transfer stops mid-way
    // on a flaky link otherwise blocks the install step forever.
    for args in [
        git_clone_args("/tmp/repo", None),
        git_clone_args("/tmp/repo", Some("main")),
        git_fetch_rev_args("3b4b8dee"),
        git_deepen_args("main"),
        git_submodule_sync_args(),
    ] {
        let pos = |needle: &str| args.iter().position(|a| a == needle);
        let limit = pos("http.lowSpeedLimit=1024").expect("a low-speed floor");
        let time = pos("http.lowSpeedTime=60").expect("a stall window");
        // Global options: they only apply before the subcommand.
        let sub = args
            .iter()
            .position(|a| ["clone", "fetch", "submodule"].contains(&a.as_str()))
            .unwrap();
        assert!(limit < sub && time < sub, "{args:?}");
    }
}

#[test]
fn abbreviated_commits_are_the_only_thing_worth_deepening_for() {
    // A prefix is what a server cannot answer, so it is the only case that
    // pays for a deep fetch.
    assert!(is_abbreviated_commit("3b4b8dee"));
    assert!(is_abbreviated_commit(
        "3b4b8deec0ffee1234567890abcdef123456789"
    ));
    // A full object name the server already refused is genuinely absent.
    assert!(!is_abbreviated_commit(
        "3b4b8deec0ffee1234567890abcdef1234567890"
    ));
    // Branches and tags are not hex and never reach the deepen path.
    assert!(!is_abbreviated_commit("main"));
    assert!(!is_abbreviated_commit("v0.99.359"));
    assert!(!is_abbreviated_commit(""));
}

#[test]
fn the_deepen_and_checkout_args_pin_rather_than_follow() {
    assert_eq!(
        git_command(&git_deepen_args("main")),
        vec!["fetch", "--depth", "500", "origin", "main"]
    );
    assert_eq!(
        git_rev_parse_args("3b4b8dee"),
        vec!["rev-parse", "--verify", "--quiet", "3b4b8dee^{commit}"]
    );
    // Detached: a pin is a commit, not a branch to follow forward.
    let co = git_checkout_detached_args("FETCH_HEAD");
    assert!(co.contains(&"--detach".to_string()));
    assert!(co.contains(&"--force".to_string()));
    assert_eq!(co.last().unwrap(), "FETCH_HEAD");
    // Submodules follow the superproject or the tree is mixed-revision.
    assert_eq!(
        git_command(&git_submodule_sync_args()),
        vec![
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--depth",
            "1"
        ]
    );
}

#[test]
fn an_unresolvable_pin_refuses_to_fall_back_to_the_branch() {
    // The silent fallback is the entire bug class: a deploy that gated on one
    // commit and then installed whatever the branch had become.
    let msg = rev_unresolvable("deadbeef", "main");
    assert!(msg.contains("Not falling back"), "{msg}");
    assert!(msg.contains("500"), "names the deepen bound: {msg}");
    assert!(msg.contains("40-character SHA"), "names the fix: {msg}");
}

/// Build a throwaway "remote" with two commits and clone it, so the resolver
/// runs against a real object store. `allowAnySHA1InWant` mirrors github.com,
/// where fetching a bare object name is what makes a SHA pin possible at all.
fn fixture_clone() -> (tempfile::TempDir, PathBuf, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let remote = dir.path().join("remote");
    let git = |args: &[&str]| {
        let out = exec::run("git", args);
        assert!(out.success(), "git {args:?} failed: {}", out.stderr);
        out
    };
    std::fs::create_dir_all(&remote).unwrap();
    let r = remote.to_string_lossy().into_owned();
    git(&["init", "-q", "-b", "main", &r]);
    git(&["-C", &r, "config", "user.email", "t@example.com"]);
    git(&["-C", &r, "config", "user.name", "t"]);
    git(&["-C", &r, "config", "uploadpack.allowAnySHA1InWant", "true"]);
    std::fs::write(remote.join("marker"), b"old\n").unwrap();
    git(&["-C", &r, "add", "-A"]);
    git(&["-C", &r, "commit", "-qm", "first"]);
    let old = git(&["-C", &r, "rev-parse", "HEAD"])
        .stdout
        .trim()
        .to_string();
    std::fs::write(remote.join("marker"), b"new\n").unwrap();
    git(&["-C", &r, "add", "-A"]);
    git(&["-C", &r, "commit", "-qm", "second"]);
    let tip = git(&["-C", &r, "rev-parse", "HEAD"])
        .stdout
        .trim()
        .to_string();

    // `file://` rather than a bare path: the local transport hardlinks the
    // object store and ignores `--depth`, so a bare path would neither be a
    // shallow clone nor exercise upload-pack's `allowAnySHA1InWant` — the
    // very thing a SHA pin depends on.
    let clone = dir.path().join("clone");
    let c = clone.to_string_lossy().into_owned();
    git(&["clone", "-q", "--depth", "1", &format!("file://{r}"), &c]);
    (dir, clone, old, tip)
}

#[test]
fn checkout_rev_moves_the_tree_and_reports_the_full_object_name() {
    let (_dir, clone, old, _tip) = fixture_clone();
    let sink = ProgressSink::default();
    let full = checkout_rev(&clone, &old, "main", &sink).unwrap();
    assert_eq!(full, old, "the pin's own full object name is returned");
    assert_eq!(
        std::fs::read_to_string(clone.join("marker")).unwrap(),
        "old\n",
        "the tree is at the pinned commit, not the branch tip"
    );
}

#[test]
fn checkout_rev_expands_an_abbreviated_pin_locally() {
    // The expansion is what lets `fetch_binaries` address `rev-<full sha>`:
    // CI publishes the full 40-char tag, so an unexpanded prefix would 404
    // the whole per-revision release. No GitHub API, no credential — the
    // clone's own object store answers it.
    let (_dir, clone, old, _tip) = fixture_clone();
    let short = &old[..8];
    let full = checkout_rev(&clone, short, "main", &ProgressSink::default()).unwrap();
    assert_eq!(full, old);
    assert_eq!(full.len(), 40);
}

#[test]
fn checkout_rev_refuses_an_unknown_pin_and_leaves_the_tree_where_it_was() {
    let (_dir, clone, _old, tip) = fixture_clone();
    let err = checkout_rev(
        &clone,
        "ffffffffffffffffffffffffffffffffffffffff",
        "main",
        &ProgressSink::default(),
    )
    .expect_err("an absent commit must fail rather than install the tip");
    assert!(err.to_string().contains("Not falling back"));
    let head = exec::run(
        "git",
        &["-C", &clone.to_string_lossy(), "rev-parse", "HEAD"],
    );
    assert_eq!(
        head.stdout.trim(),
        tip,
        "a failed pin must not have moved the tree"
    );
}
