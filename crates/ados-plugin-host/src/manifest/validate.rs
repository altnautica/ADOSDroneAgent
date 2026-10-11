//! Field validators shared by the manifest model.

use super::*;

/// `^[a-z0-9]+(\.[a-z0-9-]+)+$`: at least two dot-separated segments, the first
/// lowercase alnum, the rest lowercase alnum or hyphen.
pub fn is_plugin_id(id: &str) -> bool {
    let mut segments = id.split('.');
    let first_ok = segments.next().is_some_and(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    });
    let mut rest = 0usize;
    let rest_ok = segments.all(|s| {
        rest += 1;
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    first_ok && rest_ok && rest > 0
}

/// Semver 2.0: `MAJOR.MINOR.PATCH`, no leading zeros, with an optional
/// dot-separated pre-release (`-...`) and build (`+...`) suffix.
pub(super) fn is_semver(v: &str) -> bool {
    let (rest, build) = match v.split_once('+') {
        Some((r, b)) => (r, Some(b)),
        None => (v, None),
    };
    let (core, pre) = match rest.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (rest, None),
    };
    let numeric = |s: &str| {
        !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'))
    };
    let alnum =
        |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 || !parts.iter().all(|p| numeric(p)) {
        return false;
    }
    // A pre-release identifier is numeric without leading zeros, or
    // alphanumeric with at least one non-digit.
    let pre_ok = pre.is_none_or(|p| {
        p.split('.')
            .all(|id| alnum(id) && (numeric(id) || id.bytes().any(|b| !b.is_ascii_digit())))
    });
    let build_ok = build.is_none_or(|b| b.split('.').all(alnum));
    pre_ok && build_ok
}

/// Refuse an entrypoint that could escape the plugin's install dir or break
/// out of the unit line it is interpolated into.
pub(super) fn validate_entrypoint(field: &str, value: &str) -> Result<(), ManifestError> {
    let ok = match value.split_once(':') {
        Some((module, class)) => is_module_path(module) && is_identifier(class),
        None => is_relative_posix_path(value),
    };
    if ok {
        Ok(())
    } else {
        Err(ManifestError(format!(
            "{field} {value:?} must be a relative posix path over [A-Za-z0-9._/-] \
             with no empty or '..' segment, or a module:Class reference"
        )))
    }
}

pub(super) fn validate_profile_list(field: &str, profiles: &[String]) -> Result<(), ManifestError> {
    if profiles.is_empty() {
        return Err(ManifestError(format!(
            "{field} must list at least one profile"
        )));
    }
    for p in profiles {
        if canonical_profile(p).is_none() {
            return Err(ManifestError(format!(
                "{field} entry {p:?} must be one of {NODE_PROFILES:?}"
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_payload(payload: &PayloadSpec) -> Result<(), ManifestError> {
    let path = &payload.path;
    if !is_relative_posix_path(path) {
        return Err(ManifestError(format!(
            "agent.payloads path {path:?} must be a relative posix path"
        )));
    }
    if path == crate::attestation::ATTESTATION_FILENAME {
        return Err(ManifestError(format!(
            "agent.payloads path {path:?} is reserved for the install attestation"
        )));
    }
    if !payload.source.starts_with("https://") {
        return Err(ManifestError(format!(
            "agent.payloads[{path}].source must be an https:// URL"
        )));
    }
    if payload.sha256.len() != 64
        || !payload
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ManifestError(format!(
            "agent.payloads[{path}].sha256 must be 64 lowercase hex characters"
        )));
    }
    if !(1..=PAYLOAD_MAX_BYTES).contains(&payload.size_bytes) {
        return Err(ManifestError(format!(
            "agent.payloads[{path}].size_bytes must be 1..={PAYLOAD_MAX_BYTES}"
        )));
    }
    if let Some(profiles) = &payload.profiles {
        validate_profile_list(&format!("agent.payloads[{path}].profiles"), profiles)?;
    }
    if let Some(arch_os) = &payload.arch_os {
        if !is_arch_os(arch_os) {
            return Err(ManifestError(format!(
                "agent.payloads[{path}].arch_os {arch_os:?} must be <arch>-<os>"
            )));
        }
    }
    Ok(())
}

pub(super) fn validate_contribution_id(field: &str, id: &str) -> Result<(), ManifestError> {
    if is_bin_name(id) {
        Ok(())
    } else {
        Err(ManifestError(format!(
            "{field} id {id:?} must match [a-z0-9][a-z0-9_-]{{0,63}}"
        )))
    }
}

pub(super) fn validate_title(field: &str, id: &str, title: &str) -> Result<(), ManifestError> {
    let len = title.chars().count();
    if (1..=80).contains(&len) && !title.chars().any(char::is_control) {
        Ok(())
    } else {
        Err(ManifestError(format!(
            "{field}[{id}].title must be 1-80 printable characters"
        )))
    }
}

/// `[a-z0-9][a-z0-9_-]{0,63}`: a `binaries` name, and a GCS contribution id.
pub(crate) fn is_bin_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.len() <= 64
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// `[a-z0-9_]+-[a-z0-9_]+`: an `<arch>-<os>` key.
pub(super) fn is_arch_os(s: &str) -> bool {
    let part = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    };
    s.split_once('-')
        .is_some_and(|(arch, os)| part(arch) && part(os))
}

/// `[a-z0-9][a-z0-9._-]{0,63}`: the `<name>` of a declared capability.
pub(super) fn is_declared_cap_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.len() <= 64
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// `plugin.<segment>.<name>` over `[a-z0-9._-]`, at most 128 characters.
pub(super) fn is_shared_topic(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("plugin.") else {
        return false;
    };
    let Some((segment, name)) = rest.split_once('.') else {
        return false;
    };
    s.len() <= 128
        && !segment.is_empty()
        && segment
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

pub(super) fn is_relative_posix_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && !segment.starts_with(".."))
}

pub(super) fn is_identifier(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub(super) fn is_module_path(s: &str) -> bool {
    s.split('.').all(is_identifier)
}
