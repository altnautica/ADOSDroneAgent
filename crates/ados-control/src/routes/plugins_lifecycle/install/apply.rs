//! Install: the upload, allowlisted-URL and built-in install routes.

use super::*;

/// The install response, in the Python route's field order.
#[derive(Serialize)]
pub(super) struct InstallResponse {
    pub(super) ok: bool,
    pub(super) plugin_id: String,
    pub(super) version: String,
    pub(super) signer_id: Option<String>,
    pub(super) risk: String,
    pub(super) permissions_requested: Vec<String>,
    pub(super) granted: Vec<String>,
    pub(super) job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) sha256: Option<String>,
}

impl InstallResponse {
    pub(super) fn new(result: InstallResult, granted: Vec<String>, job_id: Option<String>) -> Self {
        Self {
            ok: true,
            plugin_id: result.plugin_id,
            version: result.version,
            signer_id: result.signer_id,
            risk: result.risk,
            permissions_requested: result.permissions_requested,
            granted,
            job_id,
            sha256: None,
        }
    }
}

/// Grant each requested permission; a refused one is logged and skipped.
pub(super) fn grant_requested(
    sup: &mut PluginSupervisor,
    plugin_id: &str,
    wanted: &[String],
) -> Vec<String> {
    let mut granted = Vec::new();
    for perm in wanted {
        match sup.grant_permission(plugin_id, perm) {
            Ok(()) => granted.push(perm.clone()),
            Err(e) => tracing::warn!(
                plugin_id,
                permission = %perm,
                error = %e,
                "plugin_install_grant_skip"
            ),
        }
    }
    granted
}

/// The shared install tail: preview checks, install, grants, with the job
/// sidecar walked through `verifying → installing → completed` (or `failed`
/// with the refusal) so the progress stream serves every transport alike.
pub(super) async fn install_bytes(
    state: &AppState,
    raw: Vec<u8>,
    source_uri: String,
    job: &JobSidecar,
    wanted: Vec<String>,
) -> Result<(InstallResult, Vec<String>), Refusal> {
    job.stage("verifying");
    let contents: ArchiveContents = state
        .plugins
        .read(move |sup| {
            let contents = parse_archive_bytes(raw).map_err(Refusal::from_install)?;
            check_capabilities_known(&contents.manifest, sup)?;
            Ok(contents)
        })
        .await
        .map_err(|r| job.fail(r))?;
    job.stage("installing");
    let (result, granted) = state
        .plugins
        .write(move |sup| {
            let result = sup
                .install_contents(contents, Path::new(&source_uri))
                .map_err(Refusal::from_install)?;
            let granted = grant_requested(sup, &result.plugin_id, &wanted);
            Ok((result, granted))
        })
        .await
        .and_then(|r| r)
        .map_err(|r| job.fail(r))?;
    job.completed(&result.plugin_id);
    Ok((result, granted))
}

#[derive(Deserialize)]
pub struct InstallQuery {
    #[serde(default)]
    pub(super) job_id: Option<String>,
    #[serde(default)]
    pub(super) requested_permissions: Option<String>,
}

/// `POST /api/plugins/install?job_id=&requested_permissions=a,b`: multipart
/// `file`. Installs the archive and grants each requested permission the
/// supervisor accepts.
pub async fn install_plugin(
    State(state): State<AppState>,
    Query(query): Query<InstallQuery>,
    headers: HeaderMap,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let Some(_slot) = try_archive_slot() else {
        return busy();
    };
    let job = match JobSidecar::new(&state.plugins.job_dir, query.job_id.clone()) {
        Ok(job) => job,
        Err(r) => return r.into_response(),
    };
    let expected = match expected_digest(&headers) {
        Ok(d) => d,
        Err(r) => return job.fail(r).into_response(),
    };
    let (file_name, raw) = match read_upload(multipart, expected.as_deref()).await {
        Ok(upload) => upload,
        Err(fault) => return fault.respond(Some(&job)),
    };
    let wanted: Vec<String> = query
        .requested_permissions
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    let result = install_bytes(&state, raw, file_name, &job, wanted)
        .await
        .map(|(result, granted)| InstallResponse::new(result, granted, query.job_id));
    respond(result.map(|body| json_response(StatusCode::OK, &body)))
}

#[derive(Deserialize)]
pub(super) struct InstallFromUrlRequest {
    pub(super) url: String,
    #[serde(default)]
    pub(super) expected_sha256: Option<String>,
    #[serde(default)]
    pub(super) requested_permissions: Option<Vec<String>>,
    #[serde(default)]
    pub(super) job_id: Option<String>,
    #[serde(default)]
    pub(super) from_catalog: bool,
}

/// The archive sha256 the bundled catalog pins for `url`, when `url` is one of
/// its entries' download URLs.
pub(super) fn catalog_pin_for(url: &str) -> Option<String> {
    bundled_catalog()
        .get("plugins")?
        .as_array()?
        .iter()
        .find(|p| {
            p.get("download_url")
                .and_then(|u| u.as_str())
                .is_some_and(|u| !u.is_empty() && u == url)
        })
        .map(|p| {
            p.get("archive_sha256")
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string()
        })
}

/// `POST /api/plugins/install_from_url`: `{url, expected_sha256?,
/// requested_permissions?, job_id?, from_catalog}`. Downloads an allowlisted
/// archive (streamed under the cap, pinned when a sha256 is given) and installs
/// it. A catalog install must name a URL the bundled catalog lists and pin that
/// entry's sha256.
pub async fn install_from_url(State(state): State<AppState>, body: Bytes) -> Response {
    let req: InstallFromUrlRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    let Some(_slot) = try_archive_slot() else {
        return busy();
    };
    let result = async {
        let job = JobSidecar::new(&state.plugins.job_dir, req.job_id.clone())?;
        let url = validate_url(&req.url).map_err(|r| job.fail(r))?;
        let expected = req.expected_sha256.unwrap_or_default().trim().to_string();
        if req.from_catalog {
            if expected.is_empty() {
                return Err(job.fail(Refusal::usage(
                    "sha256_required",
                    "catalog installs must pin archive_sha256",
                )));
            }
            let pinned =
                catalog_pin_for(&url).is_some_and(|pin| pin.eq_ignore_ascii_case(&expected));
            if !pinned {
                return Err(job.fail(Refusal::new(
                    12,
                    "catalog_mismatch",
                    "a catalog install must name a bundled catalog entry's url and pin its \
                     archive_sha256",
                    StatusCode::BAD_REQUEST,
                )));
            }
        }
        job.stage("downloading");
        let (raw, sha256) = download_archive(state.plugins.download.clone(), url.clone(), expected)
            .await
            .map_err(|r| job.fail(r))?;
        let wanted: Vec<String> = req
            .requested_permissions
            .unwrap_or_default()
            .iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        let bytes = raw.len();
        let (result, granted) = install_bytes(&state, raw, url, &job, wanted).await?;
        tracing::info!(
            plugin_id = %result.plugin_id,
            version = %result.version,
            sha256 = %sha256,
            bytes,
            "plugin_install_from_url_ok"
        );
        let mut body = InstallResponse::new(result, granted, req.job_id);
        body.sha256 = Some(sha256);
        Ok(body)
    }
    .await;
    respond(result.map(|body| json_response(StatusCode::OK, &body)))
}

#[derive(Deserialize)]
pub(super) struct InstallBuiltinRequest {
    pub(super) plugin_id: String,
}

/// `POST /api/plugins/install_builtin`: `{plugin_id}`. Installs one of the
/// built-in plugins the agent ships. They carry no archive signature (their
/// code is the agent's own); every other install gate applies.
pub async fn install_builtin(State(state): State<AppState>, body: Bytes) -> Response {
    let req: InstallBuiltinRequest = match parse_body(&body) {
        Ok(r) => r,
        Err(bad) => return bad.into_response(),
    };
    let Some(&(_, yaml)) = BUILTIN_MANIFESTS
        .iter()
        .find(|(id, _)| *id == req.plugin_id)
    else {
        return respond(Err(Refusal::not_found(format!(
            "{} is not a built-in plugin",
            req.plugin_id
        ))));
    };
    let result = state
        .plugins
        .write(move |sup| sup.install_builtin(yaml).map_err(Refusal::from_install))
        .await
        .and_then(|r| r);
    respond(result.map(|installed| {
        json_response(
            StatusCode::OK,
            &InstallResponse::new(installed, Vec::new(), None),
        )
    }))
}
