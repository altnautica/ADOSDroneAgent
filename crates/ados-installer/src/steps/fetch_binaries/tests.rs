use super::*;
use crate::binaries::PREBUILT;
use crate::checkpoint::Checkpoint;

#[test]
fn each_hard_gate_failing_means_fail_required() {
    // The MAVLink router is the sole C2 path with no Python fallback, so it
    // is a Hard gate alongside the orchestrator/video/cloud/vision set.
    for svc in [
        "ados-supervisor",
        "ados-mavlink-router",
        "ados-video",
        "ados-cloud",
        "ados-vision",
    ] {
        let b = PREBUILT.iter().find(|b| b.service == svc).unwrap();
        assert_eq!(b.gate, Gate::Hard, "{svc} must be a Hard gate");
        assert_eq!(
            gate_outcome(b.gate, false),
            Decision::FailRequired,
            "{svc} failing must abort the install"
        );
        // A Hard gate succeeding still continues.
        assert_eq!(gate_outcome(b.gate, true), Decision::Continue);
    }
}

#[test]
fn best_effort_failing_continues() {
    // Pick a couple of best-effort catalog entries.
    for svc in ["ados-tui", "ados-radio", "ados-groundlink"] {
        let b = PREBUILT.iter().find(|b| b.service == svc).unwrap();
        assert_eq!(b.gate, Gate::BestEffort);
        assert_eq!(
            gate_outcome(b.gate, false),
            Decision::Continue,
            "{svc} (best-effort) failing must NOT abort the install"
        );
    }
}

#[test]
fn only_a_local_build_may_install_without_a_signature() {
    // A release download is judged on its signature on every channel; the
    // `.sha256` beside it comes from the same host and proves nothing about
    // origin. Only `--artifacts` bytes, which cannot carry the CI signature,
    // may fall back to their build host's SHA256.
    let stable = ReleaseRef::pinned(&base_or_default(None), "v0.101.0");
    assert_eq!(
        AssetSource::Release {
            release: None,
            host_arch: "aarch64"
        }
        .signature_policy(),
        SignaturePolicy::Required
    );
    assert_eq!(
        AssetSource::Release {
            release: Some(&stable),
            host_arch: "aarch64"
        }
        .signature_policy(),
        SignaturePolicy::Required
    );
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        AssetSource::Local {
            dir: dir.path(),
            host_arch: "aarch64"
        }
        .signature_policy(),
        SignaturePolicy::LocalBuild
    );
}

/// Write an executable shell script at `path` with `body` as its content.
fn write_script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    set_executable(path).unwrap();
}

#[test]
fn exec_probe_detects_a_rejection_that_fills_the_stderr_pipe() {
    // A loader complaint big enough to fill the ~64 KiB pipe buffer. Reading
    // stderr only after the process is reaped deadlocks here: the child
    // blocks writing, so it never exits, so nothing drains the pipe, so the
    // probe times out and calls a rejected binary runnable. Draining on a
    // separate thread is what makes this terminate at all.
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-chatty-vision");
    write_script(
            &script,
            "printf '%s\\n' \"fake-chatty-vision: /lib/libc.so.6: version GLIBC_2.34 not found\" >&2\n\
             i=0\n\
             while [ $i -lt 2000 ]; do printf '%s\\n' \"padding line to fill the pipe buffer\" >&2; i=$((i+1)); done\n\
             exit 1",
        );
    assert!(
        !binary_execs_on_this_host(&script),
        "a rejection must be detected even when the message fills the pipe"
    );
}

#[test]
fn exec_probe_detects_a_glibc_rejection() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-onnx-vision");
    write_script(
            &script,
            "printf '%s\\n' \"fake-onnx-vision: /lib/aarch64-linux-gnu/libc.so.6: version GLIBC_2.34 not found (required by fake-onnx-vision)\" >&2\nexit 1",
        );
    assert!(
        !binary_execs_on_this_host(&script),
        "a GLIBC_-tagged nonzero exit must read as a linker rejection"
    );
}

#[test]
fn exec_probe_accepts_a_binary_that_actually_starts() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-working-vision");
    // Sleeps well past the probe's deadline, mirroring a real service that
    // stays up — the probe must kill it and report success, not hang.
    // `exec` replaces the shell with `sleep` so the probe's kill() reaps
    // the actual long-lived process instead of orphaning it.
    write_script(&script, "exec sleep 5");
    assert!(
        binary_execs_on_this_host(&script),
        "a binary still running past the deadline was accepted by the loader"
    );
}

#[test]
fn exec_probe_treats_an_unrelated_nonzero_exit_as_runnable() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-crashing-vision");
    // Fails fast, but for a reason that has nothing to do with the dynamic
    // loader (e.g. a real startup error against a missing config) — the
    // probe's job is narrowly the GLIBC_ signature, not "did it exit 0".
    write_script(&script, "echo 'config not found' >&2\nexit 1");
    assert!(
        binary_execs_on_this_host(&script),
        "a non-GLIBC_ failure must not be misread as a linker rejection"
    );
}

#[test]
fn a_rev_pin_names_the_per_revision_release() {
    let sha = "3b4b8deec0ffee1234567890abcdef1234567890";
    let pin = pinned_release("edge", Some(sha), None).unwrap();
    assert_eq!(pin.as_deref(), Some(format!("rev-{sha}").as_str()));
    // One release for every asset, so a pinned install cannot place a
    // binary from another commit.
    let release = ReleaseRef::pinned(&base_or_default(None), pin.as_deref().unwrap());
    assert_eq!(
            release.asset_url("ados-supervisor-aarch64"),
            format!(
                "https://github.com/altnautica/ADOSDroneAgent/releases/download/rev-{sha}/ados-supervisor-aarch64"
            )
        );
}

#[test]
fn stable_resolves_every_binary_from_its_own_version_release() {
    // The edge pointer is whatever main last built; a stable install must
    // place the binaries published with the release it installs, and a v-
    // prefixed or bare --version addresses the same release.
    for version in ["0.99.376", "v0.99.376"] {
        let pin = pinned_release("stable", None, Some(version)).unwrap();
        assert_eq!(pin.as_deref(), Some("v0.99.376"));
    }
    // No version means nothing pinned to install: refused, never the edge
    // build.
    assert!(pinned_release("stable", None, None).is_err());
    assert!(pinned_release("stable", None, Some("  ")).is_err());
    // The development channel reads the edge manifest unless --ref pins it.
    assert_eq!(pinned_release("edge", None, Some("0.99.376")), Ok(None));
}

#[test]
fn the_rev_tag_is_the_one_ci_publishes() {
    assert_eq!(rev_release_tag("abc123"), "rev-abc123");
}

#[test]
fn a_missing_rev_release_names_the_path_filter_as_the_reason() {
    // A bare 404 sends the operator looking for a network fault. The likely
    // cause is that the pinned commit touched only the Python tree, which
    // the Rust workflow's path filter does not build — unguessable unless
    // the message says it.
    let msg = rev_release_missing("3b4b8dee");
    assert!(msg.contains("rev-3b4b8dee"), "names the missing tag: {msg}");
    assert!(msg.contains("crates/**"), "names the filter: {msg}");
    assert!(
        msg.contains("src/ados/**"),
        "names the excluded tree: {msg}"
    );
    assert!(
        msg.contains("without --ref"),
        "names the way forward: {msg}"
    );
}

#[test]
fn the_release_base_override_wins_only_when_it_says_something() {
    assert_eq!(
        base_or_default(Some("file:///tmp/fake-release")),
        "file:///tmp/fake-release"
    );
    assert_eq!(base_or_default(None), RELEASE_BASE);
    // An exported-but-empty variable is a common accident in a CI shell; it
    // must not turn every asset URL into a relative path.
    assert_eq!(base_or_default(Some("")), RELEASE_BASE);
    assert_eq!(base_or_default(Some("   ")), RELEASE_BASE);
}

#[test]
fn the_release_base_override_is_the_variable_the_bootstrap_exports() {
    // Producer/reader symmetry: `scripts/install.sh` resolves the installer
    // binary from this same base and exports it so the install it execs
    // inherits it. A rename on either side would leave the shell half of a
    // file:// test pointed at the fake tree and the Rust half at GitHub —
    // which reads as a passing test and a fetch that never used the override.
    let bootstrap = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/install.sh")
        .canonicalize()
        .expect("the bootstrap must exist beside the crate it fetches");
    let text = std::fs::read_to_string(&bootstrap).expect("the bootstrap must be readable");
    assert!(
        text.contains(&format!("export {RELEASE_BASE_ENV}")),
        "{} must export {RELEASE_BASE_ENV} so the installer inherits the same base",
        bootstrap.display()
    );
    assert!(
        text.contains(&format!("{RELEASE_BASE_ENV}:-{RELEASE_BASE}")),
        "{} must default {RELEASE_BASE_ENV} to the release base this crate compiles in",
        bootstrap.display()
    );
}

#[test]
fn only_a_full_object_name_can_address_a_per_revision_release() {
    let full = "3b4b8deec0ffee1234567890abcdef1234567890";
    assert!(is_full_object_name(full));
    assert!(!is_full_object_name(&full[..8]));
    assert!(!is_full_object_name("main"));
    assert!(!is_full_object_name(""));
    // And an abbreviated pin that slipped through says which step did not
    // run, instead of blaming the workflow's path filter for a skipped
    // checkout.
    let msg = rev_not_expanded("3b4b8dee");
    assert!(msg.contains("--force"), "names the way forward: {msg}");
    assert!(
        !msg.contains("crates/**"),
        "must not misattribute a skipped checkout to the path filter: {msg}"
    );
}

// ----- `--artifacts <dir>`: locally-built service binaries -----

/// A minimal well-formed 64-bit little-endian ELF header declaring
/// `machine`, followed by `payload` so two artifacts can differ in content.
pub(super) fn fake_elf(machine: u16, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(20 + payload.len());
    v.extend_from_slice(b"\x7fELF");
    v.push(2); // EI_CLASS: 64-bit
    v.push(1); // EI_DATA: little-endian
    v.push(1); // EI_VERSION
    v.extend_from_slice(&[0u8; 9]); // EI_OSABI .. EI_PAD
    v.extend_from_slice(&2u16.to_le_bytes()); // e_type: ET_EXEC
    v.extend_from_slice(&machine.to_le_bytes()); // e_machine
    v.extend_from_slice(payload);
    v
}

pub(super) fn sha256_sidecar_body(bytes: &[u8], name: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{}  {name}\n", hex::encode(h.finalize()))
}

/// Drop `bytes` into `dir` as `name`, with the `sha256sum`-format sidecar a
/// build host would have produced beside it.
fn write_local_artifact(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).unwrap();
    std::fs::write(
        dir.join(format!("{name}.sha256")),
        sha256_sidecar_body(bytes, name),
    )
    .unwrap();
}

/// The real `ados-video` catalog entry. Placement is driven through
/// `install_one_at` with a tempdir dest, so the assertions exercise the
/// shipped entry (asset name, gate, tag) rather than a fabricated one.
fn video_entry() -> &'static PrebuiltBinary {
    PREBUILT.iter().find(|b| b.service == "ados-video").unwrap()
}

#[test]
fn a_local_artifact_is_verified_placed_and_leaves_a_rollback_copy() {
    // The whole point of the flag: a bench node validating unlanded agent
    // code goes through the product's own install path, not a `cp` over
    // /opt/ados/bin. So this drives the REAL `install_one` — the same
    // function the fetched-asset path calls — and asserts the same four
    // outcomes it guarantees there: digest checked, binary executable,
    // destination atomically replaced, previous binary retained for
    // rollback.
    let host_arch = crate::env::arch();
    let Some(machine) = expected_elf_machine(host_arch) else {
        // An architecture the ELF gate has no opinion about; nothing to
        // assert about a host this test cannot build a header for.
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let dest = dst.path().join("ados-video");

    // A working binary is already installed; it is what rollback must keep.
    std::fs::write(&dest, fake_elf(machine, b"the binary already installed")).unwrap();

    let new_bytes = fake_elf(machine, b"the locally built binary");
    write_local_artifact(src.path(), "ados-video", &new_bytes);

    let source = AssetSource::Local {
        dir: src.path(),
        host_arch,
    };
    install_one_at(
        video_entry(),
        &dest,
        scratch.path(),
        &ProgressSink::default(),
        &source,
    )
    .expect("a local artifact with a matching .sha256 must install");

    assert_eq!(
        std::fs::read(&dest).unwrap(),
        new_bytes,
        "dest was replaced"
    );
    assert_eq!(
        std::fs::read(prev_sibling(&dest)).unwrap(),
        fake_elf(machine, b"the binary already installed"),
        "the outgoing binary must be retained at <dest>.prev for rollback"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "placed binary must be executable");
    }
    // No staging debris left behind.
    assert!(!dl_sibling(&dest).exists());
    assert!(!sidecar_path(&dl_sibling(&dest), "sha256").exists());
}

#[test]
fn a_local_artifact_that_fails_its_digest_leaves_the_running_binary_alone() {
    // The verify gate is not decorative on this path. A file that does not
    // match the digest its build host recorded (a truncated scp, the wrong
    // binary under the right name) must be refused BEFORE the swap, so the
    // node keeps running the binary it already had — the same
    // verify-then-replace ordering the fetched path relies on.
    let host_arch = crate::env::arch();
    let Some(machine) = expected_elf_machine(host_arch) else {
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let dest = dst.path().join("ados-video");
    let installed = fake_elf(machine, b"the binary already installed");
    std::fs::write(&dest, &installed).unwrap();

    // Sidecar describes one file; the file on disk is a different one.
    write_local_artifact(
        src.path(),
        "ados-video",
        &fake_elf(machine, b"honest bytes"),
    );
    std::fs::write(
        src.path().join("ados-video"),
        fake_elf(machine, b"tampered bytes"),
    )
    .unwrap();

    let err = install_one_at(
        video_entry(),
        &dest,
        scratch.path(),
        &ProgressSink::default(),
        &AssetSource::Local {
            dir: src.path(),
            host_arch,
        },
    )
    .expect_err("a digest mismatch must refuse the install");
    assert!(
        err.to_string().contains("SHA256 verification failed"),
        "must name the check that refused it: {err}"
    );
    assert_eq!(
        std::fs::read(&dest).unwrap(),
        installed,
        "a refused artifact must never reach the live path"
    );
    assert!(
        !dl_sibling(&dest).exists(),
        "the rejected staging copy must be cleared"
    );
}

#[test]
fn a_local_artifact_without_a_sha256_is_refused_with_the_command_to_make_one() {
    // The installer will NOT hash the file and call that verified: a digest
    // computed from the bytes being checked proves nothing. So a missing
    // sidecar is a hard refusal, and the message has to carry the fix or the
    // operator's next move is to go looking for a way to skip the check.
    let host_arch = crate::env::arch();
    let Some(machine) = expected_elf_machine(host_arch) else {
        return;
    };
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let dest = dst.path().join("ados-video");
    std::fs::write(
        src.path().join("ados-video"),
        fake_elf(machine, b"unsigned"),
    )
    .unwrap();

    let err = install_one_at(
        video_entry(),
        &dest,
        scratch.path(),
        &ProgressSink::default(),
        &AssetSource::Local {
            dir: src.path(),
            host_arch,
        },
    )
    .expect_err("no .sha256 must refuse the install");
    let msg = err.to_string();
    assert!(msg.contains("no .sha256"), "names what is missing: {msg}");
    assert!(msg.contains("sha256sum"), "names how to produce it: {msg}");
    assert!(!dest.exists(), "nothing may be placed on a refusal");
}

#[test]
fn a_binary_built_for_the_wrong_machine_is_refused_before_it_replaces_anything() {
    // The bench mistake this catches: `cargo build --release` on the
    // developer's Mac, then scp the Mach-O (or an x86_64 ELF) to the node.
    // Without the gate that lands at /opt/ados/bin and surfaces as a
    // crash-looping unit with an exec-format error.
    assert_eq!(artifact_arch_error(&fake_elf(0xB7, b"x"), "aarch64"), None);
    let wrong = artifact_arch_error(&fake_elf(0x3E, b"x"), "aarch64")
        .expect("an x86_64 ELF cannot run on an aarch64 node");
    assert!(wrong.contains("aarch64"), "{wrong}");

    // A Mach-O (the `\xcf\xfa\xed\xfe` magic a Mac build carries).
    let macho = artifact_arch_error(b"\xcf\xfa\xed\xfe0123456789abcdef", "aarch64")
        .expect("a Mach-O is not an ELF");
    assert!(macho.contains("not an ELF"), "{macho}");

    // A host architecture the gate has no table entry for is not grounds for
    // refusing an artifact; the check simply does not run.
    assert_eq!(artifact_arch_error(&fake_elf(0x3E, b"x"), "s390x"), None);
}

#[test]
fn a_directory_that_supplies_nothing_fails_and_build_debris_is_only_reported() {
    // The single-file typo is fatal, because the silent outcome is the worst
    // one available here: that service falls back to the release, the install
    // reports success, and the bench measures the OLD binary believing it
    // measured the new one.
    let typo = tempfile::tempdir().unwrap();
    std::fs::write(typo.path().join("ados-vidoe"), b"typo").unwrap();
    let err = validate_artifacts_dir(typo.path())
        .expect_err("a directory supplying no service binary must fail the step");
    assert!(
        err.contains("holds no service binary"),
        "names the problem: {err}"
    );
    assert!(
        err.contains("ados-video-aarch64"),
        "names an accepted spelling: {err}"
    );

    // But a real `target/release` is the natural thing to point the flag at,
    // and it carries dep files, rlibs and the installer itself. Those are
    // reported and passed over, never grounds for refusing the directory —
    // rejecting them would make the flag unusable for its only purpose.
    let target = tempfile::tempdir().unwrap();
    for name in [
        "ados-video",
        "ados-video.d",
        "ados-installer",
        "libados_config.rlib",
    ] {
        std::fs::write(target.path().join(name), b"x").unwrap();
    }
    let passed_over = validate_artifacts_dir(target.path())
        .expect("a build directory carrying one service binary is usable");
    assert_eq!(
        passed_over,
        vec![
            "ados-installer".to_string(),
            "ados-video.d".to_string(),
            "libados_config.rlib".to_string()
        ],
        "everything that is not a service binary is named, so a mistyped \
             filename shows up in the report"
    );

    // Both accepted spellings are recognised, and their sidecars are not
    // mistaken for artifacts of their own.
    assert!(ignored_artifact_names(&[
        "ados-video".to_string(),
        "ados-supervisor-aarch64".to_string(),
        "ados-video.sha256".to_string(),
        "ados-video.minisig".to_string(),
    ])
    .is_empty());

    // An empty directory and a path that is not one are both wrong paths.
    let empty = tempfile::tempdir().unwrap();
    assert!(validate_artifacts_dir(empty.path()).is_err());
    assert!(validate_artifacts_dir(&typo.path().join("nope")).is_err());
}

#[test]
fn a_service_the_directory_does_not_carry_still_comes_from_the_release() {
    // One rebuilt crate means one copied file, not fifteen. The entries the
    // directory does not carry resolve to the release exactly as they would
    // with no flag at all.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ados-video"), b"local").unwrap();
    let video = PREBUILT.iter().find(|b| b.service == "ados-video").unwrap();
    let supervisor = PREBUILT
        .iter()
        .find(|b| b.service == "ados-supervisor")
        .unwrap();

    let release = ReleaseRef::pinned(&base_or_default(None), "rev-abc");
    assert!(matches!(
        source_for(video, Some(dir.path()), "aarch64", None),
        AssetSource::Local { .. }
    ));
    assert!(matches!(
        source_for(supervisor, Some(dir.path()), "aarch64", Some(&release)),
        AssetSource::Release {
            release: Some(_),
            ..
        }
    ));
    // With no flag every entry is a release fetch, from the one release.
    match source_for(video, None, "aarch64", Some(&release)) {
        AssetSource::Release {
            release: Some(r), ..
        } => assert_eq!(r.tag(), "rev-abc"),
        other => panic!("expected a release source: {other:?}"),
    }
    // The release asset name is accepted as well as the service name.
    std::fs::write(dir.path().join("ados-supervisor-aarch64"), b"local").unwrap();
    assert!(matches!(
        source_for(supervisor, Some(dir.path()), "aarch64", None),
        AssetSource::Local { .. }
    ));
}

#[test]
fn the_step_refuses_a_bad_artifacts_directory_before_it_installs_anything() {
    // Wiring, not policy: the validation is reached from `Ctx::artifacts` and
    // runs BEFORE the install loop, so a wrong path costs nothing and places
    // nothing. Without this the flag could be parsed, carried onto the
    // context and never read, and every test above would still pass.
    let mut ctx = Ctx::for_test(Checkpoint::new());
    if !ctx.env.supported_arch {
        // The step skips a non-aarch64 host before it looks at anything.
        return;
    }
    ctx.artifacts = Some(PathBuf::from(
        "/nonexistent/ados-artifacts-that-cannot-be-a-directory",
    ));
    match FetchBinaries.run(&mut ctx) {
        StepOutcome::Failed(msg) => {
            assert!(msg.contains("--artifacts"), "names the flag: {msg}");
            assert!(msg.contains("not a directory"), "names the fault: {msg}");
        }
        other => panic!("expected the step to fail before installing: {other:?}"),
    }
}

#[test]
fn a_local_artifact_failure_is_reported_once_rather_than_retried() {
    // The retry loop exists for a dropping link. A missing file or a bad
    // digest is terminal, so retrying it only puts the backoff between the
    // operator and the message naming what to fix.
    assert_eq!(
        AssetSource::Release {
            release: None,
            host_arch: "aarch64"
        }
        .max_attempts(),
        3
    );
    assert_eq!(
        AssetSource::Local {
            dir: Path::new("/tmp"),
            host_arch: "aarch64"
        }
        .max_attempts(),
        1
    );
}

/// A test context posing as an x86_64 Linux host, whatever the build host.
fn x86_64_ctx() -> Ctx {
    let mut ctx = Ctx::for_test(Checkpoint::new());
    ctx.env.arch = "x86_64".to_string();
    ctx.env.supported_arch = true;
    ctx
}

#[test]
fn an_x86_64_host_without_artifacts_fails_before_fetching() {
    let mut ctx = x86_64_ctx();
    ctx.artifacts = None;
    match FetchBinaries.run(&mut ctx) {
        StepOutcome::Failed(msg) => {
            assert_eq!(msg, no_release_assets("x86_64"));
            assert!(msg.contains("--channel edge --artifacts <dir>"), "{msg}");
        }
        other => panic!("expected a failure naming --artifacts: {other:?}"),
    }
}

#[test]
fn an_x86_64_host_never_falls_back_to_the_release_for_a_missing_hard_entry() {
    // The directory carries only the BestEffort relay (last in the catalog),
    // so every Hard entry resolves to the release. On a host without release
    // assets that is a failed entry, never a download: the first Hard one
    // aborts the step before anything is placed.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("mediamtx"), b"local").unwrap();
    let mut ctx = x86_64_ctx();
    ctx.artifacts = Some(dir.path().to_path_buf());
    let first_hard = binaries::for_profile(&ctx.profile)
        .into_iter()
        .find(|b| b.gate == Gate::Hard)
        .expect("the catalog has a Hard entry");
    match FetchBinaries.run(&mut ctx) {
        StepOutcome::Failed(msg) => assert_eq!(
            msg,
            format!(
                "required prebuilt binary {} could not be installed",
                first_hard.service
            )
        ),
        other => panic!("expected the missing Hard entry to fail: {other:?}"),
    }
    assert!(ctx.replaced_binaries.is_empty(), "nothing was placed");
}
