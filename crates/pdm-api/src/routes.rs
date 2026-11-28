use axum::extract::{Extension, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use uuid::Uuid;

use pdm_core::{PdmError, User};
use tokio::process::Command;

use crate::auth::{Authed, require_user};
use crate::design_link;
use crate::error::AppError;
use crate::state::AppState;

// ── Request body types ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateProjectReq {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateProjectReq {
    pub name: String,
    pub description: Option<String>,
    pub image_data: Option<String>,
}

/// Body for `PUT /projects/{id}/main-assembly`. `item_id` of `null` clears the
/// project's main assembly.
#[derive(Deserialize)]
pub struct SetMainAssemblyReq {
    pub item_id: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct CreateItemReq {
    pub part_number: Option<String>,
    pub description: Option<String>,
    pub classification: Option<String>,
    pub folder_id: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct CreateFolderReq {
    pub name: String,
    pub parent_id: Option<Uuid>,
}

#[derive(Deserialize)]
pub struct PartNumberPrefixReq {
    pub category: String,
    pub prefix: String,
}

#[derive(Deserialize)]
pub struct CheckoutReq {
    pub workstation: Option<String>,
}

#[derive(Deserialize)]
pub struct CheckinReq {
    pub sha256: String,
    pub commit_message: Option<String>,
    pub base_version: Option<i32>,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    /// Check in as a new *major* version (V2 - R1 instead of V1 - R2+).
    /// Default `false`: the save is a new revision of the current version.
    #[serde(default)]
    pub new_version: Option<bool>,
}

#[derive(Deserialize)]
pub struct PreviewStyleReq {
    pub preview_style: String,
}

pub async fn get_preview_style(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    Ok(Json(
        serde_json::json!({"preview_style":pdm_db::get_preview_style(&state.pool, authed.org_id).await?}),
    ))
}

pub async fn put_preview_style(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<PreviewStyleReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "admin")?;
    Ok(Json(
        serde_json::json!({"preview_style":pdm_db::set_preview_style(&state.pool, authed.org_id, &body.preview_style).await?}),
    ))
}

/// Query params on the listing routes: opt in to archived rows with
/// `?include_archived=true`.
#[derive(Deserialize)]
pub struct ListQuery {
    pub include_archived: Option<bool>,
}

fn include_archived_flag(v: Option<bool>) -> bool {
    v.unwrap_or(false)
}

/// The last line of `out` that is not empty once trimmed (and trimmed).
fn last_nonempty_line(out: &str) -> Option<&str> {
    out.lines().map(str::trim).rfind(|l| !l.is_empty())
}

#[derive(Deserialize)]
pub struct SetPropertyReq {
    pub value: serde_json::Value,
    pub value_type: Option<String>,
}

#[derive(Deserialize)]
pub struct ReleaseReq {
    pub label: Option<String>,
    pub note: Option<String>,
    /// Explicit assignees for the approval request when release is gated on
    /// approval. Empty/default routes to the org's editors/admins.
    pub assignee_ids: Option<Vec<Uuid>>,
}

/// Pagination + server-side filters shared by `GET /items`, `GET
/// /projects/{id}/items` and `GET /search`.
#[derive(Deserialize)]
pub struct ListParams {
    pub page: Option<u32>,
    pub limit: Option<u32>,
    pub include_archived: Option<bool>,
    pub classification: Option<String>,
    pub state: Option<String>,
    pub kind: Option<String>,
    pub owner: Option<Uuid>,
    pub folder: Option<Uuid>,
    pub q: Option<String>,
    /// `true` = only items with a sheet-metal spec; `false` = only items
    /// without one; omitted = no sheet-metal filtering.
    pub sheet_metal: Option<bool>,
    pub sort: Option<String>,
    pub dir: Option<String>,
}

#[derive(Deserialize)]
pub struct FlatPatternExportQuery {
    pub items: String,
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub project: Option<Uuid>,
    pub include_archived: Option<bool>,
    pub classification: Option<String>,
    pub state: Option<String>,
    pub kind: Option<String>,
    pub owner: Option<Uuid>,
    pub folder: Option<Uuid>,
    pub sheet_metal: Option<bool>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub page: Option<u32>,
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct ApprovalListQuery {
    pub status: Option<String>,
    pub mine: Option<bool>,
    pub page: Option<u32>,
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct LifecycleTransitionReq {
    pub state: String,
}

#[derive(Deserialize)]
pub struct ApprovalDecideReq {
    pub decision: String,
    pub note: Option<String>,
}

#[derive(Deserialize)]
pub struct ApprovalCreateReq {
    pub item_id: Uuid,
    pub assignee_ids: Option<Vec<Uuid>>,
    pub note: Option<String>,
}

/// Item metadata update. `None` field = leave unchanged; `Some(null)` = clear.
#[derive(Deserialize)]
pub struct UpdateItemReq {
    pub description: Option<String>,
    pub folder_id: Option<Option<Uuid>>,
    pub owner_id: Option<Option<Uuid>>,
}

#[derive(Deserialize)]
pub struct CommentReq {
    pub body: String,
}

#[derive(Deserialize)]
pub struct SetPasswordReq {
    pub password: String,
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    pub current: String,
    pub new: String,
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn require_org(authed: &Authed) -> Uuid {
    authed.org_id
}

/// 404 unless the project is the caller's organization's and, when one is given, the folder is in
/// that project. The foreign keys alone accept any organization's ids.
async fn require_project_and_folder(
    state: &AppState,
    org: Uuid,
    project_id: Uuid,
    folder_id: Option<Uuid>,
) -> Result<(), AppError> {
    if pdm_db::get_project(&state.pool, project_id, org).await?.is_none() {
        return Err(AppError(PdmError::NotFound(format!("project {project_id}"))));
    }
    if let Some(folder_id) = folder_id
        && !pdm_db::folder_in_project(&state.pool, folder_id, org, project_id).await?
    {
        return Err(AppError(PdmError::NotFound(format!(
            "folder {folder_id} in project {project_id}"
        ))));
    }
    Ok(())
}

/// 404 unless the item is the caller's organization's.
async fn require_item_in_org(state: &AppState, org: Uuid, item_id: Uuid) -> Result<(), AppError> {
    if pdm_db::get_item(&state.pool, item_id, org).await?.is_none() {
        return Err(AppError(PdmError::NotFound(format!("item {item_id}"))));
    }
    Ok(())
}

/// Auto-generate the next alpha revision label: A, B, … Z, AA, AB, …
fn auto_revision_label(existing: usize) -> String {
    if existing < 26 {
        return char::from(b'A' + existing as u8).to_string();
    }
    let mut n = existing + 1;
    let mut s = String::new();
    while n > 0 {
        n -= 1;
        s.insert(0, char::from(b'A' + (n % 26) as u8));
        n /= 26;
    }
    s
}

// ── Handlers ────────────────────────────────────────────────────────────

/// GET /healthz
pub async fn healthz() -> &'static str {
    "ok"
}

/// GET /readyz — checks DB connectivity.
pub async fn readyz(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    let db_ok = sqlx::query("SELECT 1").execute(&state.pool).await.is_ok();
    Ok(serde_json::json!({ "ready": db_ok, "db": db_ok }).into())
}

/// What build is running: name, package version, the test-build tag
/// (`PDM_BUILD_TAG`, e.g. "V.01-01" — null when unset), git commit ("unknown"
/// outside a checkout) and the build date. Served by `GET /version` and logged
/// at startup.
pub fn version_info() -> serde_json::Value {
    serde_json::json!({
        "name": "solidcraft-pdm",
        "version": env!("CARGO_PKG_VERSION"),
        "build": std::env::var("PDM_BUILD_TAG").ok().filter(|s| !s.is_empty()),
        "commit": option_env!("PDM_API_COMMIT"),
        "built_at": option_env!("PDM_API_BUILD_DATE")
    })
}

/// GET /version — what build of the PDM server is running (public, no auth).
/// `latest` (null until the boot-time release check has run) and
/// `update_available` are additive: old clients that deserialize the
/// original five fields keep working.
pub async fn version(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut info = version_info();
    let update = state.update.read().await;
    info["latest"] = serde_json::json!(update.latest);
    info["update_available"] = serde_json::json!(update.update_available);
    Json(info)
}

/// GET /system-info — server + bundled SolidCraft CLI + infrastructure details for
/// the web UI "System" panel. Authenticated; org-scoped counts are for the caller's org.
pub async fn system_info(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);

    // Bundled SolidCraft CAD CLI — the engine the PDM worker drives for analysis,
    // previews and CAD restatement. Report its path and `--version`.
    let cli_path = std::env::var("PDM_SOLVECRAFT_CLI")
        .unwrap_or_else(|_| "/usr/local/bin/solvecraft-cli".into());
    let mut cli = serde_json::json!({ "path": cli_path, "present": false, "version": null });
    if std::path::Path::new(&cli_path).exists() {
        cli["present"] = serde_json::json!(true);
        match Command::new(&cli_path).arg("--version").output().await {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                // `solvecraft-cli --version` prints a deprecation note then "solidcraft-cli X.Y.Z";
                // take the last non-empty stdout line, falling back to stderr.
                let has_digit = |s: &str| s.bytes().any(|b| b.is_ascii_digit());
                let mut version: Option<String> = last_nonempty_line(&stdout).map(str::to_string);
                if version.as_deref().is_none_or(|v| !has_digit(v)) {
                    version = last_nonempty_line(&stderr).map(str::to_string);
                }
                cli["version"] = serde_json::json!(version);
            }
            Err(e) => cli["error"] = serde_json::json!(format!("failed to run CLI --version: {e}")),
        }
    }

    // PostgreSQL version (e.g. "PostgreSQL 16.3 (Debian …)" → "PostgreSQL 16.3").
    let pg_raw = sqlx::query_scalar::<_, String>("SELECT version()")
        .fetch_one(&state.pool)
        .await
        .ok();
    let pg_version = pg_raw
        .as_deref()
        .map(|v| v.split_whitespace().take(2).collect::<Vec<_>>().join(" "));

    // Object storage backend (MinIO/S3 or local dir) from env.
    let storage_kind = if std::env::var("PDM_STORAGE_LOCAL").is_ok() {
        "local"
    } else {
        "minio/s3"
    };
    let storage_endpoint = std::env::var("PDM_S3_ENDPOINT").ok();
    let storage_bucket = std::env::var("PDM_S3_BUCKET").ok();

    // Counts: global for orgs/users, org-scoped for the caller's data.
    let orgs = sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM organizations")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    let users = sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM users")
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    let projects =
        sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM projects WHERE org_id = $1")
            .bind(org)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0);
    let items = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM items WHERE org_id = $1 AND archived_at IS NULL",
    )
    .bind(org)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);
    let files =
        sqlx::query_scalar::<_, i64>("SELECT count(*)::bigint FROM files WHERE org_id = $1")
            .bind(org)
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0);
    let revisions = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM file_revisions WHERE org_id = $1",
    )
    .bind(org)
    .fetch_one(&state.pool)
    .await
    .unwrap_or(0);

    // The release-check result, so the System panel's Update row can link out.
    let update = state.update.read().await;
    let mut pdm = version_info();
    pdm["latest"] = serde_json::json!(update.latest);
    pdm["update_available"] = serde_json::json!(update.update_available);
    pdm["releases_url"] = serde_json::json!(pdm_update::releases_page_url());

    let gpu_policy = pdm_db::get_gpu_policy(&state.pool, org)
        .await
        .unwrap_or_else(|_| "auto".into());
    let mut gpu = serde_json::json!({ "policy": gpu_policy, "survey": serde_json::Value::Null });
    if std::path::Path::new(&cli_path).exists() {
        match Command::new(&cli_path)
            .arg("gpu")
            .arg("--policy")
            .arg(&gpu_policy)
            .output()
            .await
        {
            Ok(out) if out.status.success() => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    gpu["survey"] = v;
                } else {
                    gpu["error"] = serde_json::json!(String::from_utf8_lossy(&out.stderr));
                }
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                gpu["error"] = serde_json::json!(
                    last_nonempty_line(&err)
                        .unwrap_or("CLI gpu failed")
                        .to_string()
                );
            }
            Err(e) => gpu["error"] = serde_json::json!(format!("failed to run CLI gpu: {e}")),
        }
    }

    Ok(serde_json::json!({
        "pdm": pdm,
        "solidcraft_cli": cli,
        "gpu": gpu,
        "postgres": { "version": pg_version, "ok": pg_raw.is_some() },
        "storage": { "kind": storage_kind, "endpoint": storage_endpoint, "bucket": storage_bucket },
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "counts": {
            "organizations": orgs, "users": users,
            "projects": projects, "items": items, "files": files, "revisions": revisions
        }
    })
    .into())
}

/// GET /me
pub async fn me(Extension(authed): Extension<Authed>) -> Result<Json<serde_json::Value>, AppError> {
    let user = authed.user.clone();
    Ok(serde_json::json!({
        "org_id": authed.org_id,
        "user": user,
        "scopes": authed.scopes
    })
    .into())
}

// ── Client settings (per-user app preferences, e.g. SolidCraft's Prefs) ─

#[derive(Deserialize)]
pub struct PutClientSettingsReq {
    pub payload: serde_json::Value,
}

/// GET /me/settings — this account's client settings (`payload` null: never stored).
pub async fn client_settings_get(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&authed)?.clone();
    let payload = pdm_db::get_client_settings(&state.pool, user.id).await?;
    Ok(serde_json::json!({ "payload": payload }).into())
}

/// PUT /me/settings — store this account's client settings (last write wins).
pub async fn client_settings_put(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<PutClientSettingsReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&authed)?.clone();
    pdm_db::put_client_settings(&state.pool, user.id, &body.payload).await?;
    Ok(serde_json::json!({ "ok": true }).into())
}

/// POST /me/password — change the caller's own password; verifies the current
/// one first. Unlike the admin reset (`/users/{id}/password`), this requires
/// the user to know their existing password.
pub async fn change_own_password(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<ChangePasswordReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&authed)?.clone();
    let org = require_org(&authed);
    let stored = match &user.password_hash {
        Some(h) if !h.is_empty() => h,
        _ => return Err(AppError(PdmError::BadCredentials)),
    };
    if !pdm_db::verify_password(&body.current, stored)? {
        return Err(AppError(PdmError::BadCredentials));
    }
    if body.new.len() < 4 {
        return Err(AppError(PdmError::Validation("password too short".into())));
    }
    let hash = pdm_db::hash_password(&body.new)?;
    pdm_db::set_user_password(&state.pool, user.id, &hash).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "user.password_changed",
        "user",
        Some(user.id),
        None,
    )
    .await?;
    Ok(serde_json::json!({ "ok": true }).into())
}

// ── Settings: part-number prefixes ─────────────────────────────────────

/// Org-admin only. Returns a forbidden error when the caller isn't an admin.
fn require_admin(authed: &Authed) -> Result<&User, AppError> {
    let user = require_user(authed)?;
    if user.role != "admin" {
        return Err(AppError(PdmError::Forbidden("admin only".into())));
    }
    Ok(user)
}

/// GET /settings/part-number-prefixes — list the org's configured part-number prefixes.
pub async fn list_part_number_prefixes(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let prefixes = pdm_db::list_part_number_prefixes(&state.pool, org).await?;
    Ok(serde_json::json!({ "prefixes": prefixes }).into())
}

/// POST /settings/part-number-prefixes — create or update a prefix (admin).
pub async fn upsert_part_number_prefix(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<PartNumberPrefixReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    let category = body.category.trim();
    let prefix = body.prefix.trim();
    if category.is_empty() || prefix.is_empty() {
        return Err(AppError(PdmError::Validation(
            "category and prefix are required".into(),
        )));
    }
    let p = pdm_db::upsert_part_number_prefix(&state.pool, org, category, prefix).await?;
    Ok(serde_json::json!({ "prefix": p }).into())
}

#[derive(Deserialize)]
pub struct GpuPolicyReq {
    pub gpu_policy: String,
}

/// GET /settings/gpu-policy — how preview snapshots are rendered.
pub async fn get_gpu_policy(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let gpu_policy = pdm_db::get_gpu_policy(&state.pool, org).await?;
    Ok(serde_json::json!({ "gpu_policy": gpu_policy }).into())
}

#[derive(Deserialize)]
pub struct MailSettingsReq {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub from: Option<String>,
    pub tls: Option<String>,
    pub login_url: Option<String>,
}

/// GET /settings/mail — org SMTP settings (password never returned).
pub async fn get_mail_settings(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    let raw = pdm_db::get_mail_settings(&state.pool, org).await?;
    Ok(crate::mail::MailSettings::from_json(&raw).public().into())
}

/// PUT /settings/mail — save org SMTP settings (admin). Empty password keeps the current one.
pub async fn put_mail_settings(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<MailSettingsReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    let current =
        crate::mail::MailSettings::from_json(&pdm_db::get_mail_settings(&state.pool, org).await?);
    let tls = body.tls.unwrap_or(current.tls);
    if !matches!(tls.as_str(), "starttls" | "tls" | "none") {
        return Err(AppError(PdmError::BadRequest(
            "tls must be starttls, tls or none".into(),
        )));
    }
    let password = match body.password.as_deref().map(str::trim) {
        Some("") | None => current.password,
        Some(p) => p.to_string(),
    };
    let saved = crate::mail::MailSettings {
        host: body.host.unwrap_or(current.host),
        port: body.port.unwrap_or(current.port),
        username: body.username.unwrap_or(current.username),
        password,
        from: body.from.unwrap_or(current.from),
        tls,
        login_url: body.login_url.unwrap_or(current.login_url),
    };
    pdm_db::set_mail_settings(
        &state.pool,
        org,
        &serde_json::json!({
            "host": saved.host,
            "port": saved.port,
            "username": saved.username,
            "password": saved.password,
            "from": saved.from,
            "tls": saved.tls,
            "login_url": saved.login_url,
        }),
    )
    .await?;
    Ok(saved.public().into())
}

/// POST /settings/mail/test — send a test message to the signed-in admin.
pub async fn test_mail_settings(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let admin = require_admin(&authed)?;
    let org = require_org(&authed);
    let mail =
        crate::mail::MailSettings::from_json(&pdm_db::get_mail_settings(&state.pool, org).await?);
    crate::mail::send_mail(
        &mail,
        &admin.email,
        "Foundry PDM mail test",
        "This is a test message from Foundry PDM. Mail is working.\n",
    )
    .await?;
    Ok(serde_json::json!({ "ok": true, "to": admin.email }).into())
}

#[derive(Deserialize)]
pub struct OnshapeSettingsReq {
    pub base_url: Option<String>,
    pub access_key: Option<String>,
    pub secret: Option<String>,
    pub default_project_id: Option<Uuid>,
    pub folder_sync_enabled: Option<bool>,
}

/// GET /settings/onshape — org Onshape API keys (secret never returned).
pub async fn get_onshape_settings(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    let raw = pdm_db::get_onshape_settings(&state.pool, org).await?;
    Ok(crate::onshape::OnshapeSettings::from_json(&raw)
        .public()
        .into())
}

/// PUT /settings/onshape — save Onshape API keys (admin). Empty secret keeps the current one.
pub async fn put_onshape_settings(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<OnshapeSettingsReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    if let Some(project_id) = body.default_project_id
        && pdm_db::get_project(&state.pool, project_id, org)
            .await?
            .is_none()
    {
        return Err(AppError(PdmError::NotFound(format!(
            "project {project_id}"
        ))));
    }
    let current = crate::onshape::OnshapeSettings::from_json(
        &pdm_db::get_onshape_settings(&state.pool, org).await?,
    );
    let secret = match body.secret.as_deref().map(str::trim) {
        Some("") | None => current.secret,
        Some(s) => s.to_string(),
    };
    let saved = crate::onshape::OnshapeSettings {
        base_url: body
            .base_url
            .unwrap_or(current.base_url)
            .trim()
            .trim_end_matches('/')
            .to_string(),
        access_key: body.access_key.unwrap_or(current.access_key),
        secret,
        default_project_id: body.default_project_id.or(current.default_project_id),
        folder_sync_enabled: body
            .folder_sync_enabled
            .unwrap_or(current.folder_sync_enabled),
    };
    pdm_db::set_onshape_settings(
        &state.pool,
        org,
        &serde_json::json!({
            "base_url": saved.base_url,
            "access_key": saved.access_key,
            "secret": saved.secret,
            "default_project_id": saved.default_project_id,
            "folder_sync_enabled": saved.folder_sync_enabled,
        }),
    )
    .await?;
    Ok(saved.public().into())
}

#[derive(Deserialize)]
pub struct OnshapeSendReq {
    pub document_id: String,
    pub workspace_id: String,
    pub element_id: String,
    pub element_type: Option<String>,
    pub project_id: Option<Uuid>,
    pub item_id: Option<Uuid>,
    pub part_number: Option<String>,
    pub description: Option<String>,
    pub format: Option<String>,
    pub part_id: Option<String>,
    pub part_name: Option<String>,
    pub source_element_id: Option<String>,
    pub source_document_id: Option<String>,
    pub source_workspace_id: Option<String>,
    pub source_workspace_type: Option<String>,
    pub source_configuration: Option<String>,
}

/// POST /integrations/onshape/send — export the Onshape element and check it into PDM.
pub async fn onshape_send(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<OnshapeSendReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_role(&authed, "editor")?.clone();
    let org = require_org(&authed);
    if body.document_id.trim().is_empty() || body.element_id.trim().is_empty() {
        return Err(AppError(PdmError::Validation(
            "document_id and element_id are required".into(),
        )));
    }
    let settings = crate::onshape::OnshapeSettings::from_json(
        &pdm_db::get_onshape_settings(&state.pool, org).await?,
    );
    let result = crate::onshape::send_to_pdm(
        &state,
        org,
        user.id,
        &settings,
        crate::onshape::SendReq {
            document_id: body.document_id.trim().to_string(),
            workspace_id: body.workspace_id.trim().to_string(),
            element_id: body.element_id.trim().to_string(),
            element_type: body.element_type.unwrap_or_else(|| "partstudio".into()),
            project_id: body.project_id.or(settings.default_project_id),
            item_id: body.item_id,
            part_number: body.part_number.or_else(|| body.part_name.clone()),
            description: body.description.or_else(|| body.part_name.clone()),
            format: body.format.unwrap_or_else(|| "STEP".into()),
            part_id: body.part_id,
            part_name: body.part_name,
            source_element_id: body.source_element_id,
            source_document_id: body.source_document_id,
            source_workspace_id: body.source_workspace_id,
            source_workspace_type: body.source_workspace_type,
            source_configuration: body.source_configuration,
        },
    )
    .await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "onshape.send",
        "item",
        result
            .get("item_id")
            .and_then(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok())),
        Some(result.clone()),
    )
    .await?;
    Ok(result.into())
}

#[derive(serde::Deserialize)]
pub struct OnshapeImportReq {
    pub url: String,
    pub name: Option<String>,
    pub part_number: Option<String>,
}

/// One Onshape Part Studio link, in the shapes Onshape share links and the PDM
/// picker produce:
/// `https://cad.onshape.com/documents/<doc>/w/<ws>/e/<element>` or the
/// immutable `.../v/<version>/e/<element>` variant. Returns
/// `(document_id, element_id)`.
fn parse_onshape_part_studio_url(url: &str) -> Result<(String, String), PdmError> {
    fn is_onshape_id(s: &str) -> bool {
        s.len() == 24 && s.bytes().all(|b| b.is_ascii_hexdigit())
    }
    let rest = url
        .trim()
        .strip_prefix("https://")
        .ok_or_else(|| PdmError::BadRequest("Onshape URL must start with https://".into()))?;
    let seg: Vec<&str> = rest.split('/').collect();
    let host_ok = seg
        .first()
        .is_some_and(|h| matches!(*h, "cad.onshape.com" | "onshape.com"));
    let valid = match seg.as_slice() {
        [_, "documents", doc, "w", ws, "e", el, ..]
            if host_ok && is_onshape_id(doc) && is_onshape_id(ws) && is_onshape_id(el) =>
        {
            Some((doc.to_string(), el.to_string()))
        }
        [_, "documents", doc, "v", ver, "e", el, ..]
            if host_ok && is_onshape_id(doc) && is_onshape_id(ver) && is_onshape_id(el) =>
        {
            Some((doc.to_string(), el.to_string()))
        }
        _ => None,
    };
    valid.ok_or_else(|| {
        PdmError::BadRequest(
            "use a Part Studio link like https://cad.onshape.com/documents/<id>/w/<workspace>/e/<element>".into(),
        )
    })
}

/// POST /projects/{project_id}/onshape-import — import one Onshape Part Studio
/// into the project: the worker's `onshape.import` job fetches the offline
/// `.onshape-bundle` (Dutchman's Onshape reader, the org's channel must
/// reference a build that carries it) and checks it in as a version; the usual
/// `cad.solidcraft` conversion then follows. Re-imports land on the item
/// already tied to that document element.
pub async fn onshape_import_request(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<OnshapeImportReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&authed)?.clone();
    let org = require_org(&authed);
    require_role(&authed, "editor")?;
    let (doc, element) = parse_onshape_part_studio_url(&body.url).map_err(AppError)?;
    let settings = crate::onshape::OnshapeSettings::from_json(
        &pdm_db::get_onshape_settings(&state.pool, org).await?,
    );
    if !settings.configured() {
        return Err(AppError(PdmError::BadRequest(
            "Onshape API keys are not configured — set them in Settings".into(),
        )));
    }
    let has_dutchman = std::env::var(pdm_update::DUTCHMAN_CLI_ENV)
        .is_ok_and(|p| !p.trim().is_empty())
        || !pdm_update::dutchman_update_disabled();
    if !has_dutchman {
        return Err(AppError(PdmError::BadRequest(
            "Dutchman is not available (no PDM_DUTCHMAN_CLI set, and PDM_DUTCHMAN_UPDATE=0)".into(),
        )));
    }
    pdm_db::get_project(&state.pool, project_id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("project {project_id}"))))?;
    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let item = match pdm_db::find_item_by_onshape(&state.pool, org, &doc, &element, None).await? {
        Some(existing) => {
            if existing.project_id != project_id {
                return Err(AppError(PdmError::Conflict(
                    "This Onshape Part Studio is already linked to an item in another project"
                        .into(),
                )));
            }
            existing
        }
        None => {
            let classification = "BE";
            let prefixes = pdm_db::list_part_number_prefixes(&state.pool, org).await?;
            let prefix = pdm_core::prefix_for_category(&prefixes, classification);
            let part_number = body
                .part_number
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or(name.clone())
                .unwrap_or_else(|| {
                    // The auto part number is filled in below; a placeholder
                    // cannot be returned from this sync arm.
                    String::new()
                });
            let part_number = if part_number.is_empty() {
                let seq = pdm_db::next_part_number(&state.pool, org, &prefix).await?;
                pdm_core::generate_part_number(&prefix, seq)
            } else {
                part_number
            };
            let description = name
                .clone()
                .unwrap_or_else(|| format!("Onshape {doc}/{element}"));
            pdm_db::create_item(
                &state.pool,
                org,
                project_id,
                None,
                &part_number,
                &description,
                classification,
                Some(user.id),
            )
            .await?
        }
    };
    let _ = pdm_db::upsert_property(
        &state.pool,
        org,
        item.id,
        "onshape.document_id",
        "string",
        &serde_json::json!(doc),
    )
    .await;
    let _ = pdm_db::upsert_property(
        &state.pool,
        org,
        item.id,
        "onshape.element_id",
        "string",
        &serde_json::json!(element),
    )
    .await;
    let payload = serde_json::json!({
        "item_id": item.id,
        "url": body.url.trim(),
        "author_id": user.id,
    });
    let job = pdm_db::enqueue_job(&state.pool, org, "onshape.import", &payload).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "project.onshape_import",
        "item",
        Some(item.id),
        Some(serde_json::json!({ "url": body.url.trim(), "project_id": project_id, "job_id": job.id })),
    )
    .await?;
    Ok(serde_json::json!({ "item": item, "job": job }).into())
}

/// GET /settings/cad-cli — installed SolidCraft CLI vs GitHub latest.
pub async fn get_cad_cli(
    Extension(_authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let path = std::env::var("PDM_SOLVECRAFT_CLI")
        .unwrap_or_else(|_| pdm_update::CAD_CLI_DEFAULT_PATH.to_string());
    let installed = tokio::process::Command::new(&path)
        .arg("--version")
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    let latest = pdm_update::fetch_cad_cli_latest()
        .await
        .ok()
        .map(|r| r.tag_name);
    let update_available = match (&latest, &installed) {
        (Some(tag), Some(cur)) => pdm_update::is_newer(tag, cur),
        (Some(_), None) => true,
        _ => false,
    };
    Ok(serde_json::json!({
        "path": path,
        "installed": installed,
        "latest": latest,
        "update_available": update_available,
        "source": "https://github.com/tangency-labs/solidcraft/releases/latest",
    })
    .into())
}

/// POST /settings/cad-cli/update — download GitHub latest solidcraft-cli onto this host.
pub async fn update_cad_cli(
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let dest = std::env::var("PDM_SOLVECRAFT_CLI")
        .unwrap_or_else(|_| pdm_update::CAD_CLI_DEFAULT_PATH.to_string());
    let tag = pdm_update::apply_cad_cli(std::path::Path::new(&dest))
        .await
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
    Ok(serde_json::json!({ "ok": true, "installed": tag, "path": dest }).into())
}

/// GET /settings/dutchman — the installed Dutchman (native-CAD → SolidCraft
/// feature-tree converter), this org's conversion settings, and the release
/// channels to pick from.
pub async fn get_dutchman(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let path = pdm_update::dutchman_cli_path();
    let installed = if std::path::Path::new(&path).exists() {
        pdm_update::dutchman_version_of(std::path::Path::new(&path)).await
    } else {
        None
    };
    let info = pdm_update::read_dutchman_info(std::path::Path::new(&path));
    let settings = pdm_db::get_dutchman_settings(&state.pool, org).await?;
    let latest = pdm_update::fetch_dutchman_latest()
        .await
        .ok()
        .map(|r| r.tag_name);
    // A due update: no binary yet, or the installed build came from another
    // channel than the org's (the boot/interval loop chases marker changes).
    let update_available = !std::path::Path::new(&path).exists()
        || info
            .as_ref()
            .map(|i| i.channel != settings.channel)
            .unwrap_or(true);
    let channels = pdm_update::dutchman_channels().await.map_or_else(
        |_| serde_json::json!([{ "value": "latest", "label": format!("Latest stable ({})", latest.as_deref().unwrap_or("?")) }, { "value": "nightly", "label": "Nightly (includes unreleased work)" }]),
        |opts| {
            serde_json::json!(opts
                .into_iter()
                .map(|(value, label)| serde_json::json!({ "value": value, "label": label }))
                .collect::<Vec<_>>())
        },
    );
    Ok(serde_json::json!({
        "path": path,
        "installed": installed,
        "info": info,
        "settings": settings,
        "latest": latest,
        "update_available": update_available,
        "channels": channels,
        "auto_update": !pdm_update::dutchman_update_disabled(),
        "source": "https://github.com/dutchman-cad/dutchman/releases/latest",
    })
    .into())
}

/// PUT /settings/dutchman — this org's Dutchman conversion settings (admin).
#[derive(serde::Deserialize)]
pub struct DutchmanSettingsReq {
    channel: Option<String>,
    preset: Option<String>,
    part_positions: Option<String>,
    verify: Option<bool>,
    timeout_secs: Option<u64>,
    onshape_max_calls: Option<u32>,
}

pub async fn save_dutchman(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<DutchmanSettingsReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    let mut settings = pdm_db::get_dutchman_settings(&state.pool, org).await?;
    if let Some(v) = body.channel.filter(|s| !s.trim().is_empty()) {
        settings.channel = v.trim().to_string();
    }
    if let Some(v) = body.preset.filter(|s| !s.trim().is_empty()) {
        settings.preset = v.trim().to_string();
    }
    if let Some(v) = body.part_positions.filter(|s| !s.trim().is_empty()) {
        settings.part_positions = v.trim().to_string();
    }
    if let Some(v) = body.verify {
        settings.verify = v;
    }
    if let Some(v) = body.timeout_secs {
        settings.timeout_secs = v;
    }
    if let Some(v) = body.onshape_max_calls {
        settings.onshape_max_calls = v;
    }
    pdm_db::set_dutchman_settings(&state.pool, org, &settings).await?;
    Ok(serde_json::json!({ "settings": settings }).into())
}

/// GET /settings/nest-defaults — the org's saved laser-nest create-panel
/// defaults (null when unset).
pub async fn get_nest_defaults(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let defaults = pdm_db::get_org_nest_defaults(&state.pool, org).await?;
    Ok(serde_json::json!({ "defaults": defaults }).into())
}

/// PUT /settings/nest-defaults — save the org's laser-nest defaults (admin).
pub async fn save_nest_defaults(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<pdm_core::NestDefaults>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    pdm_db::set_org_nest_defaults(&state.pool, org, &body).await?;
    Ok(serde_json::json!({ "defaults": body }).into())
}

/// POST /settings/dutchman/update — pull the release the org's channel
/// references onto this host now (admin).
pub async fn update_dutchman(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    let settings = pdm_db::get_dutchman_settings(&state.pool, org).await?;
    let dest = std::path::PathBuf::from(pdm_update::dutchman_cli_path());
    let rel = pdm_update::apply_dutchman_tag(&dest, &settings.channel, &settings.channel)
        .await
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
    Ok(serde_json::json!({
        "ok": true,
        "channel": settings.channel,
        "installed": rel.tag_name,
        "path": dest.to_string_lossy(),
    })
    .into())
}

/// Where the SolidCraft web build is served from at runtime: the portal
/// directory's `editors/solidcraft` (the image layout), else the source tree's
/// `static/editors/solidcraft` (dev).
fn solidcraft_web_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = pdm_update::solidcraft_web_dir() {
        return Some(dir);
    }
    let dev = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static/editors/solidcraft");
    dev.is_dir().then_some(dev)
}

/// GET /settings/solidcraft-web — the installed "Edit in SolidCraft web" build
/// vs the newest SolidCraft release on GitHub.
pub async fn get_solidcraft_web(
    Extension(_authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let dir = solidcraft_web_dir();
    let installed = dir
        .as_ref()
        .and_then(|d| pdm_update::installed_solidcraft_web_version(d));
    let latest = pdm_update::fetch_cad_cli_latest()
        .await
        .ok()
        .map(|r| r.tag_name);
    let update_available = match (&latest, &installed) {
        (Some(tag), Some(cur)) => pdm_update::is_newer(tag, cur),
        _ => false,
    };
    Ok(serde_json::json!({
        "dir": dir.map(|d| d.to_string_lossy().into_owned()),
        "installed": installed,
        "latest": latest,
        "update_available": update_available,
        "auto_update": !pdm_update::sc_web_update_disabled(),
        "source": "https://github.com/tangency-labs/solidcraft/releases/latest",
    })
    .into())
}

/// POST /settings/solidcraft-web/update — download the newest web build onto
/// this host now (admin).
pub async fn update_solidcraft_web(
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let dir = solidcraft_web_dir().ok_or_else(|| {
        AppError(PdmError::BadRequest(
            "no SolidCraft web build directory on this server (PDM_PORTAL_DIR)".into(),
        ))
    })?;
    let tag = pdm_update::apply_solidcraft_web(&dir)
        .await
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
    Ok(serde_json::json!({ "ok": true, "installed": tag, "dir": dir.to_string_lossy() }).into())
}

/// GET /integrations/onshape/config — what a CAD user needs to one-click send (no secrets).
pub async fn onshape_config(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let settings = crate::onshape::OnshapeSettings::from_json(
        &pdm_db::get_onshape_settings(&state.pool, org).await?,
    );
    Ok(serde_json::json!({
      "configured": settings.configured(),
      "default_project_id": settings.default_project_id,
      "app_url": "/static/onshape.html",
      "build": version_info().get("build").cloned().unwrap_or(serde_json::Value::Null),
    })
    .into())
}

pub async fn onshape_preview(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<crate::onshape::PreviewQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_user(&authed)?;
    Ok(crate::onshape::preview(&state, require_org(&authed), &q)
        .await?
        .into())
}

/// GET /integrations/onshape/picker — the document's tabs + Part Studio bodies.
pub async fn onshape_picker(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<OnshapePickerQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let _user = require_user(&authed)?;
    let org = require_org(&authed);
    let result = crate::onshape::picker(&state, org, &q.document_id, &q.workspace_id).await?;
    Ok(result.into())
}

#[derive(Deserialize)]
pub struct OnshapePickerQuery {
    pub document_id: String,
    pub workspace_id: String,
}

/// GET /integrations/onshape/lookup — the PDM item already tied to this Onshape tab.
pub async fn onshape_lookup(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<OnshapeLookupQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let item = pdm_db::find_item_by_onshape(
        &state.pool,
        org,
        &q.document_id,
        &q.element_id,
        q.part_id.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "item": item }).into())
}

#[derive(Deserialize)]
pub struct OnshapeLookupQuery {
    pub document_id: String,
    pub element_id: String,
    pub part_id: Option<String>,
}

/// PUT /settings/gpu-policy — set preview renderer (admin). `auto` | `hardware` | `software` | `cpu`.
pub async fn put_gpu_policy(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<GpuPolicyReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    let gpu_policy = pdm_db::set_gpu_policy(&state.pool, org, &body.gpu_policy).await?;
    Ok(serde_json::json!({ "gpu_policy": gpu_policy }).into())
}

/// DELETE /settings/part-number-prefixes/{category} — remove a prefix (admin).
pub async fn delete_part_number_prefix(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(category): Path<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_admin(&authed)?;
    let org = require_org(&authed);
    let deleted = pdm_db::delete_part_number_prefix(&state.pool, org, &category).await?;
    Ok(serde_json::json!({ "deleted": deleted }).into())
}

// ── Projects ────────────────────────────────────────────────────────────

/// GET /projects — archived projects are hidden unless `?include_archived=true`.
pub async fn list_projects(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let projects =
        pdm_db::list_projects(&state.pool, org, include_archived_flag(q.include_archived)).await?;
    Ok(serde_json::json!({ "projects": projects }).into())
}

/// PATCH /projects/{project_id}/archive — soft-delete (archive) a project.
pub async fn archive_project(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let project = pdm_db::archive_project(&state.pool, org, project_id).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "project.archived",
        "project",
        Some(project_id),
        Some(serde_json::json!({ "name": project.name })),
    )
    .await?;
    Ok(serde_json::json!({ "project": project }).into())
}

/// PATCH /projects/{project_id}/unarchive — undo a soft-delete.
pub async fn unarchive_project(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let project = pdm_db::unarchive_project(&state.pool, org, project_id).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "project.unarchived",
        "project",
        Some(project_id),
        Some(serde_json::json!({ "name": project.name })),
    )
    .await?;
    Ok(serde_json::json!({ "project": project }).into())
}

/// DELETE /projects/{project_id} — hard-delete a project (cascades to items).
#[derive(Deserialize)]
pub struct FolderSyncLinkReq {
    pub folder_id: String,
}

pub async fn get_folder_sync(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_user(&authed)?;
    let org = require_org(&authed);
    pdm_db::get_project(&state.pool, project, org)
        .await?
        .ok_or_else(|| PdmError::NotFound("Project not found".into()))?;
    let settings = crate::onshape::OnshapeSettings::from_json(
        &pdm_db::get_onshape_settings(&state.pool, org).await?,
    );
    let row: Option<(String, String, String, serde_json::Value)> = sqlx::query_as("SELECT folder_id, folder_name, status, result FROM onshape_folder_sync WHERE project_id=$1 AND org_id=$2")
        .bind(project).bind(org).fetch_optional(&state.pool).await.map_err(PdmError::Db)?;
    Ok(serde_json::json!({"enabled": settings.folder_sync_enabled, "link": row.map(|(folder_id,folder_name,status,result)| serde_json::json!({"folder_id":folder_id,"folder_name":folder_name,"status":status,"result":result}))}).into())
}

pub async fn put_folder_sync(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project): Path<Uuid>,
    Json(body): Json<FolderSyncLinkReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    pdm_db::get_project(&state.pool, project, org)
        .await?
        .ok_or_else(|| PdmError::NotFound("Project not found".into()))?;
    let settings = crate::onshape::OnshapeSettings::from_json(
        &pdm_db::get_onshape_settings(&state.pool, org).await?,
    );
    if !settings.folder_sync_enabled {
        return Err(
            PdmError::BadRequest("Enable Onshape folder sync in Settings first".into()).into(),
        );
    }
    let folder = body.folder_id.trim();
    let name = crate::onshape::folder_name(&settings, folder).await?;
    let changed = sqlx::query("INSERT INTO onshape_folder_sync(project_id,org_id,folder_id,folder_name) VALUES($1,$2,$3,$4) ON CONFLICT(project_id) DO UPDATE SET folder_id=EXCLUDED.folder_id, folder_name=EXCLUDED.folder_name,status='idle',result='{}'::jsonb,updated_at=now() WHERE onshape_folder_sync.status <> 'running'")
        .bind(project).bind(org).bind(folder).bind(&name).execute(&state.pool).await.map_err(PdmError::Db)?;
    if changed.rows_affected() == 0 {
        return Err(PdmError::BadRequest("Wait for the current sync to finish".into()).into());
    }
    Ok(serde_json::json!({"folder_id":folder,"folder_name":name}).into())
}

pub async fn start_folder_sync(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_role(&authed, "editor")?.id;
    let org = require_org(&authed);
    let settings = crate::onshape::OnshapeSettings::from_json(
        &pdm_db::get_onshape_settings(&state.pool, org).await?,
    );
    if !settings.folder_sync_enabled {
        return Err(PdmError::BadRequest("Onshape folder sync is disabled".into()).into());
    }
    let folder: Option<String> = sqlx::query_scalar("UPDATE onshape_folder_sync SET status='running',result='{}'::jsonb,updated_at=now() WHERE project_id=$1 AND org_id=$2 AND status <> 'running' RETURNING folder_id")
        .bind(project).bind(org).fetch_optional(&state.pool).await.map_err(PdmError::Db)?;
    let folder = folder.ok_or_else(|| {
        PdmError::BadRequest("Link a folder first, or wait for the current sync".into())
    })?;
    tokio::spawn(async move {
        let outcome = crate::onshape::sync_folder(&state, org, user, project, &folder).await;
        let (status, result) = match outcome {
            Ok(v) => ("completed", v),
            Err(e) => ("failed", serde_json::json!({"error":e.to_string()})),
        };
        if let Err(e) = sqlx::query("UPDATE onshape_folder_sync SET status=$1,result=$2,updated_at=now() WHERE project_id=$3 AND org_id=$4")
            .bind(status).bind(result).bind(project).bind(org).execute(&state.pool).await { tracing::error!("Onshape sync status: {e}"); }
    });
    Ok(serde_json::json!({"status":"running"}).into())
}

/// DELETE /projects/{project_id} — hard-delete a project (cascades to items).
pub async fn delete_project(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let project = pdm_db::delete_project(&state.pool, org, project_id).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "project.deleted",
        "project",
        Some(project_id),
        Some(serde_json::json!({ "name": project.name })),
    )
    .await?;
    Ok(serde_json::json!({ "ok": true, "project_id": project_id }).into())
}

/// PATCH /projects/{project_id} — rename, description, cover image.
pub async fn update_project(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<UpdateProjectReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let desc = body.description.unwrap_or_default();
    let project = pdm_db::update_project(
        &state.pool,
        org,
        project_id,
        &body.name,
        &desc,
        body.image_data.as_deref(),
    )
    .await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "project.updated",
        "project",
        Some(project_id),
        Some(serde_json::json!({ "name": project.name })),
    )
    .await?;
    Ok(serde_json::json!({ "project": project }).into())
}

/// PUT /projects/{project_id}/main-assembly — designate (or clear, with a
/// `null` item) the project's main assembly: the root item the BOM is built off
/// and the source of the project's cover image.
pub async fn set_main_assembly(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<SetMainAssemblyReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let project = pdm_db::set_main_assembly(&state.pool, org, project_id, body.item_id).await?;
    let action = if body.item_id.is_some() {
        "project.main_assembly_set"
    } else {
        "project.main_assembly_cleared"
    };
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        action,
        "project",
        Some(project_id),
        Some(serde_json::json!({
            "project": project.name,
            "item_id": body.item_id,
        })),
    )
    .await?;
    Ok(serde_json::json!({ "project": project }).into())
}

/// POST /projects
pub async fn create_project(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateProjectReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let desc = body.description.unwrap_or_default();
    let project = pdm_db::create_project(&state.pool, org, &body.name, &desc).await?;
    Ok(serde_json::json!({ "project": project }).into())
}

// ── Items ───────────────────────────────────────────────────────────────

/// POST /projects/{project_id}/items
pub async fn create_item(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateItemReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    // Without this, a missing project id was a 500, and another organization's project id made an
    // item in this organization pointing at it.
    require_project_and_folder(&state, org, project_id, body.folder_id).await?;
    let classification = body.classification.unwrap_or_else(|| "BE".to_string());

    // Resolve the configured part-number prefix for this category. If the org
    // hasn't defined one, fall back to the classification itself (backward
    // compatible with pre-config items like "BE", "ASM", "MFG").
    let prefixes = pdm_db::list_part_number_prefixes(&state.pool, org).await?;
    let prefix = pdm_core::prefix_for_category(&prefixes, &classification);

    let part_number = if let Some(pn) = &body.part_number {
        pn.clone()
    } else {
        let seq = pdm_db::next_part_number(&state.pool, org, &prefix).await?;
        pdm_core::generate_part_number(&prefix, seq)
    };

    let desc = body.description.unwrap_or_default();
    let item = pdm_db::create_item(
        &state.pool,
        org,
        project_id,
        body.folder_id,
        &part_number,
        &desc,
        &classification,
        Some(user.id),
    )
    .await?;

    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.created",
        "item",
        Some(item.id),
        Some(serde_json::json!({ "part_number": item.part_number })),
    )
    .await?;

    Ok(serde_json::json!({ "item": item }).into())
}

// ── Folders ─────────────────────────────────────────────────────────────

/// GET /projects/{project_id}/folders
pub async fn list_folders(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let folders = pdm_db::list_folders(&state.pool, org, project_id).await?;
    Ok(serde_json::json!({ "folders": folders }).into())
}

/// POST /projects/{project_id}/folders
pub async fn create_folder(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
    Json(body): Json<CreateFolderReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    require_project_and_folder(&state, org, project_id, body.parent_id).await?;
    let folder =
        pdm_db::create_folder(&state.pool, org, project_id, body.parent_id, &body.name).await?;

    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "folder.created",
        "folder",
        Some(folder.id),
        Some(serde_json::json!({ "name": folder.name })),
    )
    .await?;

    Ok(serde_json::json!({ "folder": folder }).into())
}

/// GET /items/{item_id}
pub async fn get_item(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let item = pdm_db::get_item_summary(&state.pool, item_id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("item {item_id}"))))?;
    let file = pdm_db::get_file(&state.pool, item_id).await?;
    let lock = pdm_db::get_lock(&state.pool, item_id).await?;
    let versions = match &file {
        Some(f) => pdm_db::list_version_details(&state.pool, f.id).await?,
        None => vec![],
    };
    let head_version = versions.first().cloned();

    Ok(serde_json::json!({
        "item": item,
        "file": file,
        "lock": lock,
        "versions": versions,
        "head_version": head_version,
    })
    .into())
}

/// GET /projects/{project_id}/items — paginated, server-side filtered item
/// list for a project. Archvied items are hidden unless `?include_archived=true`.
pub async fn list_items(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(project_id): Path<Uuid>,
    Query(q): Query<ListParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let (page, limit) = pdm_db::pagination(q.page, q.limit);
    let p = pdm_db::list_items_paginated(
        &state.pool,
        org,
        Some(project_id),
        include_archived_flag(q.include_archived),
        q.classification.as_deref(),
        q.state.as_deref(),
        q.kind.as_deref(),
        q.owner,
        q.folder,
        q.q.as_deref(),
        q.sheet_metal,
        q.sort.as_deref(),
        q.dir.as_deref(),
        page,
        limit,
    )
    .await?;
    Ok(serde_json::json!({
        "items": p.items,
        "page": p.page,
        "limit": p.limit,
        "total": p.total,
        "has_more": p.has_more,
    })
    .into())
}

/// GET /items — org-wide item list with the same pagination and filters as
/// `GET /projects/{id}/items`, replacing the per-project fan-out.
pub async fn list_items_org(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<ListParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let (page, limit) = pdm_db::pagination(q.page, q.limit);
    let p = pdm_db::list_items_paginated(
        &state.pool,
        org,
        None,
        include_archived_flag(q.include_archived),
        q.classification.as_deref(),
        q.state.as_deref(),
        q.kind.as_deref(),
        q.owner,
        q.folder,
        q.q.as_deref(),
        q.sheet_metal,
        q.sort.as_deref(),
        q.dir.as_deref(),
        page,
        limit,
    )
    .await?;
    Ok(serde_json::json!({
        "items": p.items,
        "page": p.page,
        "limit": p.limit,
        "total": p.total,
        "has_more": p.has_more,
    })
    .into())
}

/// PATCH /items/{item_id}/archive — soft-delete (archive) an item.
pub async fn archive_item(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let item = pdm_db::archive_item(&state.pool, org, item_id).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.archived",
        "item",
        Some(item_id),
        Some(serde_json::json!({ "part_number": item.part_number })),
    )
    .await?;
    Ok(serde_json::json!({ "item": item }).into())
}

/// PATCH /items/{item_id}/unarchive — undo a soft-delete.
pub async fn unarchive_item(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let item = pdm_db::unarchive_item(&state.pool, org, item_id).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.unarchived",
        "item",
        Some(item_id),
        Some(serde_json::json!({ "part_number": item.part_number })),
    )
    .await?;
    Ok(serde_json::json!({ "item": item }).into())
}

/// DELETE /items/{item_id} — hard-delete an item (409 when it holds a live lock).
pub async fn delete_item(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let item = pdm_db::delete_item(&state.pool, org, item_id).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.deleted",
        "item",
        Some(item_id),
        Some(serde_json::json!({ "part_number": item.part_number })),
    )
    .await?;
    Ok(serde_json::json!({ "ok": true, "item_id": item_id }).into())
}

// ── Checkout / Checkin ──────────────────────────────────────────────────

/// POST /items/{item_id}/checkout
pub async fn checkout(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
    Json(body): Json<CheckoutReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    require_item_in_org(&state, org, item_id).await?;
    let workstation = body.workstation.unwrap_or_else(|| "unknown".to_string());

    // The file record may not exist before the first check-in.
    let file = pdm_db::get_file(&state.pool, item_id).await?;
    let head = match &file {
        Some(f) => pdm_db::get_head_version(&state.pool, f.id).await?,
        None => None,
    };
    let base_version_id = head.as_ref().map(|v| v.id);

    let lock = pdm_db::acquire_lock(
        &state.pool,
        item_id,
        user.id,
        &workstation,
        base_version_id,
        state.lease_hours,
    )
    .await?;

    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.checkout",
        "item",
        Some(item_id),
        Some(serde_json::json!({ "workstation": workstation, "base_version_id": base_version_id })),
    )
    .await?;

    Ok(serde_json::json!({
        "lock": lock,
        "base_version": head,
    })
    .into())
}

/// DELETE /items/{item_id}/checkout
pub async fn undo_checkout(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_user(&authed)?.clone();
    require_item_in_org(&state, org, item_id).await?;
    pdm_db::release_lock(&state.pool, item_id, user.id).await?;

    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.undo_checkout",
        "item",
        Some(item_id),
        None,
    )
    .await?;

    Ok(serde_json::json!({ "ok": true }).into())
}

/// POST /items/{item_id}/checkin
pub async fn checkin(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
    Json(body): Json<CheckinReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    require_item_in_org(&state, org, item_id).await?;

    // 1. Verify lock is held by this user (a quick refusal; checked again in step 4)
    let lock = pdm_db::get_lock(&state.pool, item_id)
        .await?
        .ok_or_else(|| AppError(PdmError::Conflict("no active checkout".into())))?;
    if lock.user_id != user.id {
        return Err(AppError(PdmError::Conflict(
            "lock held by another user".into(),
        )));
    }

    let filename = body.filename.clone().unwrap_or_default();
    let mime = body
        .mime_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());

    // 3. Verify blob integrity (the store, before the transaction)
    let blob = pdm_db::get_blob_by_sha(&state.pool, org, &body.sha256)
        .await?
        .ok_or_else(|| {
            AppError(PdmError::IntegrityMismatch {
                expected: body.sha256.clone(),
                actual: "not_found".into(),
            })
        })?;
    state.store.verify_by_sha(&body.sha256).await.map_err(|e| {
        AppError(PdmError::IntegrityMismatch {
            expected: body.sha256.clone(),
            actual: e.to_string(),
        })
    })?;

    // 4. The lease check, the base check (step 2), the file record, the version and the lease
    //    release (step 5), as one transaction: parallel check-ins of one item run one at a time.
    let commit_msg = body.commit_message.unwrap_or_default();
    let native_format = std::path::Path::new(&filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let new_version = body.new_version.unwrap_or(false);
    let checked_in = pdm_db::checkin_version(
        &state.pool,
        item_id,
        user.id,
        &filename,
        &mime,
        body.base_version,
        blob.id,
        &commit_msg,
        &native_format,
        new_version,
    )
    .await?;
    let version = checked_in.version;
    let head = checked_in.previous_head;

    // 4b. Enqueue background analysis (measurements via `solvecraft-cli eval`) and
    //     thumbnail rendering (`solvecraft-cli snapshot`). The jobs are independent
    //     so a missing CLI does not block analysis; the worker stores each result
    //     on the version when it succeeds.
    let job_payload =
        serde_json::json!({ "version_id": version.id, "item_id": item_id, "filename":filename });
    let _ = pdm_db::enqueue_job(&state.pool, org, "design.analyze", &job_payload).await;
    let _ = pdm_db::enqueue_job(&state.pool, org, "preview.render", &job_payload).await;
    // Inventor, FreeCAD, Rhino, STEP and IGES files are additionally restated as STEP
    // (the worker's `cad.convert` job), so the portal and SolidCraft can open or download
    // the part without the native format.
    if pdm_core::is_cad_convert_path(&filename) {
        let _ = pdm_db::enqueue_job(&state.pool, org, "cad.convert", &job_payload).await;
    }

    // 4c. Flag any NC program for this item that isn't based on the new version as potentially
    //     outdated (the source design changed). Only a new major version does this: revisions
    //     within a version stay compatible by definition. Best-effort: MFG is optional.
    if new_version {
        let _ = pdm_db::flag_outdated_nc_programs(&state.pool, org, item_id, version.id).await;
        // A new CAD version supersedes any laser nest that pinned the old one.
        if let Some(old_head) = &head {
            let _ = pdm_db::mark_laser_nests_stale_for_version(
                &state.pool,
                org,
                old_head.id,
                "a new CAD version was checked in",
            )
            .await;
        }
    }

    // 5. (The lease was released in step 4's transaction.)

    // 6. Audit
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.checkin",
        "item",
        Some(item_id),
        Some(serde_json::json!({
            "version_seq": version.seq,
            "version": version.version,
            "revision": version.revision,
            "commit_message": commit_msg,
            "sha256": body.sha256
        })),
    )
    .await?;

    Ok(serde_json::json!({ "version": version }).into())
}

// ── Blob upload / download ──────────────────────────────────────────────

/// PUT /blobs — upload raw bytes, content-address by SHA-256.
pub async fn upload_blob(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    if body.len() as u64 > state.max_upload {
        return Err(AppError(PdmError::Validation(format!(
            "upload size {} exceeds limit {}",
            body.len(),
            state.max_upload
        ))));
    }
    if body.is_empty() {
        return Err(AppError(PdmError::Validation("empty upload".into())));
    }

    let (sha, size) = state
        .store
        .put_content_addressed(&body)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    let storage_key = sha.clone();

    let blob = pdm_db::upload_blob(
        &state.pool,
        org,
        &sha,
        i64::try_from(size).map_err(|_| PdmError::Validation("file too large".into()))?,
        &storage_key,
    )
    .await?;

    Ok(serde_json::json!({ "blob": blob }).into())
}

/// GET /versions/{version_id}/blob
pub async fn download_version_blob(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
) -> Result<Response, AppError> {
    owned_version_source(&state, &authed, version_id).await?;
    let version = pdm_db::get_version(&state.pool, version_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("version {version_id}"))))?;

    let blob = pdm_db::get_blob_by_id(&state.pool, version.blob_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("blob for version".into())))?;

    let data = state
        .store
        .get_by_sha(&blob.sha256)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;

    let digest = format!("sha-256={}", blob.sha256);
    let response = axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/octet-stream")
        .header("content-length", data.len().to_string())
        .header("content-digest", digest)
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(format!("response build: {e}"))))?;

    Ok(response)
}

/// GET /versions/{version_id}/derived-step — the converted STEP the `cad.convert` job
/// stored for a version (Inventor, FreeCAD, Rhino, STEP and IGES check-ins), if any.
pub async fn download_version_derived_step(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
) -> Result<Response, AppError> {
    owned_version_source(&state, &authed, version_id).await?;
    let derived = pdm_db::get_derived_step(&state.pool, version_id)
        .await?
        .ok_or_else(|| {
            AppError(PdmError::NotFound(format!(
                "version {version_id} has no converted STEP"
            )))
        })?;

    let data = state
        .store
        .get_by_sha(&derived.sha256)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    let digest = format!("sha-256={}", derived.sha256);
    let response = axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/octet-stream")
        .header("content-length", data.len().to_string())
        .header("content-digest", digest)
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(format!("response build: {e}"))))?;
    Ok(response)
}

async fn owned_version_source(
    state: &AppState,
    authed: &Authed,
    version_id: Uuid,
) -> Result<pdm_db::VersionSource, AppError> {
    pdm_db::version_source(&state.pool, authed.org_id, version_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("version {version_id}"))))
}

/// Metadata and progress for a native artifact; source versions remain immutable.
pub async fn solidcraft_conversion_status(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let source = owned_version_source(&state, &authed, version_id).await?;
    let artifact = pdm_db::get_solidcraft_artifact(&state.pool, version_id).await?;
    let job = match artifact.job_id {
        Some(id) => pdm_db::get_job(&state.pool, id, authed.org_id).await?,
        None => None,
    };
    Ok(Json(
        serde_json::json!({"version_id":version_id,"filename":pdm_core::solidcraft_artifact_filename(&source.filename,source.seq),"artifact":artifact,"job":job}),
    ))
}

pub async fn request_solidcraft_conversion(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_role(&authed, "editor")?;
    let source = owned_version_source(&state, &authed, version_id).await?;
    if !pdm_core::can_convert_to_solidcraft(&source.filename) {
        return Err(AppError(PdmError::Validation(
            "this file type does not need or support SolidCraft conversion".into(),
        )));
    }
    // A conversion needs Dutchman (the native formats with feature history) or the
    // SolidCraft CLI (meshes, IGES). Dutchman counts as available when it is
    // configured or the PDM may pull the latest release for it.
    let has_cli = std::env::var("PDM_SOLVECRAFT_CLI").is_ok_and(|p| !p.trim().is_empty());
    let has_dutchman = std::env::var(pdm_update::DUTCHMAN_CLI_ENV)
        .is_ok_and(|p| !p.trim().is_empty())
        || !pdm_update::dutchman_update_disabled();
    if !has_cli && !has_dutchman {
        return Err(AppError(PdmError::BadRequest(
            "SolidCraft conversion is not configured (no CAD CLI, and Dutchman updates are disabled)".into(),
        )));
    }
    pdm_db::queue_solidcraft_conversion(&state.pool, authed.org_id, version_id, &source).await?;
    pdm_db::record_audit(
        &state.pool,
        authed.org_id,
        Some(user.id),
        "",
        "version.convert_solidcraft",
        "version",
        Some(version_id),
        None,
    )
    .await?;
    solidcraft_conversion_status(State(state), Extension(authed), Path(version_id)).await
}

pub async fn download_solidcraft_artifact(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let source = owned_version_source(&state, &authed, version_id).await?;
    let artifact = pdm_db::get_solidcraft_artifact(&state.pool, version_id).await?;
    let sha = artifact.sha256.ok_or_else(|| {
        AppError(PdmError::NotFound(
            "SolidCraft artifact is not ready".into(),
        ))
    })?;
    let data = state
        .store
        .get_by_sha(&sha)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/octet-stream")
        .header("content-length", data.len().to_string())
        .header("content-digest", format!("sha-256={sha}"))
        .header(
            "content-disposition",
            format!(
                "attachment; filename=\"{}\"",
                pdm_core::solidcraft_artifact_filename(&source.filename, source.seq)
            ),
        )
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))
}

#[derive(sqlx::FromRow)]
struct PublicDesignRow {
    seq: i32,
    blob_id: Uuid,
    native_format: String,
    filename: String,
    derived_solidcraft_sha256: Option<String>,
}

/// The file a design link serves for a version: the derived .sc artifact's name and bytes when
/// one exists, otherwise the version's own source file.
async fn design_link_file(
    state: &AppState,
    version_id: Uuid,
) -> Result<(String, String), AppError> {
    let row = sqlx::query_as::<_, PublicDesignRow>(
        "SELECT v.seq, v.blob_id, v.native_format, f.filename, v.derived_solidcraft_sha256 \
         FROM file_versions v JOIN files f ON f.id = v.file_id WHERE v.id = $1",
    )
    .bind(version_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| AppError(PdmError::Db(e)))?
    .ok_or_else(|| AppError(PdmError::NotFound(format!("version {version_id}"))))?;
    let source = pdm_core::version_filename(&row.filename, &row.native_format);
    Ok(match row.derived_solidcraft_sha256 {
        Some(sha) => (
            pdm_core::solidcraft_artifact_filename(&source, row.seq),
            sha,
        ),
        None => {
            let blob = pdm_db::get_blob_by_id(&state.pool, row.blob_id)
                .await?
                .ok_or_else(|| AppError(PdmError::NotFound("blob for version".into())))?;
            (source, blob.sha256)
        }
    })
}

/// POST /versions/{version_id}/design-link — a short-lived link to the version's design for the
/// bundled SolidCraft web editor, whose fetch sends no credentials. Checks the caller's
/// organization; the link expires after `design_link::TTL_SECS`.
pub async fn create_design_link(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    owned_version_source(&state, &authed, version_id).await?;
    let (filename, _) = design_link_file(&state, version_id).await?;
    let expires_at = chrono::Utc::now() + chrono::Duration::seconds(design_link::TTL_SECS);
    let expires = expires_at.timestamp();
    let signature = state.design_links.sign(version_id, &filename, expires);
    let url = format!(
        "/public/design/{version_id}/{}?expires={expires}&signature={signature}",
        design_link::encode_path_segment(&filename)
    );
    Ok(Json(serde_json::json!({
        "url": url,
        "filename": filename,
        "expires_at": expires_at,
    })))
}

#[derive(Deserialize)]
pub struct DesignLinkQuery {
    pub expires: Option<String>,
    pub signature: Option<String>,
}

/// GET /public/design/{version_id}/{filename}?expires=&signature= — unauthenticated design
/// download for the bundled SolidCraft web editor: the app runs in a same-origin iframe and its
/// design-link fetch sends no credentials, so the link carries an expiry and a signature from
/// `POST /versions/{version_id}/design-link` instead of a token. With a derived .sc artifact the
/// filename must match that artifact; otherwise it must match the version's own source file.
pub async fn public_design_download(
    State(state): State<AppState>,
    Path((version_id, filename)): Path<(Uuid, String)>,
    Query(link): Query<DesignLinkQuery>,
) -> Result<Response, AppError> {
    let not_found = || AppError(PdmError::NotFound("design link".into()));
    let expires = link
        .expires
        .as_deref()
        .and_then(|e| e.parse::<i64>().ok())
        .ok_or_else(not_found)?;
    let signature = link.signature.as_deref().ok_or_else(not_found)?;
    match state.design_links.verify(
        version_id,
        &filename,
        expires,
        signature,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(()) => {}
        Err(design_link::LinkError::Invalid) => return Err(not_found()),
        Err(design_link::LinkError::Expired) => {
            return Err(AppError(PdmError::Forbidden(
                "design link has expired; open the design again".into(),
            )));
        }
    }
    let (expected, sha) = design_link_file(&state, version_id).await?;
    if filename != expected {
        return Err(not_found());
    }
    let data = state
        .store
        .get_by_sha(&sha)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    let response = axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/octet-stream")
        .header("content-length", data.len().to_string())
        .header("content-digest", format!("sha-256={sha}"))
        .header("cache-control", "no-store")
        .header(
            "content-disposition",
            format!("attachment; filename=\"{expected}\""),
        )
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
    Ok(response)
}

pub async fn regenerate_preview(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_role(&authed, "editor")?;
    let source = owned_version_source(&state, &authed, version_id).await?;
    if pdm_core::preview_plan(&source.filename) == pdm_core::PreviewPlan::Skip {
        return Err(AppError(PdmError::Validation(
            "this file type has no server-rendered preview".into(),
        )));
    }
    if !std::env::var("PDM_SOLVECRAFT_CLI").is_ok_and(|p| !p.trim().is_empty()) {
        return Err(AppError(PdmError::BadRequest(
            "SolidCraft CLI is not configured".into(),
        )));
    }
    let job = pdm_db::queue_preview_render(&state.pool, authed.org_id, version_id, &source).await?;
    pdm_db::record_audit(
        &state.pool,
        authed.org_id,
        Some(user.id),
        "",
        "version.regenerate_preview",
        "version",
        Some(version_id),
        None,
    )
    .await?;
    Ok(Json(serde_json::json!({"job":job})))
}

pub async fn get_job_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let job = pdm_db::get_job(&state.pool, id, authed.org_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("job".into())))?;
    Ok(Json(serde_json::json!({"job":job})))
}

/// Render a PNG thumbnail response from a version's recorded thumbnail blob.
async fn version_thumbnail_response(
    state: &AppState,
    version: &pdm_core::FileVersion,
) -> Result<Response, AppError> {
    let Some(sha) = version.thumbnail_sha256.clone() else {
        let why = version
            .thumbnail_error
            .as_deref()
            .map(|e| format!(" (the preview job recorded: {e})"))
            .unwrap_or_default();
        return Err(AppError(PdmError::NotFound(format!(
            "version {} has no thumbnail{why}",
            version.id
        ))));
    };
    let data = state
        .store
        .get_by_sha(&sha)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    let digest = format!("sha-256={sha}");
    let response = axum::http::Response::builder()
        .status(200)
        .header("content-type", "image/png")
        .header("content-length", data.len().to_string())
        .header("content-digest", digest)
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(format!("response build: {e}"))))?;
    Ok(response)
}

/// GET /versions/{version_id}/thumbnail — the rendered PNG preview of a version.
pub async fn download_version_thumbnail(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
) -> Result<Response, AppError> {
    owned_version_source(&state, &authed, version_id).await?;
    let version = pdm_db::get_version(&state.pool, version_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("version {version_id}"))))?;
    version_thumbnail_response(&state, &version).await
}

/// GET /items/{item_id}/thumbnail — the rendered PNG preview of the head version.
pub async fn download_item_thumbnail(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Response, AppError> {
    owned_item(&state, require_org(&authed), item_id).await?;
    let file = pdm_db::get_file(&state.pool, item_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("file not found".into())))?;
    let head = pdm_db::get_head_version(&state.pool, file.id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("no versions".into())))?;
    version_thumbnail_response(&state, &head).await
}

/// PUT /versions/{version_id}/thumbnail — store a client-rendered preview image for a version.
///
/// solidCraft renders a PNG preview of the design itself and uploads it here, so a thumbnail
/// exists even when the server's own render worker (`solvecraft-cli snapshot`) is unavailable —
/// the PDM-only dev image runs without the CLI. The stored image is served back by
/// `GET /versions/{version_id}/thumbnail` and, for the head version, `GET /items/{item_id}/thumbnail`.
pub async fn upload_version_thumbnail(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(version_id): Path<Uuid>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    if body.is_empty() {
        return Err(AppError(PdmError::Validation("empty image".into())));
    }
    if body.len() as u64 > state.max_upload {
        return Err(AppError(PdmError::Validation(format!(
            "thumbnail size {} exceeds limit {}",
            body.len(),
            state.max_upload
        ))));
    }
    // The version must be one of the organization's.
    owned_version_source(&state, &authed, version_id).await?;

    let (sha, size) = state
        .store
        .put_content_addressed(&body)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    let size_i64 =
        i64::try_from(size).map_err(|_| PdmError::Validation("image too large".into()))?;
    let storage_key = sha.clone();
    pdm_db::upload_blob(&state.pool, org, &sha, size_i64, &storage_key)
        .await
        .map_err(AppError)?;
    pdm_db::store_thumbnail(&state.pool, version_id, &sha, size_i64)
        .await
        .map_err(AppError)?;

    Ok(serde_json::json!({
        "ok": true,
        "version_id": version_id,
        "thumbnail_sha256": sha,
        "thumbnail_size_bytes": size_i64
    })
    .into())
}

// ── Item metadata update ────────────────────────────────────────────────

/// PUT /items/{item_id} — update mutable metadata (description, folder, owner).
///
/// Editors and admins only: the route had no role check, which went unnoticed while its SQL
/// failed every change; once it works, viewers must not move or reassign items.
pub async fn update_item(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
    Json(body): Json<UpdateItemReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let desc = body.description.as_deref();
    let item = pdm_db::update_item(
        &state.pool,
        item_id,
        org,
        desc,
        body.folder_id,
        body.owner_id,
    )
    .await?;

    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.updated",
        "item",
        Some(item_id),
        Some(serde_json::json!({
            "description": desc,
            "folder_id": body.folder_id,
            "owner_id": body.owner_id,
        })),
    )
    .await?;

    Ok(serde_json::json!({ "item": item }).into())
}

// ── Comments ────────────────────────────────────────────────────────────

/// GET /items/{item_id}/comments
pub async fn list_comments(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    owned_item(&state, require_org(&authed), item_id).await?;
    let comments = pdm_db::list_comments(&state.pool, item_id).await?;
    Ok(serde_json::json!({ "comments": comments }).into())
}

/// POST /items/{item_id}/comments
pub async fn add_comment(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
    Json(body): Json<CommentReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_user(&authed)?.clone();
    let text = body.body.trim();
    if text.is_empty() {
        return Err(AppError(PdmError::Validation("empty comment".into())));
    }
    let comment = pdm_db::add_comment(&state.pool, org, item_id, user.id, text).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.comment",
        "item",
        Some(item_id),
        Some(serde_json::json!({ "body": text })),
    )
    .await?;
    Ok(serde_json::json!({ "comment": comment }).into())
}

// ── Revisions ───────────────────────────────────────────────────────────

/// GET /items/{item_id}/revisions — released revision labels for an item.
pub async fn list_revisions(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    owned_item(&state, require_org(&authed), item_id).await?;
    let revisions = pdm_db::list_revisions(&state.pool, item_id).await?;
    Ok(serde_json::json!({ "revisions": revisions }).into())
}

// ── Versions ────────────────────────────────────────────────────────────

/// GET /items/{item_id}/versions
pub async fn list_item_versions(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    owned_item(&state, require_org(&authed), item_id).await?;
    let file = pdm_db::get_file(&state.pool, item_id).await?;
    let versions = match file {
        Some(f) => pdm_db::list_version_details(&state.pool, f.id).await?,
        None => vec![],
    };
    Ok(serde_json::json!({ "versions": versions }).into())
}

// ── Search ──────────────────────────────────────────────────────────────

/// GET /search?q=... — full-text + server-side filters + pagination.
pub async fn search(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let (page, limit) = pdm_db::pagination(q.page, q.limit);
    let p = pdm_db::search_items_paginated(
        &state.pool,
        org,
        q.project,
        &q.q,
        include_archived_flag(q.include_archived),
        q.classification.as_deref(),
        q.state.as_deref(),
        q.kind.as_deref(),
        q.owner,
        q.folder,
        q.sheet_metal,
        q.sort.as_deref(),
        q.dir.as_deref(),
        page,
        limit,
    )
    .await?;
    Ok(serde_json::json!({
        "results": p.items,
        "page": p.page,
        "limit": p.limit,
        "total": p.total,
        "has_more": p.has_more,
    })
    .into())
}

// ── Audit ───────────────────────────────────────────────────────────────

/// GET /audit?item=...&limit=...
#[derive(Deserialize)]
pub struct AuditQuery {
    pub item: Option<Uuid>,
    pub limit: Option<u32>,
}

pub async fn audit(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let limit = q.limit.unwrap_or(50).min(200);
    let events = pdm_db::list_audit(&state.pool, org, q.item, limit).await?;
    Ok(serde_json::json!({ "events": events }).into())
}

/// GET /home — dashboard aggregate: KPIs + recent audit + my approvals +
/// active jobs in one round trip.
pub async fn home(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_user(&authed)?.clone();
    let data = pdm_db::home_dashboard(&state.pool, org, user.id).await?;
    Ok(serde_json::json!(data).into())
}

// ── Properties ──────────────────────────────────────────────────────────

/// GET /items/{item_id}/properties
pub async fn get_properties(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    owned_item(&state, require_org(&authed), item_id).await?;
    let props = pdm_db::list_properties(&state.pool, item_id).await?;
    Ok(serde_json::json!({ "properties": props }).into())
}

/// PUT /items/{item_id}/properties/{key}
pub async fn set_property(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path((item_id, key)): Path<(Uuid, String)>,
    Json(body): Json<SetPropertyReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    require_role(&authed, "editor")?;
    let value_type = body.value_type.unwrap_or_else(|| "text".to_string());
    let prop =
        pdm_db::upsert_property(&state.pool, org, item_id, &key, &value_type, &body.value).await?;
    Ok(serde_json::json!({ "property": prop }).into())
}

// ── Release ─────────────────────────────────────────────────────────────

/// POST /items/{item_id}/release
pub async fn release(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
    Json(body): Json<ReleaseReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    require_item_in_org(&state, org, item_id).await?;

    let file = pdm_db::get_file(&state.pool, item_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("file not found".into())))?;
    let head = pdm_db::get_head_version(&state.pool, file.id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("no versions".into())))?;

    let label = if let Some(l) = body.label {
        l
    } else {
        // Auto-generate: A, B, C…
        let existing = pdm_db::list_revisions(&state.pool, item_id)
            .await
            .map(|r| r.len())
            .unwrap_or(0);
        auto_revision_label(existing)
    };

    let note = body.note.unwrap_or_default();

    // Optional per-release approval gate (org setting "require_approval_on_release").
    // When gated, release() only creates a pending approval request (item -> in_review);
    // the revision + WIP -> Released flip happen on `POST /approvals/{id}/decide` approve.
    let gated = pdm_db::org_setting_bool(&state.pool, org, "require_approval_on_release").await?;
    if gated {
        let assignees = body.assignee_ids.clone().unwrap_or_default();
        let assignees = if assignees.is_empty() {
            pdm_db::list_users(&state.pool, org)
                .await?
                .into_iter()
                .filter(|u| matches!(u.role.as_str(), "admin" | "editor"))
                .map(|u| u.id)
                .collect()
        } else {
            assignees
        };
        let request =
            pdm_db::create_approval_request(&state.pool, org, item_id, Some(user.id), &assignees)
                .await?;
        let _ = pdm_db::set_item_lifecycle_state(&state.pool, org, item_id, "in_review").await?;
        pdm_db::record_audit(
            &state.pool,
            org,
            Some(user.id),
            "",
            "item.release_requested",
            "item",
            Some(item_id),
            Some(serde_json::json!({ "assignees": assignees })),
        )
        .await?;
        pdm_db::enqueue_outbox(
            &state.pool,
            org,
            "approval.requested",
            &serde_json::json!({
                "request_id": request.id,
                "item_id": item_id,
                "requested_by": user.id,
                "assignees": assignees,
            }),
        )
        .await?;
        return Ok(serde_json::json!({
            "status": "pending_approval",
            "approval_request": request,
        })
        .into());
    }

    let revision = pdm_db::release_revision(
        &state.pool,
        org,
        item_id,
        &label,
        head.id,
        Some(user.id),
        &note,
        "alpha",
    )
    .await?;

    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.release",
        "item",
        Some(item_id),
        Some(serde_json::json!({ "revision": revision.label, "version_seq": head.seq })),
    )
    .await?;
    pdm_db::enqueue_outbox(
        &state.pool,
        org,
        "item.released",
        &serde_json::json!({
            "item_id": item_id,
            "revision": revision.label,
            "version_seq": head.seq,
            "user_id": user.id,
        }),
    )
    .await?;

    Ok(serde_json::json!({ "revision": revision }).into())
}

// ── Auth (login / logout) ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
    pub org: Option<String>,
}

/// POST /auth/login — username/password login; issues a fresh web sign-in token, which ends
/// after an idle spell or at its lifetime (see `SessionLimits`).
pub async fn login(
    State(state): State<AppState>,
    Json(body): Json<LoginReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let bad_credentials = AppError(PdmError::BadCredentials);
    let mut users = pdm_db::get_users_by_email(&state.pool, &body.username).await?;
    if users.is_empty() {
        return Err(bad_credentials);
    }
    if let Some(org_name) = &body.org {
        let org = match pdm_db::get_org_by_name(&state.pool, org_name).await? {
            Some(o) => o,
            None => return Err(bad_credentials),
        };
        users.retain(|u| u.org_id == org.id);
        if users.is_empty() {
            return Err(bad_credentials);
        }
    } else if users.len() > 1 {
        return Err(AppError(PdmError::BadRequest(format!(
            "email '{}' exists in {} organizations; pass \"org\" to pick one",
            body.username,
            users.len()
        ))));
    }
    let user = users.remove(0);
    if !user.active {
        return Err(bad_credentials);
    }
    let stored_hash = match &user.password_hash {
        Some(h) if !h.is_empty() => h,
        _ => return Err(bad_credentials),
    };
    let verified = pdm_db::verify_password(&body.password, stored_hash)?;
    if !verified {
        return Err(bad_credentials);
    }

    let token_value = format!("pdm_{}", Uuid::new_v4());
    let token_hash = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(token_value.as_bytes());
        hex::encode(h.finalize())
    };
    // A web sign-in, which ends after an idle spell or at its lifetime (unlike API tokens).
    let _tok = pdm_db::create_session_token(&state.pool, user.org_id, user.id, &token_hash).await?;
    let org = pdm_db::get_org_by_id(&state.pool, user.org_id)
        .await?
        .ok_or_else(|| AppError(PdmError::Internal("org not found".into())))?;

    pdm_db::record_audit(
        &state.pool,
        user.org_id,
        Some(user.id),
        "",
        "user.login",
        "user",
        Some(user.id),
        None,
    )
    .await?;

    Ok(serde_json::json!({
        "token": token_value,
        "user": user,
        "org": org
    })
    .into())
}

/// POST /auth/logout — delete the caller's token. Idempotent.
pub async fn logout(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    pdm_db::delete_token(&state.pool, authed.token_id).await?;
    Ok(serde_json::json!({ "ok": true }).into())
}

// ── Users / Admin ───────────────────────────────────────────────────────

const ROLE_LEVEL: &[(&str, u8)] = &[("viewer", 0), ("editor", 1), ("admin", 2)];

fn role_level(role: &str) -> u8 {
    ROLE_LEVEL
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, l)| *l)
        .unwrap_or(0)
}

fn require_role<'a>(authed: &'a Authed, min_role: &str) -> Result<&'a pdm_core::User, AppError> {
    let user = require_user(authed)?;
    let min = role_level(min_role);
    if role_level(&user.role) < min {
        return Err(AppError(PdmError::Forbidden(format!(
            "requires role '{}' but user has '{}'",
            min_role, user.role
        ))));
    }
    Ok(user)
}

#[derive(Deserialize)]
pub struct CreateUserReq {
    pub email: String,
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub password: Option<String>,
}

/// POST /users — admin creates a user in the current org (optional password).
pub async fn create_user_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateUserReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let admin = require_role(&authed, "admin")?;
    let org = require_org(&authed);
    let role = body.role.as_deref().unwrap_or("editor");
    let display = body.display_name.as_deref().unwrap_or("");
    let hash = match body.password.as_deref() {
        Some(pw) if pw.len() < 4 => {
            return Err(AppError(PdmError::Validation("password too short".into())));
        }
        Some(pw) => Some(pdm_db::hash_password(pw)?),
        None => None,
    };
    let user = pdm_db::create_user(
        &state.pool,
        org,
        &body.email,
        display,
        role,
        hash.as_deref(),
    )
    .await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(admin.id),
        "",
        "user.created",
        "user",
        Some(user.id),
        Some(serde_json::json!({ "email": user.email, "role": role })),
    )
    .await?;
    Ok(serde_json::json!({ "user": user }).into())
}

/// POST /users/invite — create a user and email them a temporary password.
pub async fn invite_user_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateUserReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let admin = require_role(&authed, "admin")?;
    let org = require_org(&authed);
    let raw = pdm_db::get_mail_settings(&state.pool, org).await?;
    let mail = crate::mail::MailSettings::from_json(&raw);
    if !mail.configured() {
        return Err(AppError(PdmError::BadRequest(
            "mail server is not configured — set it in Settings".into(),
        )));
    }
    let role = body.role.as_deref().unwrap_or("editor");
    let display = body.display_name.as_deref().unwrap_or("");
    let password = crate::mail::random_password();
    let hash = pdm_db::hash_password(&password)?;
    let user =
        pdm_db::create_user(&state.pool, org, &body.email, display, role, Some(&hash)).await?;
    let org_name = pdm_db::get_org_by_id(&state.pool, org)
        .await?
        .map(|o| o.name)
        .unwrap_or_else(|| "your organization".into());
    let body_text = crate::mail::invite_body(&mail, &user.email, &password, &org_name);
    if let Err(e) = crate::mail::send_mail(
        &mail,
        &user.email,
        &format!("Invitation to {org_name} on Foundry PDM"),
        &body_text,
    )
    .await
    {
        return Err(AppError(PdmError::Internal(format!(
            "user created but the invite email failed: {e}"
        ))));
    }
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(admin.id),
        "",
        "user.invited",
        "user",
        Some(user.id),
        Some(serde_json::json!({ "email": user.email, "role": role })),
    )
    .await?;
    Ok(serde_json::json!({ "user": user, "emailed": true }).into())
}

#[derive(Deserialize)]
pub struct CreateTokenReq {
    pub name: Option<String>,
}

/// GET /users
pub async fn list_users_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let users = pdm_db::list_users(&state.pool, org).await?;
    Ok(serde_json::json!({ "users": users }).into())
}

/// POST /users/{user_id}/password — set a user's password (admin).
pub async fn set_user_password_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(user_id): Path<Uuid>,
    Json(body): Json<SetPasswordReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let admin = require_role(&authed, "admin")?;
    let org = require_org(&authed);
    let target = pdm_db::get_user_by_id(&state.pool, user_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("user {user_id}"))))?;
    if target.org_id != org {
        return Err(AppError(PdmError::Forbidden("user not in this org".into())));
    }
    if body.password.len() < 4 {
        return Err(AppError(PdmError::Validation("password too short".into())));
    }
    let hash = pdm_db::hash_password(&body.password)?;
    pdm_db::set_user_password(&state.pool, user_id, &hash).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(admin.id),
        "",
        "user.password_set",
        "user",
        Some(user_id),
        None,
    )
    .await?;
    Ok(serde_json::json!({ "ok": true }).into())
}

#[derive(Deserialize)]
pub struct UpdateUserReq {
    pub display_name: Option<String>,
    pub role: Option<String>,
    pub active: Option<bool>,
}

/// PUT /users/{user_id} — admin edits a user's name, role, and active state.
pub async fn update_user_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UpdateUserReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let admin = require_role(&authed, "admin")?;
    let org = require_org(&authed);
    let target = pdm_db::get_user_by_id(&state.pool, user_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("user {user_id}"))))?;
    if target.org_id != org {
        return Err(AppError(PdmError::Forbidden("user not in this org".into())));
    }
    let role = body.role.as_deref().unwrap_or(&target.role);
    if !matches!(role, "viewer" | "editor" | "admin") {
        return Err(AppError(PdmError::Validation(format!(
            "invalid role '{role}'"
        ))));
    }
    let display = body.display_name.as_deref().unwrap_or(&target.display_name);
    let active = body.active.unwrap_or(target.active);
    let user = pdm_db::update_user(&state.pool, org, user_id, display, role, active).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(admin.id),
        "",
        "user.updated",
        "user",
        Some(user_id),
        Some(serde_json::json!({ "display_name": display, "role": role, "active": active })),
    )
    .await?;
    Ok(serde_json::json!({ "user": user }).into())
}

/// GET /jobs?limit=... — recent background jobs (dashboard processing widget).
pub async fn list_jobs_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<JobsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let limit = q.limit.unwrap_or(20).min(100) as i32;
    let jobs = pdm_db::list_jobs(&state.pool, org, limit).await?;
    Ok(serde_json::json!({ "jobs": jobs }).into())
}

#[derive(Deserialize)]
pub struct JobsQuery {
    pub limit: Option<u32>,
}

/// GET /approvals?status=pending&mine=true — the approval inbox, paginated.
pub async fn list_approvals_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<ApprovalListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user_id = if q.mine.unwrap_or(false) {
        Some(require_user(&authed)?.id)
    } else {
        None
    };
    let (page, limit) = pdm_db::pagination(q.page, q.limit);
    let p =
        pdm_db::list_approval_requests(&state.pool, org, q.status.as_deref(), user_id, page, limit)
            .await?;
    Ok(serde_json::json!({
        "approvals": p.items,
        "page": p.page,
        "limit": p.limit,
        "total": p.total,
        "has_more": p.has_more,
    })
    .into())
}

/// POST /approvals — create an approval request for an item, routed to
/// assignees (default: the org's editors/admins when none are named).
pub async fn create_approval_request_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<ApprovalCreateReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_role(&authed, "editor")?.clone();
    let item = pdm_db::get_item(&state.pool, body.item_id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("item {}", body.item_id))))?;
    let assignees = body.assignee_ids.unwrap_or_default();
    let request =
        pdm_db::create_approval_request(&state.pool, org, body.item_id, Some(user.id), &assignees)
            .await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "approval.requested",
        "approval_request",
        Some(request.id),
        Some(serde_json::json!({
            "item_id": body.item_id,
            "part_number": item.part_number,
            "assignees": assignees,
            "note": body.note,
        })),
    )
    .await?;
    pdm_db::enqueue_outbox(
        &state.pool,
        org,
        "approval.requested",
        &serde_json::json!({
            "request_id": request.id,
            "item_id": body.item_id,
            "requested_by": user.id,
            "assignees": assignees,
        }),
    )
    .await?;
    Ok(serde_json::json!({ "approval_request": request }).into())
}

/// POST /approvals/{id}/decide — approve or reject a pending request (with
/// optional note). On approve: release the item (WIP -> Released) as a new
/// revision. Emits audit + outbox events.
pub async fn decide_approval_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(request_id): Path<Uuid>,
    Json(body): Json<ApprovalDecideReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let user = require_user(&authed)?.clone();
    let is_lead = matches!(user.role.as_str(), "admin" | "editor");

    let note = body.note.clone();
    let decision = body.decision.trim();
    if decision != "approve" && decision != "reject" {
        return Err(AppError(PdmError::Validation(
            "decision must be 'approve' or 'reject'".into(),
        )));
    }

    let request = pdm_db::decide_approval_request(
        &state.pool,
        org,
        request_id,
        user.id,
        is_lead,
        decision,
        note.as_deref().unwrap_or(""),
    )
    .await?;

    let mut revision: Option<serde_json::Value> = None;
    let mut item: Option<serde_json::Value> = None;
    if let Some(item_id) = request.item_id {
        let owned = owned_item(&state, org, item_id).await?;
        item = Some(serde_json::json!(owned));
        if decision == "approve" {
            let file = pdm_db::get_file(&state.pool, item_id)
                .await?
                .ok_or_else(|| AppError(PdmError::NotFound("file not found".into())))?;
            let head = pdm_db::get_head_version(&state.pool, file.id)
                .await?
                .ok_or_else(|| AppError(PdmError::NotFound("no versions".into())))?;
            let existing = pdm_db::list_revisions(&state.pool, item_id).await?.len();
            let label = auto_revision_label(existing);
            let note = body.note.unwrap_or_default();
            let rev = pdm_db::release_revision(
                &state.pool,
                org,
                item_id,
                &label,
                head.id,
                Some(user.id),
                &note,
                "alpha",
            )
            .await?;
            revision = Some(serde_json::json!(rev));
            pdm_db::record_audit(
                &state.pool,
                org,
                Some(user.id),
                "",
                "item.release",
                "item",
                Some(item_id),
                Some(serde_json::json!({
                    "revision": rev.label,
                    "version_seq": head.seq,
                    "approval_request": request_id,
                })),
            )
            .await?;
            pdm_db::enqueue_outbox(
                &state.pool,
                org,
                "item.released",
                &serde_json::json!({
                    "item_id": item_id,
                    "revision": rev.label,
                    "version_seq": head.seq,
                    "user_id": user.id,
                    "approval_request": request_id,
                }),
            )
            .await?;
        } else {
            let _ = pdm_db::set_item_lifecycle_state(&state.pool, org, item_id, "wip").await?;
        }
    }

    let decision_action = format!("approval.{decision}");
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        &decision_action,
        "approval_request",
        Some(request_id),
        Some(serde_json::json!({
            "item_id": request.item_id,
            "decision": decision,
            "note": note,
        })),
    )
    .await?;
    pdm_db::enqueue_outbox(
        &state.pool,
        org,
        "approval.decided",
        &serde_json::json!({
            "request_id": request_id,
            "item_id": request.item_id,
            "decision": decision,
            "user_id": user.id,
        }),
    )
    .await?;

    Ok(serde_json::json!({
        "request": request,
        "decision": decision,
        "item": item,
        "revision": revision,
    })
    .into())
}

/// PATCH /items/{item_id}/lifecycle — flip lifecycle state after validating
/// the transition against the org's lifecycle definition. Editor+ only.
pub async fn transition_lifecycle_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
    Json(body): Json<LifecycleTransitionReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let _lead = require_role(&authed, "editor")?;
    let user = require_user(&authed)?.clone();

    let target = body.state.trim();
    if target.is_empty() {
        return Err(AppError(PdmError::Validation("state is required".into())));
    }

    let item = pdm_db::get_item(&state.pool, item_id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("item {item_id}"))))?;
    let from = item.lifecycle_state.as_str();
    if from == target {
        return Err(AppError(PdmError::Conflict(format!(
            "item is already in state '{target}'"
        ))));
    }

    let definitions = pdm_db::list_lifecycle_definitions(&state.pool, org).await?;
    if definitions.is_empty() {
        return Err(AppError(PdmError::BadRequest(
            "org has no lifecycle definition configured".into(),
        )));
    }

    if !pdm_db::lifecycle_transition_exists(&state.pool, org, from, target).await? {
        return Err(AppError(PdmError::BadRequest(format!(
            "lifecycle transition '{from}' -> '{target}' is not permitted by the definition"
        ))));
    }

    let item = pdm_db::set_item_lifecycle_state(&state.pool, org, item_id, target).await?;

    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.lifecycle_transitioned",
        "item",
        Some(item_id),
        Some(serde_json::json!({ "from": from, "to": target })),
    )
    .await?;
    pdm_db::enqueue_outbox(
        &state.pool,
        org,
        "item.lifecycle_transitioned",
        &serde_json::json!({
            "item_id": item_id,
            "from": from,
            "to": target,
            "user_id": user.id,
        }),
    )
    .await?;

    Ok(serde_json::json!({ "item": item }).into())
}

/// POST /users/{user_id}/tokens — admin creates an API token for a user.
pub async fn create_token_handler(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(user_id): Path<Uuid>,
    Json(body): Json<CreateTokenReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let admin = require_role(&authed, "admin")?;
    let org = require_org(&authed);
    let target = pdm_db::get_user_by_id(&state.pool, user_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("user {user_id}"))))?;
    if target.org_id != org {
        return Err(AppError(PdmError::Forbidden("user not in this org".into())));
    }
    let token_value = format!("pdm_{}", Uuid::new_v4());
    let hash = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(token_value.as_bytes());
        hex::encode(h.finalize())
    };
    let name = body.name.as_deref().unwrap_or("api-token");
    let _tok =
        pdm_db::create_token(&state.pool, org, Some(user_id), name, &hash, "full", None).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(admin.id),
        "",
        "user.token_created",
        "user",
        Some(user_id),
        Some(serde_json::json!({ "token_name": name })),
    )
    .await?;
    Ok(serde_json::json!({ "token": token_value, "user_id": user_id }).into())
}

// ── Manufacturing registry (Machines & Tools) ───────────────────────────

#[derive(Deserialize)]
pub struct CreateMachineReq {
    pub name: String,
    pub kind: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub controller: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    pub work_envelope: Option<serde_json::Value>,
    pub axes: Option<Vec<String>>,
    pub materials: Option<Vec<String>>,
    pub maintenance_status: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateToolReq {
    pub name: String,
    pub kind: Option<String>,
    pub description: Option<String>,
    pub vendor: Option<String>,
    pub part_number: Option<String>,
    pub link: Option<String>,
    pub units: Option<String>,
    pub rotation: Option<String>,
    pub flutes: Option<i32>,
    pub tool_material: Option<String>,
    pub tool_number: Option<i32>,
    pub dimensions: Option<serde_json::Value>,
    pub cutting_params: Option<serde_json::Value>,
    pub material_compatibility: Option<Vec<String>>,
    pub inventory: Option<i32>,
    pub tool_life: Option<serde_json::Value>,
}

/// `PUT /tools/{id}` — every field optional; absent fields keep their value.
#[derive(Deserialize)]
pub struct UpdateToolReq {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub description: Option<String>,
    pub vendor: Option<String>,
    pub part_number: Option<String>,
    pub link: Option<String>,
    pub units: Option<String>,
    pub rotation: Option<String>,
    pub flutes: Option<i32>,
    pub tool_material: Option<String>,
    pub tool_number: Option<i32>,
    pub dimensions: Option<serde_json::Value>,
    pub cutting_params: Option<serde_json::Value>,
    pub material_compatibility: Option<Vec<String>>,
    pub inventory: Option<i32>,
    pub tool_life: Option<serde_json::Value>,
}

/// Speed + feedrate values for one cutting-data set (per material/finish).
#[derive(Deserialize)]
pub struct CuttingDataReq {
    pub label: Option<String>,
    pub material: Option<String>,
    pub finish: Option<String>,
    pub spindle_speed: Option<f64>,
    pub surface_speed: Option<f64>,
    pub ramp_spindle_speed: Option<f64>,
    pub cutting_feedrate: Option<f64>,
    pub feed_per_tooth: Option<f64>,
    pub lead_in_feedrate: Option<f64>,
    pub lead_out_feedrate: Option<f64>,
    pub transition_feedrate: Option<f64>,
    pub ramp_feedrate: Option<f64>,
    pub ramp_angle: Option<f64>,
    pub plunge_feedrate: Option<f64>,
    pub plunge_feed_per_rev: Option<f64>,
}

/// `POST /tools/{id}/documents` — the blob was already uploaded via PUT /blobs.
#[derive(Deserialize)]
pub struct AddToolDocumentReq {
    pub filename: String,
    pub blob_id: Uuid,
    pub content_type: Option<String>,
}

#[derive(Deserialize)]
pub struct CreatePostProcessorReq {
    pub name: String,
    pub version: Option<String>,
    pub dialect: Option<String>,
    pub machine_kind: Option<String>,
}

/// GET /machines — the org's machine registry.
pub async fn list_machines(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let machines = pdm_db::list_machines(&state.pool, org).await?;
    Ok(serde_json::json!({ "machines": machines }).into())
}

/// POST /machines — add a machine to the registry.
pub async fn create_machine(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateMachineReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let kind = body.kind.as_deref().unwrap_or("cnc_mill");
    let axes: &[String] = body.axes.as_deref().unwrap_or(&[]);
    let materials: &[String] = body.materials.as_deref().unwrap_or(&[]);
    let machine = pdm_db::create_machine(
        &state.pool,
        org,
        &body.name,
        kind,
        body.manufacturer.as_deref(),
        body.model.as_deref(),
        body.controller.as_deref(),
        body.capabilities.as_ref(),
        body.work_envelope.as_ref(),
        axes,
        materials,
        body.maintenance_status.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "machine": machine }).into())
}

/// GET /tools — the org's tool library.
pub async fn list_tools(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let tools = pdm_db::list_tools(&state.pool, org).await?;
    Ok(serde_json::json!({ "tools": tools }).into())
}

/// POST /tools — add a tool to the library.
pub async fn create_tool(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateToolReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let kind = body.kind.as_deref().unwrap_or("end_mill");
    let units = body.units.as_deref().unwrap_or("mm");
    let rotation = body.rotation.as_deref().unwrap_or("cw");
    for v in [units, rotation] {
        if !(v == "mm" || v == "in" || v == "cw" || v == "ccw") {
            return Err(AppError(PdmError::Validation(
                "units must be mm|in and rotation cw|ccw".into(),
            )));
        }
    }
    let mat: &[String] = body.material_compatibility.as_deref().unwrap_or(&[]);
    let tool = pdm_db::create_tool(
        &state.pool,
        org,
        &body.name,
        kind,
        body.description.as_deref(),
        body.vendor.as_deref(),
        body.part_number.as_deref(),
        body.link.as_deref(),
        units,
        rotation,
        body.flutes,
        body.tool_material.as_deref(),
        body.tool_number,
        body.dimensions.as_ref(),
        body.cutting_params.as_ref(),
        mat,
        body.inventory.unwrap_or(0),
        body.tool_life.as_ref(),
    )
    .await?;
    Ok(serde_json::json!({ "tool": tool }).into())
}

/// GET /tools/{id} — one tool (with its cutting-data sets and documents).
pub async fn get_tool(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let tool = pdm_db::get_tool(&state.pool, org, id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("tool".into())))?;
    let cutting_data = pdm_db::list_cutting_data(&state.pool, id).await?;
    let documents = pdm_db::list_tool_documents(&state.pool, id).await?;
    Ok(serde_json::json!({
        "tool": tool,
        "cutting_data": cutting_data,
        "documents": documents,
    })
    .into())
}

/// PUT /tools/{id} — update the tool. Absent fields keep their current value.
pub async fn update_tool(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateToolReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let cur = pdm_db::get_tool(&state.pool, org, id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("tool".into())))?;
    let units = body.units.as_deref().unwrap_or(&cur.units);
    let rotation = body.rotation.as_deref().unwrap_or(&cur.rotation);
    if !(units == "mm" || units == "in") {
        return Err(AppError(PdmError::Validation(
            "units must be mm or in".into(),
        )));
    }
    if !(rotation == "cw" || rotation == "ccw") {
        return Err(AppError(PdmError::Validation(
            "rotation must be cw or ccw".into(),
        )));
    }
    let name = body.name.as_deref().unwrap_or(&cur.name);
    let kind = body.kind.as_deref().unwrap_or(&cur.kind);
    let tool = pdm_db::update_tool(
        &state.pool,
        org,
        id,
        name,
        kind,
        body.description.as_deref().or(cur.description.as_deref()),
        body.vendor.as_deref().or(cur.vendor.as_deref()),
        body.part_number.as_deref().or(cur.part_number.as_deref()),
        body.link.as_deref().or(cur.link.as_deref()),
        units,
        rotation,
        body.flutes.or(cur.flutes),
        body.tool_material
            .as_deref()
            .or(cur.tool_material.as_deref()),
        body.tool_number.or(cur.tool_number),
        body.dimensions.as_ref().or(cur.dimensions.as_ref()),
        body.cutting_params.as_ref().or(cur.cutting_params.as_ref()),
        body.material_compatibility
            .as_ref()
            .unwrap_or(&cur.material_compatibility),
        body.inventory.unwrap_or(cur.inventory),
        body.tool_life.as_ref().or(cur.tool_life.as_ref()),
    )
    .await?;
    Ok(serde_json::json!({ "tool": tool }).into())
}

/// DELETE /tools/{id} — remove a tool (and its cutting data + documents).
pub async fn delete_tool(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    pdm_db::delete_tool(&state.pool, org, id).await?;
    Ok(serde_json::json!({ "ok": true }).into())
}

// ── Tool cutting data (speeds + feedrates per material/finish) ──────────

/// GET /tools/{id}/cutting-data
pub async fn list_tool_cutting_data(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    require_tool_exists(&state.pool, org, id).await?;
    let rows = pdm_db::list_cutting_data(&state.pool, id).await?;
    Ok(serde_json::json!({ "cutting_data": rows }).into())
}

/// POST /tools/{id}/cutting-data
pub async fn create_tool_cutting_data(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
    Json(body): Json<CuttingDataReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    require_tool_exists(&state.pool, org, id).await?;
    let row = pdm_db::create_cutting_data(
        &state.pool,
        id,
        body.label.as_deref(),
        body.material.as_deref(),
        body.finish.as_deref(),
        body.spindle_speed,
        body.surface_speed,
        body.ramp_spindle_speed,
        body.cutting_feedrate,
        body.feed_per_tooth,
        body.lead_in_feedrate,
        body.lead_out_feedrate,
        body.transition_feedrate,
        body.ramp_feedrate,
        body.ramp_angle,
        body.plunge_feedrate,
        body.plunge_feed_per_rev,
    )
    .await?;
    Ok(serde_json::json!({ "cutting_data": row }).into())
}

/// PUT /tools/{id}/cutting-data/{cid}
pub async fn update_tool_cutting_data(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path((id, cid)): Path<(Uuid, Uuid)>,
    Json(body): Json<CuttingDataReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    require_tool_exists(&state.pool, org, id).await?;
    let row = pdm_db::update_cutting_data(
        &state.pool,
        id,
        cid,
        body.label.as_deref(),
        body.material.as_deref(),
        body.finish.as_deref(),
        body.spindle_speed,
        body.surface_speed,
        body.ramp_spindle_speed,
        body.cutting_feedrate,
        body.feed_per_tooth,
        body.lead_in_feedrate,
        body.lead_out_feedrate,
        body.transition_feedrate,
        body.ramp_feedrate,
        body.ramp_angle,
        body.plunge_feedrate,
        body.plunge_feed_per_rev,
    )
    .await?;
    Ok(serde_json::json!({ "cutting_data": row }).into())
}

/// DELETE /tools/{id}/cutting-data/{cid}
pub async fn delete_tool_cutting_data(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path((id, cid)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    require_tool_exists(&state.pool, org, id).await?;
    pdm_db::delete_cutting_data(&state.pool, id, cid).await?;
    Ok(serde_json::json!({ "ok": true }).into())
}

// ── Tool documents ──────────────────────────────────────────────────────

async fn require_tool_exists(pool: &sqlx::PgPool, org: Uuid, id: Uuid) -> Result<(), AppError> {
    pdm_db::get_tool(pool, org, id)
        .await?
        .map(|_| ())
        .ok_or_else(|| AppError(PdmError::NotFound("tool".into())))
}

/// GET /tools/{id}/documents
pub async fn list_tool_documents(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    require_tool_exists(&state.pool, org, id).await?;
    let docs = pdm_db::list_tool_documents(&state.pool, id).await?;
    Ok(serde_json::json!({ "documents": docs }).into())
}

/// POST /tools/{id}/documents — attach an already-uploaded blob to the tool.
pub async fn add_tool_document(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
    Json(body): Json<AddToolDocumentReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    require_tool_exists(&state.pool, org, id).await?;
    let blob = pdm_db::get_blob_by_id(&state.pool, body.blob_id)
        .await?
        .filter(|b| b.org_id == org)
        .ok_or_else(|| AppError(PdmError::NotFound("blob".into())))?;
    let doc = pdm_db::create_tool_document(
        &state.pool,
        id,
        &body.filename,
        blob.id,
        blob.size_bytes,
        body.content_type.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "document": doc }).into())
}

/// GET /tools/{id}/documents/{doc_id}/download — stream the attached blob.
pub async fn download_tool_document(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path((id, doc_id)): Path<(Uuid, Uuid)>,
) -> Result<Response, AppError> {
    let org = require_org(&authed);
    require_tool_exists(&state.pool, org, id).await?;
    let doc = pdm_db::get_tool_document(&state.pool, id, doc_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("document".into())))?;
    let blob = pdm_db::get_blob_by_id(&state.pool, doc.blob_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("blob for document".into())))?;
    let data = state
        .store
        .get_by_sha(&blob.sha256)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    let filename = doc.filename.replace('"', "");
    let response = axum::http::Response::builder()
        .status(200)
        .header(
            "content-type",
            doc.content_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".into()),
        )
        .header("content-length", data.len().to_string())
        .header(
            "content-disposition",
            format!("attachment; filename=\"{filename}\""),
        )
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(format!("response build: {e}"))))?;
    Ok(response)
}

/// DELETE /tools/{id}/documents/{doc_id}
pub async fn delete_tool_document(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path((id, doc_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    require_tool_exists(&state.pool, org, id).await?;
    pdm_db::delete_tool_document(&state.pool, id, doc_id).await?;
    Ok(serde_json::json!({ "ok": true }).into())
}

/// GET /post-processors — the org's post-processors.
pub async fn list_post_processors(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let pps = pdm_db::list_post_processors(&state.pool, org).await?;
    Ok(serde_json::json!({ "post_processors": pps }).into())
}

/// POST /post-processors — add a post-processor.
pub async fn create_post_processor(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreatePostProcessorReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let pp = pdm_db::create_post_processor(
        &state.pool,
        org,
        &body.name,
        body.version.as_deref(),
        body.dialect.as_deref(),
        body.machine_kind.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "post_processor": pp }).into())
}

// ── Manufacturing execution chain (setup → CAM → NC → job → inspection) ──

#[derive(Deserialize)]
pub struct CreateMfgSetupReq {
    pub item_id: Uuid,
    pub file_version_id: Option<Uuid>,
    pub name: String,
    pub stock: Option<serde_json::Value>,
    pub workholding: Option<String>,
    pub wcs: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateCamProjectReq {
    pub mfg_setup_id: Uuid,
    pub name: String,
    pub source: Option<String>,
    pub tool_ids: Option<Vec<Uuid>>,
}

#[derive(Deserialize)]
pub struct CreateNcProgramReq {
    pub item_id: Uuid,
    pub file_version_id: Option<Uuid>,
    pub mfg_setup_id: Option<Uuid>,
    pub cam_project_id: Option<Uuid>,
    pub machine_id: Option<Uuid>,
    pub post_processor_id: Option<Uuid>,
    pub post_processor_version: Option<String>,
    pub gcode_sha256: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateProductionJobReq {
    pub nc_program_id: Uuid,
    pub machine_id: Option<Uuid>,
    pub operator: Option<String>,
}

#[derive(Deserialize)]
pub struct SetJobStatusReq {
    pub status: String,
}

#[derive(Deserialize)]
pub struct CreateInspectionReq {
    pub production_job_id: Option<Uuid>,
    pub item_id: Option<Uuid>,
    pub passed: Option<bool>,
    pub notes: Option<String>,
    pub measured: Option<serde_json::Value>,
}

/// GET /mfg-setups
pub async fn list_mfg_setups(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    Ok(
        serde_json::json!({ "mfg_setups": pdm_db::list_mfg_setups(&state.pool, org).await? })
            .into(),
    )
}

/// POST /mfg-setups
pub async fn create_mfg_setup(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateMfgSetupReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let s = pdm_db::create_mfg_setup(
        &state.pool,
        org,
        body.item_id,
        body.file_version_id,
        &body.name,
        body.stock.as_ref(),
        body.workholding.as_deref(),
        body.wcs.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "mfg_setup": s }).into())
}

/// GET /cam-projects
pub async fn list_cam_projects(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    Ok(
        serde_json::json!({ "cam_projects": pdm_db::list_cam_projects(&state.pool, org).await? })
            .into(),
    )
}

/// POST /cam-projects
pub async fn create_cam_project(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateCamProjectReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let tools = body.tool_ids.clone().unwrap_or_default();
    let c = pdm_db::create_cam_project(
        &state.pool,
        org,
        body.mfg_setup_id,
        &body.name,
        body.source.as_deref(),
        &tools,
    )
    .await?;
    Ok(serde_json::json!({ "cam_project": c }).into())
}

/// GET /nc-programs
pub async fn list_nc_programs(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    Ok(
        serde_json::json!({ "nc_programs": pdm_db::list_nc_programs(&state.pool, org).await? })
            .into(),
    )
}

/// POST /nc-programs
pub async fn create_nc_program(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateNcProgramReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let n = pdm_db::create_nc_program(
        &state.pool,
        org,
        body.item_id,
        body.file_version_id,
        body.mfg_setup_id,
        body.cam_project_id,
        body.machine_id,
        body.post_processor_id,
        body.post_processor_version.as_deref(),
        body.gcode_sha256.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "nc_program": n }).into())
}

/// POST /nc-programs/{id}/release
pub async fn release_nc_program(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let n = pdm_db::release_nc_program(&state.pool, org, id).await?;
    Ok(serde_json::json!({ "nc_program": n }).into())
}

/// GET /production-jobs
pub async fn list_production_jobs(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    Ok(serde_json::json!({ "production_jobs": pdm_db::list_production_jobs(&state.pool, org).await? }).into())
}

/// POST /production-jobs
pub async fn create_production_job(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateProductionJobReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let j = pdm_db::create_production_job(
        &state.pool,
        org,
        body.nc_program_id,
        body.machine_id,
        body.operator.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "production_job": j }).into())
}

/// POST /production-jobs/{id}/status
pub async fn set_production_job_status(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
    Json(body): Json<SetJobStatusReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let j = pdm_db::set_production_job_status(&state.pool, org, id, &body.status).await?;
    Ok(serde_json::json!({ "production_job": j }).into())
}

/// GET /inspections
pub async fn list_inspections(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    Ok(
        serde_json::json!({ "inspections": pdm_db::list_inspections(&state.pool, org).await? })
            .into(),
    )
}

/// POST /inspections
pub async fn create_inspection(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateInspectionReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let i = pdm_db::create_inspection(
        &state.pool,
        org,
        body.production_job_id,
        body.item_id,
        body.passed,
        body.notes.as_deref(),
        body.measured.as_ref(),
    )
    .await?;
    Ok(serde_json::json!({ "inspection": i }).into())
}

/// GET /items/{item_id}/manufacturing — the item's MFG chain status (dashboard / robotics workflow).
pub async fn item_manufacturing(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let setups: Vec<_> = pdm_db::list_mfg_setups(&state.pool, org).await?;
    let cam_projects: Vec<_> = pdm_db::list_cam_projects(&state.pool, org).await?;
    let nc_programs: Vec<_> = pdm_db::list_nc_programs(&state.pool, org).await?;
    let jobs: Vec<_> = pdm_db::list_production_jobs(&state.pool, org).await?;
    let inspections: Vec<_> = pdm_db::list_inspections(&state.pool, org).await?;

    let item_setups: Vec<_> = setups
        .into_iter()
        .filter(|s| s.item_id == item_id)
        .collect();
    let item_setup_ids: std::collections::HashSet<Uuid> =
        item_setups.iter().map(|s| s.id).collect();
    let item_cam: Vec<_> = cam_projects
        .into_iter()
        .filter(|c| item_setup_ids.contains(&c.mfg_setup_id))
        .collect();
    let item_nc: Vec<_> = nc_programs
        .into_iter()
        .filter(|n| n.item_id == item_id)
        .collect();
    let item_nc_ids: std::collections::HashSet<Uuid> = item_nc.iter().map(|n| n.id).collect();
    let item_jobs: Vec<_> = jobs
        .into_iter()
        .filter(|j| item_nc_ids.contains(&j.nc_program_id))
        .collect();
    let item_job_ids: std::collections::HashSet<Uuid> = item_jobs.iter().map(|j| j.id).collect();
    let item_inspections: Vec<_> = inspections
        .into_iter()
        .filter(|i| {
            i.item_id == Some(item_id)
                || i.production_job_id
                    .is_some_and(|j| item_job_ids.contains(&j))
        })
        .collect();

    let in_machining = item_jobs.iter().any(|j| j.status == "in_progress");
    let done = item_jobs.iter().any(|j| j.status == "done");
    let released = item_nc.iter().any(|n| n.released);
    let status: String = if in_machining {
        "being_machined".into()
    } else if done {
        match item_inspections.iter().find(|i| i.passed.is_some()) {
            Some(i) if i.passed == Some(true) => "completed".into(),
            Some(_) => "rejected".into(),
            None => "awaiting_inspection".into(),
        }
    } else if released {
        "ready_for_machining".into()
    } else if !item_cam.is_empty() {
        "in_cam".into()
    } else if !item_setups.is_empty() {
        "needs_cad".into()
    } else {
        "none".into()
    };

    Ok(serde_json::json!({
        "item_id": item_id,
        "status": status,
        "mfg_setups": item_setups,
        "cam_projects": item_cam,
        "nc_programs": item_nc,
        "production_jobs": item_jobs,
        "inspections": item_inspections,
    })
    .into())
}

/// GET /manufacturing/dashboard — org-wide MFG workflow status (robotics kanban): each part that
/// has any manufacturing activity, bucketed by where it is in the chain.
pub async fn manufacturing_dashboard(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let setups: Vec<_> = pdm_db::list_mfg_setups(&state.pool, org).await?;
    let cam: Vec<_> = pdm_db::list_cam_projects(&state.pool, org).await?;
    let nc: Vec<_> = pdm_db::list_nc_programs(&state.pool, org).await?;
    let jobs: Vec<_> = pdm_db::list_production_jobs(&state.pool, org).await?;
    let inspections: Vec<_> = pdm_db::list_inspections(&state.pool, org).await?;

    let mut item_ids: std::collections::BTreeSet<Uuid> = std::collections::BTreeSet::new();
    for s in &setups {
        item_ids.insert(s.item_id);
    }
    for n in &nc {
        item_ids.insert(n.item_id);
    }

    let mut items: Vec<serde_json::Value> = Vec::new();
    for item_id in &item_ids {
        let i_setups: Vec<_> = setups.iter().filter(|s| &s.item_id == item_id).collect();
        let i_setup_ids: std::collections::HashSet<Uuid> = i_setups.iter().map(|s| s.id).collect();
        let i_cam: Vec<_> = cam
            .iter()
            .filter(|c| i_setup_ids.contains(&c.mfg_setup_id))
            .collect();
        let i_nc: Vec<_> = nc.iter().filter(|n| &n.item_id == item_id).collect();
        let i_nc_ids: std::collections::HashSet<Uuid> = i_nc.iter().map(|n| n.id).collect();
        let i_jobs: Vec<_> = jobs
            .iter()
            .filter(|j| i_nc_ids.contains(&j.nc_program_id))
            .collect();
        let i_job_ids: std::collections::HashSet<Uuid> = i_jobs.iter().map(|j| j.id).collect();
        let i_insp: Vec<_> = inspections
            .iter()
            .filter(|i| {
                i.item_id == Some(*item_id)
                    || i.production_job_id.is_some_and(|j| i_job_ids.contains(&j))
            })
            .collect();

        let in_machining = i_jobs.iter().any(|j| j.status == "in_progress");
        let done = i_jobs.iter().any(|j| j.status == "done");
        let released = i_nc.iter().any(|n| n.released);
        let status: String = if in_machining {
            "being_machined".into()
        } else if done {
            match i_insp.iter().find(|i| i.passed.is_some()) {
                Some(i) if i.passed == Some(true) => "completed".into(),
                Some(_) => "rejected".into(),
                None => "awaiting_inspection".into(),
            }
        } else if released {
            "ready_for_machining".into()
        } else if !i_cam.is_empty() {
            "in_cam".into()
        } else {
            "needs_cad".into()
        };
        items.push(serde_json::json!({
            "item_id": item_id,
            "status": status,
            "nc_programs": i_nc.len(),
            "production_jobs": i_jobs.len(),
        }));
    }

    let mut buckets: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for it in &items {
        if let Some(s) = it["status"].as_str() {
            *buckets.entry(s.to_string()).or_insert(0) += 1;
        }
    }
    Ok(serde_json::json!({ "items": items, "buckets": buckets }).into())
}

// ── 3D printing + sheet metal ───────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreatePrintJobReq {
    pub item_id: Uuid,
    pub file_version_id: Option<Uuid>,
    pub printer_id: Option<Uuid>,
    pub material: Option<String>,
    pub slicer_profile: Option<String>,
    pub orientation: Option<String>,
    pub supports: Option<bool>,
    pub layer_height_mm: Option<f64>,
    pub estimated_minutes: Option<i32>,
    pub machine_file_sha: Option<String>,
}

#[derive(Deserialize)]
pub struct SetPrintJobStatusReq {
    pub status: String,
}

#[derive(Deserialize)]
pub struct CreateSheetMetalReq {
    pub item_id: Uuid,
    pub file_version_id: Option<Uuid>,
    pub material: Option<String>,
    pub thickness_mm: Option<f64>,
    pub bend_radius_mm: Option<f64>,
    pub k_factor: Option<f64>,
    pub bend_table: Option<serde_json::Value>,
    pub flat_pattern_sha: Option<String>,
    pub cutting_operation: Option<String>,
    pub forming_operation: Option<String>,
}

/// GET /print-jobs
pub async fn list_print_jobs(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    Ok(
        serde_json::json!({ "print_jobs": pdm_db::list_print_jobs(&state.pool, org).await? })
            .into(),
    )
}

/// POST /print-jobs
pub async fn create_print_job(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreatePrintJobReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let p = pdm_db::create_print_job(
        &state.pool,
        org,
        body.item_id,
        body.file_version_id,
        body.printer_id,
        body.material.as_deref(),
        body.slicer_profile.as_deref(),
        body.orientation.as_deref(),
        body.supports.unwrap_or(false),
        body.layer_height_mm,
        body.estimated_minutes,
        body.machine_file_sha.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "print_job": p }).into())
}

/// POST /print-jobs/{id}/status
pub async fn set_print_job_status(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
    Json(body): Json<SetPrintJobStatusReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let p = pdm_db::set_print_job_status(&state.pool, org, id, &body.status).await?;
    Ok(serde_json::json!({ "print_job": p }).into())
}

/// GET /sheet-metal
pub async fn list_sheet_metal(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    Ok(
        serde_json::json!({ "sheet_metal": pdm_db::list_sheet_metal(&state.pool, org).await? })
            .into(),
    )
}

/// POST /sheet-metal
pub async fn create_sheet_metal(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateSheetMetalReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let s = pdm_db::create_sheet_metal(
        &state.pool,
        org,
        body.item_id,
        body.file_version_id,
        body.material.as_deref(),
        body.thickness_mm,
        body.bend_radius_mm,
        body.k_factor,
        body.bend_table.as_ref(),
        body.flat_pattern_sha.as_deref(),
        body.cutting_operation.as_deref(),
        body.forming_operation.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "sheet_metal_spec": s }).into())
}

// ── Per-item sheet metal: spec editing + flat-pattern DXF ───────────────

/// Marks a 3D model as a sheet-metal part (or edits its spec). The presence
/// of a spec row is what distinguishes sheet-metal parts from other items
/// in the listings and enables flat-pattern export.
#[derive(Deserialize)]
pub struct SaveItemSheetMetalReq {
    pub material: Option<String>,
    pub thickness_mm: Option<f64>,
    pub bend_radius_mm: Option<f64>,
    pub k_factor: Option<f64>,
}

async fn owned_item(
    state: &AppState,
    org: Uuid,
    item_id: Uuid,
) -> Result<pdm_core::Item, AppError> {
    pdm_db::get_item(&state.pool, item_id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("item {item_id}"))))
}

/// GET /items/{item_id}/sheet-metal — the item's sheet-metal spec, if any.
pub async fn item_sheet_metal(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    owned_item(&state, org, item_id).await?;
    let spec = pdm_db::get_item_sheet_metal(&state.pool, org, item_id).await?;
    Ok(serde_json::json!({ "sheet_metal_spec": spec, "item_id": item_id }).into())
}

/// POST /items/{item_id}/sheet-metal — create or update the spec.
pub async fn save_item_sheet_metal(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
    Json(body): Json<SaveItemSheetMetalReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_role(&authed, "editor")?;
    let org = require_org(&authed);
    owned_item(&state, org, item_id).await?;
    let s = pdm_db::upsert_item_sheet_metal(
        &state.pool,
        org,
        item_id,
        body.material.as_deref(),
        body.thickness_mm,
        body.bend_radius_mm,
        body.k_factor,
    )
    .await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.sheet_metal",
        "item",
        Some(item_id),
        None,
    )
    .await?;
    Ok(serde_json::json!({ "sheet_metal_spec": s }).into())
}

/// POST /items/{item_id}/flat-pattern — attach a generated flat-pattern DXF
/// (raw bytes). The blob is content-addressed like every other upload.
pub async fn upload_item_flat_pattern(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
    body: Bytes,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_role(&authed, "editor")?;
    let org = require_org(&authed);
    owned_item(&state, org, item_id).await?;
    if body.is_empty() {
        return Err(AppError(PdmError::Validation("empty upload".into())));
    }
    if body.len() as u64 > state.max_upload {
        return Err(AppError(PdmError::Validation(format!(
            "flat pattern exceeds the {} byte upload limit",
            state.max_upload
        ))));
    }
    let (sha, size) = state
        .store
        .put_content_addressed(&body)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    pdm_db::upload_blob(
        &state.pool,
        org,
        &sha,
        i64::try_from(size).map_err(|_| PdmError::Validation("file too large".into()))?,
        &sha,
    )
    .await?;
    let s = pdm_db::set_item_flat_pattern(&state.pool, org, item_id, &sha).await?;
    // Pin a revision-bound artifact to the item's head version so nests can
    // reference the exact CAD revision this uploaded DXF belongs to.
    if let Ok(Some(file)) = pdm_db::get_file(&state.pool, item_id).await
        && let Ok(Some(head)) = pdm_db::get_head_version(&state.pool, file.id).await
    {
        let _ = pdm_db::create_flat_pattern_artifact(
            &state.pool,
            org,
            item_id,
            head.id,
            &sha,
            i64::try_from(size).map_err(|_| PdmError::Validation("file too large".into()))?,
            "uploaded",
            "user upload",
            "unknown",
            "uploaded flat pattern",
        )
        .await;
    }
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.flat_pattern",
        "item",
        Some(item_id),
        None,
    )
    .await?;
    Ok(serde_json::json!({ "sheet_metal_spec": s }).into())
}

/// POST /items/{item_id}/flat-pattern/generate — queue a `sheet.flat-pattern`
/// job: run the engine's `sheet.export_dxf` on the head version's design and
/// attach the result to the item's sheet-metal spec.
pub async fn generate_item_flat_pattern(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_role(&authed, "editor")?;
    let org = require_org(&authed);
    owned_item(&state, org, item_id).await?;
    let file = pdm_db::get_file(&state.pool, item_id)
        .await?
        .ok_or_else(|| AppError(PdmError::Validation("item has no file yet".into())))?;
    let head = pdm_db::get_head_version(&state.pool, file.id)
        .await?
        .ok_or_else(|| {
            AppError(PdmError::Validation(
                "item has no checked-in version yet".into(),
            ))
        })?;
    let payload = serde_json::json!({
        "version_id": head.id,
        "item_id": item_id,
        "filename": file.filename,
    });
    let job = pdm_db::enqueue_job(&state.pool, org, "sheet.flat-pattern", &payload).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "item.flat_pattern_generate",
        "item",
        Some(item_id),
        None,
    )
    .await?;
    Ok(serde_json::json!({ "job": job, "item_id": item_id }).into())
}

/// GET /items/{item_id}/flat-pattern — download the item's flat-pattern DXF.
pub async fn download_item_flat_pattern(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let org = require_org(&authed);
    let item = owned_item(&state, org, item_id).await?;
    let spec = pdm_db::get_item_sheet_metal(&state.pool, org, item_id)
        .await?
        .and_then(|s| s.flat_pattern_sha)
        .ok_or_else(|| {
            AppError(PdmError::NotFound(format!(
                "item {} has no flat pattern attached",
                item.part_number
            )))
        })?;
    let blob = pdm_db::get_blob_by_sha(&state.pool, org, &spec)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("flat pattern blob".into())))?;
    let data = state
        .store
        .get_by_sha(&blob.sha256)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/dxf")
        .header("content-length", data.len().to_string())
        .header("content-digest", format!("sha-256={}", blob.sha256))
        .header(
            "content-disposition",
            format!(
                "attachment; filename=\"{}-flat.dxf\"",
                item.part_number
                    .chars()
                    .map(|c| if matches!(c, '/' | '\\' | ':') {
                        '_'
                    } else {
                        c
                    })
                    .collect::<String>()
            ),
        )
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(format!("response build: {e}"))))
}

/// Resolve the item's nestable flat-pattern DXF text (latest revision-bound
/// artifact → spec pointer → head DXF version).
async fn flat_pattern_text(state: &AppState, org: Uuid, item_id: Uuid) -> Result<String, AppError> {
    let req = NestPartReq {
        item_id,
        file_version_id: None,
        layer_cut: None,
        layers_mark: None,
        unit: None,
        quantity: 1,
        mirror: false,
        allow_mirror: true,
        grain: None,
        step_angle: 90.0,
        priority: 1,
        label: None,
        use_bounding_box: false,
        common_edge: false,
        min_qty: 0,
        filler_qty: 0,
    };
    let src = resolve_part_source(state, org, &req).await?;
    let blob = pdm_db::get_blob_by_sha(&state.pool, org, &src.dxf_sha)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("flat pattern blob".into())))?;
    let data = state
        .store
        .get_by_sha(&blob.sha256)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    String::from_utf8(data).map_err(|e| {
        AppError(PdmError::Validation(format!(
            "flat pattern is not UTF-8: {e}"
        )))
    })
}

/// GET /items/{item_id}/flat-pattern/preview — an SVG preview of the item's
/// flat pattern for the nest part picker (profile / holes / marks).
pub async fn flat_pattern_preview(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(item_id): Path<Uuid>,
) -> Result<Response, AppError> {
    let org = require_org(&authed);
    owned_item(&state, org, item_id).await?;
    let text = flat_pattern_text(&state, org, item_id).await?;
    let dxf = pdm_nesting::dxf::parse(&text)
        .map_err(|e| AppError(PdmError::Validation(e.to_string())))?;
    let (cuts, marks) = pdm_nesting::part::auto_layers(&dxf, &[]);
    let prims = pdm_nesting::dxf::to_mm(&dxf, "unknown", "unknown");
    let assembled = pdm_nesting::part::assemble(
        &prims,
        &cuts,
        &marks,
        pdm_nesting::part::DEFAULT_CHORD_TOL,
        pdm_nesting::part::DEFAULT_JOIN_TOL,
    )
    .map_err(|e| AppError(PdmError::Validation(format!("{e:?}"))))?;
    let svg = pdm_nesting::sheet::render_parts_preview(&assembled.parts, 320.0, 200.0);
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "image/svg+xml")
        .body(axum::body::Body::from(svg))
        .map_err(|e| AppError(PdmError::Internal(format!("response build: {e}"))))
}

/// GET /sheet-metal/flat-patterns?items=id1,id2,… — export the flat patterns
/// of several items as a single ZIP (one DXF per part plus a manifest
/// listing what was included and what was missing).
pub async fn export_flat_patterns(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<FlatPatternExportQuery>,
) -> Result<Response, AppError> {
    let org = require_org(&authed);
    let ids: Vec<Uuid> = q
        .items
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse())
        .collect::<Result<Vec<Uuid>, _>>()
        .map_err(|_| {
            AppError(PdmError::BadRequest(
                "items must be comma-separated UUIDs".into(),
            ))
        })?;
    if ids.is_empty() {
        return Err(AppError(PdmError::BadRequest("no items given".into())));
    }
    if ids.len() > 200 {
        return Err(AppError(PdmError::BadRequest(
            "at most 200 items per export".into(),
        )));
    }
    let entries = pdm_db::list_flat_patterns(&state.pool, org, &ids).await?;
    let by_item: std::collections::HashMap<Uuid, _> =
        entries.iter().map(|e| (e.item_id, e)).collect();

    let mut included = Vec::new();
    let mut missing = Vec::new();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut used_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    for id in &ids {
        match by_item.get(id) {
            None => missing.push(serde_json::json!({
                "item_id": id,
                "reason": "item not found in this organization",
            })),
            Some(e) => match e.flat_pattern_sha.clone() {
                None => missing.push(serde_json::json!({
                    "item_id": id,
                    "part_number": e.part_number,
                    "reason": "no flat pattern attached",
                })),
                Some(sha) => {
                    // POST /sheet-metal takes the sha from the client: only this
                    // organization's blobs.
                    if pdm_db::get_blob_by_sha(&state.pool, org, &sha)
                        .await?
                        .is_none()
                    {
                        missing.push(serde_json::json!({
                            "item_id": id,
                            "part_number": e.part_number,
                            "reason": "flat pattern blob not found in this organization",
                        }));
                        continue;
                    }
                    let data = match state.store.get_by_sha(&sha).await {
                        Ok(data) => data,
                        Err(err) => {
                            missing.push(serde_json::json!({
                                "item_id": id,
                                "part_number": e.part_number,
                                "reason": format!("flat pattern blob not found ({err})"),
                            }));
                            continue;
                        }
                    };
                    let safe_part: String = e
                        .part_number
                        .chars()
                        .map(|c| {
                            if matches!(c, '/' | '\\' | ':') {
                                '_'
                            } else {
                                c
                            }
                        })
                        .collect();
                    let mut name = format!("{safe_part}-flat.dxf");
                    while !used_names.insert(name.clone()) {
                        name = format!("{safe_part}-flat-{item}.dxf", item = e.item_id);
                    }
                    let opts = zip::write::FileOptions::<()>::default();
                    zip.start_file(&name, opts)
                        .map_err(|err| AppError(PdmError::Internal(format!("zip: {err}"))))?;
                    std::io::Write::write_all(&mut zip, &data)
                        .map_err(|err| AppError(PdmError::Internal(format!("zip: {err}"))))?;
                    included.push(serde_json::json!({
                        "item_id": e.item_id,
                        "part_number": e.part_number,
                        "revision": e.revision,
                        "file": name,
                        "sha256": sha,
                        "size_bytes": e.flat_pattern_size,
                    }));
                }
            },
        }
    }
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "generated_at": chrono::Utc::now(),
        "included": included,
        "missing": missing,
    }))
    .map_err(|err| AppError(PdmError::Internal(format!("manifest: {err}"))))?;
    let opts = zip::write::FileOptions::<()>::default();
    zip.start_file("manifest.json", opts)
        .map_err(|err| AppError(PdmError::Internal(format!("zip: {err}"))))?;
    std::io::Write::write_all(&mut zip, &manifest)
        .map_err(|err| AppError(PdmError::Internal(format!("zip: {err}"))))?;
    let bytes = zip
        .finish()
        .map_err(|err| AppError(PdmError::Internal(format!("zip: {err}"))))?
        .into_inner();
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/zip")
        .header("content-length", bytes.len().to_string())
        .header(
            "content-disposition",
            format!("attachment; filename=\"flat-patterns-{stamp}.zip\""),
        )
        .body(axum::body::Body::from(bytes))
        .map_err(|e| AppError(PdmError::Internal(format!("response build: {e}"))))
}

// ── Sheet-metal standards library (MFG reference data) ──────────────────

#[derive(Deserialize)]
pub struct CreateSheetMaterialStandardReq {
    pub material_group: String,
    pub material: String,
    pub thickness_mm: f64,
    pub thickness_gauge: Option<String>,
    pub density_g_cm3: Option<f64>,
    pub min_bend_radius_mm: Option<f64>,
    pub k_factor: Option<f64>,
    pub bend_deduction_mm: Option<f64>,
    pub bend_allowance_mm: Option<f64>,
    pub notes: Option<String>,
}

/// GET /sheet-materials — the org's sheet-metal standards library (seeded
/// from the global catalog on first access). Rows are grouped by material so
/// each material carries its density once and lists its thickness rows.
///
/// `?unit=metric` (default) returns the full catalog; `?unit=saes` (or
/// `imperial`) returns only the rows that carry a US gauge label, i.e. the
/// SAE/imperial sheet set. Every thickness row reports `thickness_mm` and
/// `thickness_in` so the client can render the active unit system.
#[derive(Deserialize)]
pub struct SheetMaterialsQuery {
    pub unit: Option<String>,
}

pub async fn list_sheet_materials(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<SheetMaterialsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let unit = q.unit.as_deref().map(str::trim);
    let is_saes = matches!(
        unit.map(str::to_ascii_lowercase).as_deref(),
        Some("saes") | Some("imperial") | Some("in")
    );
    pdm_db::ensure_sheet_material_standards(&state.pool, org).await?;
    let rows = pdm_db::list_sheet_material_standards(&state.pool, org, unit).await?;
    // Group by (material_group, material): keep first-seen order.
    let thickness_json = |row: &pdm_core::SheetMaterialStandard| {
        serde_json::json!({
            "id": row.id,
            "thickness_mm": row.thickness_mm,
            "thickness_in": row.thickness_mm / 25.4,
            "thickness_gauge": row.thickness_gauge,
            "min_bend_radius_mm": row.min_bend_radius_mm,
            "k_factor": row.k_factor,
            "bend_deduction_mm": row.bend_deduction_mm,
            "bend_allowance_mm": row.bend_allowance_mm,
            "notes": row.notes,
        })
    };
    let mut materials: Vec<serde_json::Value> = Vec::new();
    for row in &rows {
        let entry = materials
            .iter_mut()
            .find(|m| m["material"] == row.material && m["group"] == row.material_group);
        match entry {
            Some(m) => {
                if let Some(arr) = m["thicknesses"].as_array_mut() {
                    arr.push(thickness_json(row));
                }
            }
            None => materials.push(serde_json::json!({
                "group": row.material_group,
                "material": row.material,
                "density_g_cm3": row.density_g_cm3,
                "thicknesses": [ thickness_json(row) ],
            })),
        }
    }
    Ok(serde_json::json!({
        "unit": if is_saes { "saes" } else { "metric" },
        "materials": materials,
    })
    .into())
}

/// POST /sheet-materials — add one standard (material x thickness) to the org.
pub async fn create_sheet_material_standard(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateSheetMaterialStandardReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let row = pdm_db::create_sheet_material_standard(
        &state.pool,
        org,
        &body.material_group,
        &body.material,
        body.thickness_mm,
        body.thickness_gauge.as_deref(),
        body.density_g_cm3,
        body.min_bend_radius_mm,
        body.k_factor,
        body.bend_deduction_mm,
        body.bend_allowance_mm,
        body.notes.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({ "standard": row }).into())
}

// ── G-code parsing (MFG/CAM read-only preview) ───────────────────────────

#[derive(Deserialize)]
pub struct ParseGcodeReq {
    pub gcode: String,
}

/// POST /gcode/parse — read-only parse of G-code into a summary + 2.5D toolpath for visualization.
pub async fn parse_gcode_endpoint(
    Extension(authed): Extension<Authed>,
    Json(body): Json<ParseGcodeReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let _org = require_org(&authed);
    let summary = pdm_core::gcode::parse_gcode(&body.gcode);
    Ok(serde_json::json!({ "summary": summary }).into())
}

// ── Laser nesting (Sparrow) ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct NestPartReq {
    pub item_id: Uuid,
    /// Exact CAD version to nest; omitted → the item's newest flat pattern.
    pub file_version_id: Option<Uuid>,
    #[serde(default = "default_qty")]
    pub quantity: i32,
    /// Cut layer. "auto" (default) follows flat-pattern conventions:
    /// CUT/OUTLINE/contour layers are cut, BEND_*/mark/etch layers are
    /// marking. A specific layer name restricts cutting to that layer.
    pub layer_cut: Option<String>,
    pub layers_mark: Option<Vec<String>>,
    /// Source unit override: "mm" | "in" | "unknown" (defer to the file).
    pub unit: Option<String>,
    /// Use a mirror of the original geometry for this line (default false).
    #[serde(default)]
    pub mirror: bool,
    /// Allow the solver to flip copies of this line (default false). A flipped copy is cut with
    /// the sheet's bottom face up, which is wrong for sheet with one good face, so flipping is
    /// something a line asks for.
    #[serde(default)]
    pub allow_mirror: bool,
    /// This part's grain preference: "along_length" | "along_width" | "any"
    /// (default "any"). Drives rotation restriction with the sheet grain.
    #[serde(default)]
    pub grain: Option<String>,
    /// Rotation increment in degrees (90 default => 0/90/180/270; 45 => 8;
    /// 180 => 0/180).
    #[serde(default = "default_step_angle")]
    pub step_angle: f64,
    /// Nesting priority (lower number placed first). Default 1.
    #[serde(default = "default_priority")]
    pub priority: i32,
    /// Part label drawn when the nest has "add part labels" on.
    #[serde(default)]
    pub label: Option<String>,
    /// Nest by the part's bounding box instead of its outline.
    #[serde(default)]
    pub use_bounding_box: bool,
    /// Request common-edge alignment (preference).
    #[serde(default)]
    pub common_edge: bool,
    /// Minimum placed quantity (preference; quantity is the primary demand).
    #[serde(default)]
    pub min_qty: i32,
    /// Extra filler copies beyond quantity (preference).
    #[serde(default)]
    pub filler_qty: i32,
}

fn default_step_angle() -> f64 {
    90.0
}
fn default_priority() -> i32 {
    1
}

fn default_qty() -> i32 {
    1
}

fn default_true() -> bool {
    true
}

/// One sheet-size definition for a multi-size nest (dimensions in `units`).
#[derive(Deserialize)]
pub struct SheetDefReq {
    pub width_mm: f64,
    pub length_mm: f64,
    /// Number of physical sheets of this size; `None` = as many as needed.
    #[serde(default)]
    pub quantity: Option<i32>,
}

#[derive(Deserialize)]
pub struct CreateLaserNestReq {
    pub name: String,
    pub project_id: Option<Uuid>,
    pub machine_id: Option<Uuid>,
    pub material: Option<String>,
    pub thickness_mm: Option<f64>,
    /// The unit the user entered `stock_width_mm`, `stock_height_mm`,
    /// `edge_margin_mm` and `part_gap_mm` in: "mm" (default) or "in". All
    /// dimensions are stored internally in millimetres; "in" inputs are
    /// converted (×25.4) at create time.
    #[serde(default)]
    pub units: Option<String>,
    pub stock_width_mm: f64,
    pub stock_height_mm: f64,
    /// Sheet-size definitions to work through in order. When present and
    /// non-empty, these (converted to mm) replace the single stock size;
    /// `stock_width_mm`/`stock_height_mm` then record the primary sheet.
    #[serde(default)]
    pub sheets: Vec<SheetDefReq>,
    pub edge_margin_mm: Option<f64>,
    /// Uniform part spacing (legacy): used for both axes when the separate
    /// `spacing_*_mm` values are absent.
    pub part_gap_mm: Option<f64>,
    /// Horizontal (x-axis) spacing between parts, in `units`.
    pub spacing_x_mm: Option<f64>,
    /// Vertical (y-axis) spacing between parts, in `units`.
    pub spacing_y_mm: Option<f64>,
    /// Sheet grain direction: "along_length" | "along_width" | "any"
    /// (default "along_length"). Drives per-part rotation restriction.
    #[serde(default)]
    pub grain_direction: Option<String>,
    /// Which corner the fill starts from (default "lower_left").
    #[serde(default)]
    pub nesting_corner: Option<String>,
    /// Nesting origin offset from the stock lower-left corner, in `units`
    /// (default 0).
    #[serde(default)]
    pub origin_x_mm: Option<f64>,
    #[serde(default)]
    pub origin_y_mm: Option<f64>,
    /// Reference gap between sheets, in `units` (for the manifest/package).
    #[serde(default)]
    pub sheet_sheet_distance_mm: Option<f64>,
    /// Request a guillotine-cuttable layout (preference).
    #[serde(default)]
    pub guillotine: bool,
    /// Draw part labels on the sheet DXF + preview.
    #[serde(default)]
    pub add_part_labels: bool,
    /// Fill direction preference ("auto" default).
    #[serde(default)]
    pub fill_direction: Option<String>,
    /// Use automatic sheet origins (default true).
    #[serde(default = "default_true")]
    pub auto_sheet_origins: bool,
    /// Fill all requested sheets even if not needed (preference).
    #[serde(default)]
    pub fill_all_sheets: bool,
    /// Optimize part pairs (preference).
    #[serde(default)]
    pub optimized_pairs: bool,
    /// Source unit for part DXFs that declare no `$INSUNITS` and have no
    /// per-part override: "mm", "in", or "auto" (default — honour the file's
    /// `$INSUNITS`, falling back to mm when the file is unitless).
    #[serde(default)]
    pub default_part_unit: Option<String>,
    pub allowed_rotations: Option<Vec<f64>>,
    pub seed: Option<i64>,
    #[serde(default)]
    pub parts: Vec<NestPartReq>,
}

#[derive(Deserialize)]
pub struct ListLaserNestsQuery {
    pub project_id: Option<Uuid>,
    pub limit: Option<i32>,
}

/// Resolved, version-pinned source for one nest part.
struct PartSource {
    item_id: Uuid,
    file_version_id: Uuid,
    artifact_id: Option<Uuid>,
    dxf_sha: String,
    dxf_size: i64,
    part_number: String,
    revision_label: String,
    layer_cut: String,
    layers_mark: Vec<String>,
    unit: String,
    mirror: bool,
    allow_mirror: bool,
    part_grain: String,
    step_angle: f64,
    priority: i32,
    label: String,
    use_bounding_box: bool,
    common_edge: bool,
    min_qty: i32,
    filler_qty: i32,
    material: Option<String>,
    thickness_mm: Option<f64>,
}

fn is_dxf_filename(name: &str) -> bool {
    name.rsplit('.')
        .next()
        .map(|e| e.eq_ignore_ascii_case("dxf"))
        .unwrap_or(false)
}

/// Millimetres-per-unit factor for the nest's dimensional inputs (stock,
/// margin, gap). "in"/"inch" -> 25.4, otherwise 1.0.
fn stock_unit_factor(units: &Option<String>) -> f64 {
    match units
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("in") | Some("inch") | Some("inches") | Some("imperial") => 25.4,
        _ => 1.0,
    }
}

/// The nest-level default part source unit (a concrete "mm"/"in"). Files that
/// declare `$INSUNITS` are still honoured first; this only applies to
/// unit-less files. Defaults to mm.
fn default_part_unit_of(v: &Option<String>) -> String {
    match v
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("in") | Some("inch") | Some("inches") | Some("imperial") => "in".into(),
        _ => "mm".into(),
    }
}

/// Resolve the exact flat-pattern DXF + metadata for a requested nest part.
async fn resolve_part_source(
    state: &AppState,
    org: Uuid,
    req: &NestPartReq,
) -> Result<PartSource, AppError> {
    let item = pdm_db::get_item(&state.pool, req.item_id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound(format!("item {}", req.item_id))))?;
    let spec = pdm_db::get_item_sheet_metal(&state.pool, org, req.item_id).await?;

    // 1. An explicit version, or the newest artifact, or the legacy pointer.
    let (version_id, dxf_sha, dxf_size, unit, artifact_id): (
        Uuid,
        String,
        i64,
        String,
        Option<Uuid>,
    ) = if let Some(v) = req.file_version_id {
        // The version must be one of this item's (and so this organization's).
        if pdm_db::version_source(&state.pool, org, v)
            .await?
            .is_none_or(|s| s.item_id != req.item_id)
        {
            return Err(AppError(PdmError::NotFound(format!("version {v}"))));
        }
        let version = pdm_db::get_version(&state.pool, v)
            .await?
            .ok_or_else(|| AppError(PdmError::NotFound(format!("version {v}"))))?;
        // Prefer a revision-bound artifact for this version.
        if let Some(art) =
            pdm_db::latest_flat_pattern_artifact(&state.pool, org, req.item_id, v).await?
        {
            (v, art.sha256, art.size_bytes, art.unit, Some(art.id))
        } else if is_dxf_filename(&item_filename(&state.pool, version.file_id).await) {
            // The version itself is a DXF.
            let blob = pdm_db::get_blob_by_id(&state.pool, version.blob_id)
                .await?
                .ok_or_else(|| AppError(PdmError::Validation("version has no blob".into())))?;
            (
                v,
                blob.sha256,
                blob.size_bytes,
                req.unit.clone().unwrap_or_else(|| "unknown".into()),
                None,
            )
        } else {
            return Err(AppError(PdmError::Validation(
                "the selected version has no flat-pattern DXF".into(),
            )));
        }
    } else if let Some(art) = pdm_db::list_flat_pattern_artifacts(&state.pool, org, req.item_id)
        .await?
        .into_iter()
        .next()
    {
        (
            art.file_version_id,
            art.sha256,
            art.size_bytes,
            art.unit,
            Some(art.id),
        )
    } else if let Some(sha) = spec.as_ref().and_then(|s| s.flat_pattern_sha.clone()) {
        let file = pdm_db::get_file(&state.pool, req.item_id)
            .await?
            .ok_or_else(|| AppError(PdmError::Validation("item has no file".into())))?;
        let head = pdm_db::get_head_version(&state.pool, file.id)
            .await?
            .ok_or_else(|| AppError(PdmError::Validation("item has no version".into())))?;
        // POST /sheet-metal takes this sha from the client: only this organization's blobs.
        let size = pdm_db::get_blob_by_sha(&state.pool, org, &sha)
            .await?
            .map(|b| b.size_bytes)
            .ok_or_else(|| {
                AppError(PdmError::Validation(
                    "the flat-pattern DXF is not in this organization".into(),
                ))
            })?;
        (head.id, sha, size, "unknown".into(), None)
    } else if let Some(head) = head_dxf_version(&state.pool, org, req.item_id).await? {
        // The item's head version is itself a DXF flat pattern.
        let blob = pdm_db::get_blob_by_id(&state.pool, head.blob_id)
            .await?
            .ok_or_else(|| AppError(PdmError::Validation("version has no blob".into())))?;
        (
            head.id,
            blob.sha256,
            blob.size_bytes,
            req.unit.clone().unwrap_or_else(|| "unknown".into()),
            None,
        )
    } else {
        return Err(AppError(PdmError::Validation(
            "no flat-pattern DXF for this part; generate or attach one first".into(),
        )));
    };

    let version = pdm_db::get_version(&state.pool, version_id)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("version".into())))?;
    let revision_label = format!("V{}-R{}", version.version, version.revision);

    Ok(PartSource {
        item_id: req.item_id,
        file_version_id: version_id,
        artifact_id,
        dxf_sha,
        dxf_size,
        part_number: item.part_number,
        revision_label,
        layer_cut: req
            .layer_cut
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| pdm_nesting::part::LAYER_CUT_AUTO.to_string()),
        layers_mark: req.layers_mark.clone().unwrap_or_default(),
        unit: req.unit.clone().unwrap_or(unit),
        mirror: req.mirror,
        allow_mirror: req.allow_mirror,
        part_grain: grain_value(req.grain.as_deref()),
        step_angle: req.step_angle,
        priority: req.priority,
        label: req.label.clone().unwrap_or_default(),
        use_bounding_box: req.use_bounding_box,
        common_edge: req.common_edge,
        min_qty: req.min_qty,
        filler_qty: req.filler_qty,
        material: spec.as_ref().and_then(|s| s.material.clone()),
        thickness_mm: spec.as_ref().and_then(|s| s.thickness_mm),
    })
}

/// Normalise a part grain direction to the stored canonical form (default
/// "any": no preference).
fn grain_value(v: Option<&str>) -> String {
    match v.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("along_width") | Some("across") | Some("width") => "along_width".into(),
        Some("along_length") | Some("length") => "along_length".into(),
        _ => "any".into(),
    }
}

/// Normalise the sheet grain direction (default "along_length").
fn sheet_grain_value(v: &Option<String>) -> String {
    match v
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("along_width") | Some("across") | Some("width") => "along_width".into(),
        Some("any") => "any".into(),
        _ => "along_length".into(),
    }
}

/// Normalise the nesting corner (default "lower_left").
fn corner_value(v: &Option<String>) -> String {
    match v
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("lower_right") | Some("bottom_right") => "lower_right".into(),
        Some("upper_left") | Some("top_left") => "upper_left".into(),
        Some("upper_right") | Some("top_right") => "upper_right".into(),
        _ => "lower_left".into(),
    }
}

async fn item_filename(pool: &sqlx::PgPool, file_id: Uuid) -> String {
    pdm_db::get_file_by_id(pool, file_id)
        .await
        .ok()
        .flatten()
        .map(|f| f.filename)
        .unwrap_or_default()
}

/// The item's head version, if its file is a DXF flat pattern.
async fn head_dxf_version(
    pool: &sqlx::PgPool,
    _org: Uuid,
    item_id: Uuid,
) -> Result<Option<pdm_core::FileVersion>, AppError> {
    let file = match pdm_db::get_file(pool, item_id).await? {
        Some(f) => f,
        None => return Ok(None),
    };
    if !is_dxf_filename(&file.filename) {
        return Ok(None);
    }
    pdm_db::get_head_version(pool, file.id)
        .await
        .map_err(AppError)
}

/// Preflight: resolve every part and report structured readiness without creating
/// the run.
pub async fn validate_laser_nest(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateLaserNestReq>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    if body.parts.is_empty() {
        return Err(AppError(PdmError::BadRequest("no parts".into())));
    }
    let mut problems: Vec<serde_json::Value> = Vec::new();
    let mut parts = Vec::new();
    for p in &body.parts {
        match resolve_part_source(&state, org, p).await {
            Ok(src) => parts.push(src),
            Err(e) => problems.push(serde_json::json!({
                "item_id": p.item_id,
                "problem": e.0.to_string(),
            })),
        }
    }
    // Mixed-material / thickness check.
    let materials: Vec<&Option<String>> = parts.iter().map(|p| &p.material).collect();
    let thickness: Vec<Option<f64>> = parts.iter().map(|p| p.thickness_mm).collect();
    let material_ok = materials
        .windows(2)
        .all(|w| w[0].as_deref() == w[1].as_deref());
    let thickness_ok = thickness.windows(2).all(|w| w[0] == w[1]);
    Ok(serde_json::json!({
        "ok": problems.is_empty() && material_ok && thickness_ok,
        "problems": problems,
        "mixed_material": !material_ok,
        "mixed_thickness": !thickness_ok,
        "material": body.material.clone().or_else(|| parts.first().and_then(|p| p.material.clone())),
        "thickness_mm": body.thickness_mm.or_else(|| parts.first().and_then(|p| p.thickness_mm)),
        "parts": parts.iter().zip(body.parts.iter()).map(|(p, req)| serde_json::json!({
            "item_id": p.item_id,
            "part_number": p.part_number,
            "revision": p.revision_label,
            "file_version_id": p.file_version_id,
            "dxf_sha": p.dxf_sha,
            "quantity": req.quantity,
            "unit": p.unit,
        })).collect::<Vec<_>>(),
    })
    .into())
}

pub async fn create_laser_nest(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Json(body): Json<CreateLaserNestReq>,
) -> Result<Response, AppError> {
    let user = require_role(&authed, "editor")?;
    let org = require_org(&authed);
    if body.parts.is_empty() {
        return Err(AppError(PdmError::BadRequest("no parts".into())));
    }
    // Interpret the dimensional inputs in the user's chosen unit; store mm.
    // Unspecified margin/spacing use physical mm defaults (10 mm / 5 mm).
    let um = stock_unit_factor(&body.units);
    let stock_w = body.stock_width_mm * um;
    let stock_h = body.stock_height_mm * um;
    let margin = body.edge_margin_mm.map(|m| m * um).unwrap_or(10.0);
    // Directional spacing: explicit per-axis values win; the legacy uniform
    // part_gap applies to both; otherwise physical 5 mm.
    let legacy_gap = body.part_gap_mm.map(|g| g * um);
    let gap_x = body
        .spacing_x_mm
        .map(|g| g * um)
        .or(legacy_gap)
        .unwrap_or(5.0);
    let gap_y = body
        .spacing_y_mm
        .map(|g| g * um)
        .or(legacy_gap)
        .unwrap_or(5.0);
    let default_part_unit = default_part_unit_of(&body.default_part_unit);
    let grain_direction = sheet_grain_value(&body.grain_direction);
    let nesting_corner = corner_value(&body.nesting_corner);
    let origin_x = body.origin_x_mm.map(|v| v * um).unwrap_or(0.0);
    let origin_y = body.origin_y_mm.map(|v| v * um).unwrap_or(0.0);
    let sheet_sheet_distance = body.sheet_sheet_distance_mm.map(|v| v * um).unwrap_or(0.0);
    let fill_direction = body.fill_direction.clone().unwrap_or_else(|| "auto".into());
    // Sheet-size definitions (mm). Empty = the legacy single stock size.
    let sheet_defs: Vec<pdm_core::SheetDef> = if body.sheets.is_empty() {
        vec![]
    } else {
        for s in &body.sheets {
            if s.width_mm <= 0.0 || s.length_mm <= 0.0 {
                return Err(AppError(PdmError::BadRequest(
                    "each sheet definition needs positive width and length".into(),
                )));
            }
            if let Some(q) = s.quantity
                && q < 1
            {
                return Err(AppError(PdmError::BadRequest(
                    "sheet quantity must be at least 1 (or omitted for 'as many as needed')".into(),
                )));
            }
        }
        body.sheets
            .iter()
            .map(|s| pdm_core::SheetDef {
                width_mm: s.width_mm * um,
                length_mm: s.length_mm * um,
                quantity: s.quantity,
            })
            .collect()
    };
    let mut resolved: Vec<PartSource> = Vec::new();
    for p in &body.parts {
        resolved.push(resolve_part_source(&state, org, p).await?);
    }
    // Mixed material/thickness within one run is rejected.
    for w in resolved.windows(2) {
        if w[0].material != w[1].material {
            return Err(AppError(PdmError::Validation(
                "all parts in a nest must share the same material (use separate runs)".into(),
            )));
        }
        if w[0].thickness_mm != w[1].thickness_mm {
            return Err(AppError(PdmError::Validation(
                "all parts in a nest must share the same thickness (use separate runs)".into(),
            )));
        }
    }
    let material = body
        .material
        .clone()
        .or_else(|| resolved.first().and_then(|p| p.material.clone()));
    let thickness = body
        .thickness_mm
        .or_else(|| resolved.first().and_then(|p| p.thickness_mm));

    let params = pdm_db::LaserNestParams {
        name: body.name,
        project_id: body.project_id,
        machine_id: body.machine_id,
        material,
        thickness_mm: thickness,
        stock_width_mm: stock_w,
        stock_height_mm: stock_h,
        edge_margin_mm: margin,
        part_gap_mm: legacy_gap.unwrap_or(5.0),
        spacing_x_mm: gap_x,
        spacing_y_mm: gap_y,
        grain_direction,
        nesting_corner,
        origin_x_mm: origin_x,
        origin_y_mm: origin_y,
        sheet_sheet_distance_mm: sheet_sheet_distance,
        guillotine: body.guillotine,
        add_part_labels: body.add_part_labels,
        fill_direction,
        auto_sheet_origins: body.auto_sheet_origins,
        fill_all_sheets: body.fill_all_sheets,
        optimized_pairs: body.optimized_pairs,
        sheet_defs,
        chord_tol_mm: pdm_nesting::part::DEFAULT_CHORD_TOL,
        join_tol_mm: pdm_nesting::part::DEFAULT_JOIN_TOL,
        default_part_unit,
        // Empty = derive rotations from the grain settings.
        allowed_rotations: body.allowed_rotations.clone().unwrap_or_default(),
        time_budget_sec: 600,
        attempt_limit_sec: 60,
        max_sheets: 16,
        seed: body.seed,
        solver_version: Some("sparrow 0.3.0 / jagua-rs 2.0.0".into()),
        engine_build: Some("pinned sparrow b8ef033".into()),
        created_by: Some(user.id),
        parent_nest_id: None,
    };
    let part_inputs: Vec<pdm_db::LaserNestPartInput> = resolved
        .iter()
        .zip(body.parts.iter())
        .enumerate()
        .map(|(ordinal, (src, req))| pdm_db::LaserNestPartInput {
            item_id: src.item_id,
            file_version_id: src.file_version_id,
            artifact_id: src.artifact_id,
            dxf_sha: src.dxf_sha.clone(),
            dxf_size: src.dxf_size,
            part_number: src.part_number.clone(),
            revision_label: Some(src.revision_label.clone()),
            quantity: req.quantity.max(1),
            layer_cut: src.layer_cut.clone(),
            layers_mark: src.layers_mark.clone(),
            unit: src.unit.clone(),
            mirror: src.mirror,
            allow_mirror: src.allow_mirror,
            part_grain: src.part_grain.clone(),
            step_angle: src.step_angle,
            priority: src.priority,
            label: src.label.clone(),
            use_bounding_box: src.use_bounding_box,
            common_edge: src.common_edge,
            min_qty: src.min_qty,
            filler_qty: src.filler_qty,
            ordinal: ordinal as i32,
        })
        .collect();

    let (nest, job) =
        pdm_db::create_laser_nest_run(&state.pool, org, &params, &part_inputs).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "laser_nest.create",
        "laser_nest",
        None,
        Some(serde_json::json!(nest.id)),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "nest": nest, "job": job })),
    )
        .into_response())
}

pub async fn list_laser_nests(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Query(q): Query<ListLaserNestsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let nests =
        pdm_db::list_laser_nests(&state.pool, org, q.project_id, q.limit.unwrap_or(50)).await?;
    Ok(serde_json::json!({ "nests": nests }).into())
}

pub async fn get_laser_nest(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let org = require_org(&authed);
    let detail = pdm_db::get_laser_nest_detail(&state.pool, id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("laser nest".into())))?;
    Ok(serde_json::json!({ "detail": detail }).into())
}

/// POST /laser-nests/{id}/cancel — cancel a queued/running nest.
pub async fn cancel_laser_nest(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let nest = pdm_db::get_laser_nest(&state.pool, id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("laser nest".into())))?;
    if !matches!(nest.status.as_str(), "queued" | "running") {
        return Err(AppError(PdmError::Conflict(format!(
            "nest is {}/can't be cancelled",
            nest.status
        ))));
    }
    let nest = pdm_db::set_laser_nest_status(&state.pool, org, id, "cancelled", None)
        .await?
        .ok_or_else(|| AppError(PdmError::Conflict("nest already terminal".into())))?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "laser_nest.cancel",
        "laser_nest",
        None,
        Some(serde_json::json!(id)),
    )
    .await?;
    Ok(serde_json::json!({ "nest": nest }).into())
}

/// POST /laser-nests/{id}/rerun — a new linked run from the same source parts.
pub async fn rerun_laser_nest(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Response, AppError> {
    let user = require_role(&authed, "editor")?;
    let org = require_org(&authed);
    let parent = pdm_db::get_laser_nest(&state.pool, id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("laser nest".into())))?;
    let parts = pdm_db::list_laser_nest_parts(&state.pool, id, org).await?;
    let params = pdm_db::LaserNestParams {
        name: format!("{} (rerun)", parent.name),
        project_id: parent.project_id,
        machine_id: parent.machine_id,
        material: parent.material.clone(),
        thickness_mm: parent.thickness_mm,
        stock_width_mm: parent.stock_width_mm,
        stock_height_mm: parent.stock_height_mm,
        edge_margin_mm: parent.edge_margin_mm,
        part_gap_mm: parent.part_gap_mm,
        spacing_x_mm: parent.spacing_x_mm,
        spacing_y_mm: parent.spacing_y_mm,
        grain_direction: parent.grain_direction.clone(),
        nesting_corner: parent.nesting_corner.clone(),
        origin_x_mm: parent.origin_x_mm,
        origin_y_mm: parent.origin_y_mm,
        sheet_sheet_distance_mm: parent.sheet_sheet_distance_mm,
        guillotine: parent.guillotine,
        add_part_labels: parent.add_part_labels,
        fill_direction: parent.fill_direction.clone(),
        auto_sheet_origins: parent.auto_sheet_origins,
        fill_all_sheets: parent.fill_all_sheets,
        optimized_pairs: parent.optimized_pairs,
        sheet_defs: parent.sheet_defs.0.clone(),
        chord_tol_mm: parent.chord_tol_mm,
        join_tol_mm: parent.join_tol_mm,
        default_part_unit: parent.default_part_unit.clone(),
        allowed_rotations: parent.allowed_rotations.0.clone(),
        time_budget_sec: parent.time_budget_sec,
        attempt_limit_sec: parent.attempt_limit_sec,
        max_sheets: parent.max_sheets,
        seed: None,
        solver_version: parent.solver_version.clone(),
        engine_build: parent.engine_build.clone(),
        created_by: Some(user.id),
        parent_nest_id: Some(parent.id),
    };
    let inputs: Vec<pdm_db::LaserNestPartInput> = parts
        .iter()
        .map(|p| pdm_db::LaserNestPartInput {
            item_id: p.item_id,
            file_version_id: p.file_version_id,
            artifact_id: p.artifact_id,
            dxf_sha: p.dxf_sha.clone(),
            dxf_size: p.dxf_size,
            part_number: p.part_number.clone(),
            revision_label: p.revision_label.clone(),
            quantity: p.quantity,
            layer_cut: p.layer_cut.clone(),
            layers_mark: p.layers_mark.0.clone(),
            unit: p.unit.clone(),
            mirror: p.mirror,
            allow_mirror: p.allow_mirror,
            part_grain: p.part_grain.clone(),
            step_angle: p.step_angle,
            priority: p.priority,
            label: p.label.clone(),
            use_bounding_box: p.use_bounding_box,
            common_edge: p.common_edge,
            min_qty: p.min_qty,
            filler_qty: p.filler_qty,
            ordinal: p.ordinal,
        })
        .collect();
    let (nest, job) = pdm_db::create_laser_nest_run(&state.pool, org, &params, &inputs).await?;
    pdm_db::record_audit(
        &state.pool,
        org,
        Some(user.id),
        "",
        "laser_nest.rerun",
        "laser_nest",
        None,
        Some(serde_json::json!(nest.id)),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "nest": nest, "job": job })),
    )
        .into_response())
}

/// GET /laser-nests/{id}/sheets/{index}/dxf — download one sheet's DXF.
pub async fn download_laser_nest_sheet_dxf(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path((id, index)): Path<(Uuid, i32)>,
) -> Result<Response, AppError> {
    let org = require_org(&authed);
    let nest = pdm_db::get_laser_nest(&state.pool, id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("laser nest".into())))?;
    let sheet = pdm_db::get_laser_nest_sheet(&state.pool, id, org, index)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("sheet".into())))?;
    let sha = sheet
        .dxf_sha
        .ok_or_else(|| AppError(PdmError::NotFound("sheet DXF not produced".into())))?;
    let blob = pdm_db::get_blob_by_sha(&state.pool, org, &sha)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("sheet DXF blob".into())))?;
    let data = state
        .store
        .get_by_sha(&blob.sha256)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    let safe: String = nest
        .name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | ' ') {
                '_'
            } else {
                c
            }
        })
        .collect();
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/dxf")
        .header("content-length", data.len().to_string())
        .header(
            "content-disposition",
            format!("attachment; filename=\"{safe}-sheet{index}.dxf\""),
        )
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))
}

/// GET /laser-nests/{id}/sheets/{index}/preview — one sheet's SVG preview.
pub async fn download_laser_nest_sheet_preview(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path((id, index)): Path<(Uuid, i32)>,
) -> Result<Response, AppError> {
    let org = require_org(&authed);
    pdm_db::get_laser_nest(&state.pool, id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("laser nest".into())))?;
    let sheet = pdm_db::get_laser_nest_sheet(&state.pool, id, org, index)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("sheet".into())))?;
    let sha = sheet
        .preview_sha
        .ok_or_else(|| AppError(PdmError::NotFound("sheet preview not produced".into())))?;
    let blob = pdm_db::get_blob_by_sha(&state.pool, org, &sha)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("preview blob".into())))?;
    let data = state
        .store
        .get_by_sha(&blob.sha256)
        .await
        .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "image/svg+xml")
        .header("content-length", data.len().to_string())
        .body(axum::body::Body::from(data))
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))
}

/// GET /laser-nests/{id}/package — ZIP of every sheet's DXF + preview + manifest.
pub async fn download_laser_nest_package(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    Path(id): Path<Uuid>,
) -> Result<Response, AppError> {
    let org = require_org(&authed);
    let nest = pdm_db::get_laser_nest(&state.pool, id, org)
        .await?
        .ok_or_else(|| AppError(PdmError::NotFound("laser nest".into())))?;
    if nest.status != "completed" {
        return Err(AppError(PdmError::Conflict(format!(
            "nest is {}/; only completed nests export",
            nest.status
        ))));
    }
    let sheets = pdm_db::list_laser_nest_sheets(&state.pool, id, org).await?;
    let safe: String = nest
        .name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | ' ') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut manifest_lines = vec![format!("{safe} laser nest — {} sheets", sheets.len())];
    for sheet in &sheets {
        if let Some(sha) = &sheet.dxf_sha {
            let blob = pdm_db::get_blob_by_sha(&state.pool, org, sha)
                .await?
                .ok_or_else(|| AppError(PdmError::NotFound("sheet DXF blob".into())))?;
            let data = state
                .store
                .get_by_sha(&blob.sha256)
                .await
                .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
            let name = format!("{safe}-sheet{}.dxf", sheet.sheet_index);
            zip.start_file(&name, zip::write::FileOptions::<()>::default())
                .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
            std::io::Write::write_all(&mut zip, &data)
                .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
            manifest_lines.push(format!(
                "{} ({} bytes) — {} placements",
                name,
                data.len(),
                sheet.placed_count
            ));
        }
        if let Some(sha) = &sheet.preview_sha {
            let blob = pdm_db::get_blob_by_sha(&state.pool, org, sha)
                .await?
                .ok_or_else(|| AppError(PdmError::NotFound("preview blob".into())))?;
            let data = state
                .store
                .get_by_sha(&blob.sha256)
                .await
                .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
            let name = format!("{safe}-sheet{}.svg", sheet.sheet_index);
            zip.start_file(&name, zip::write::FileOptions::<()>::default())
                .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
            std::io::Write::write_all(&mut zip, &data)
                .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
        }
    }
    // The run's full manifest (settings, parts with their item and version, every placement),
    // when the run recorded one; nests from before it was kept have none.
    if let Some(sha) = &nest.manifest_sha {
        let blob = pdm_db::get_blob_by_sha(&state.pool, org, sha)
            .await?
            .ok_or_else(|| AppError(PdmError::NotFound("nest manifest blob".into())))?;
        let data = state
            .store
            .get_by_sha(&blob.sha256)
            .await
            .map_err(|e| AppError(PdmError::Storage(e.to_string())))?;
        zip.start_file("manifest.json", zip::write::FileOptions::<()>::default())
            .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
        std::io::Write::write_all(&mut zip, &data)
            .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
        manifest_lines.push("manifest.json — settings, parts and placements".to_string());
    }
    let manifest = manifest_lines.join("\n");
    zip.start_file("MANIFEST.txt", zip::write::FileOptions::<()>::default())
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
    std::io::Write::write_all(&mut zip, manifest.as_bytes())
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))?;
    let buf = zip
        .finish()
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))?
        .into_inner();
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    axum::http::Response::builder()
        .status(200)
        .header("content-type", "application/zip")
        .header("content-length", buf.len().to_string())
        .header(
            "content-disposition",
            format!("attachment; filename=\"{safe}-nest-{stamp}.zip\""),
        )
        .body(axum::body::Body::from(buf))
        .map_err(|e| AppError(PdmError::Internal(e.to_string())))
}

// ── Router ──────────────────────────────────────────────────────────────

pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/version", get(version))
        .route("/system-info", get(system_info))
        .route(
            "/public/design/{version_id}/{filename}",
            get(public_design_download),
        )
        .route("/me", get(me))
        .route(
            "/me/settings",
            get(client_settings_get).put(client_settings_put),
        )
        .route("/me/password", post(change_own_password))
        .route(
            "/settings/gpu-policy",
            get(get_gpu_policy).put(put_gpu_policy),
        )
        .route(
            "/settings/reading-font",
            crate::reading_font::method_router(),
        )
        .route(
            "/settings/preview-style",
            get(get_preview_style).put(put_preview_style),
        )
        .route("/settings/cad-cli", get(get_cad_cli))
        .route("/settings/cad-cli/update", post(update_cad_cli))
        .route("/settings/dutchman", get(get_dutchman).put(save_dutchman))
        .route("/settings/dutchman/update", post(update_dutchman))
        .route(
            "/settings/nest-defaults",
            get(get_nest_defaults).put(save_nest_defaults),
        )
        .route("/settings/solidcraft-web", get(get_solidcraft_web))
        .route(
            "/settings/solidcraft-web/update",
            post(update_solidcraft_web),
        )
        .route(
            "/settings/mail",
            get(get_mail_settings).put(put_mail_settings),
        )
        .route("/settings/mail/test", post(test_mail_settings))
        .route(
            "/settings/onshape",
            get(get_onshape_settings).put(put_onshape_settings),
        )
        .route(
            "/projects/{project_id}/onshape-import",
            post(onshape_import_request),
        )
        .route("/integrations/onshape/send", post(onshape_send))
        .route("/integrations/onshape/config", get(onshape_config))
        .route("/integrations/onshape/picker", get(onshape_picker))
        .route("/integrations/onshape/preview", get(onshape_preview))
        .route("/integrations/onshape/lookup", get(onshape_lookup))
        .route(
            "/settings/part-number-prefixes",
            get(list_part_number_prefixes).post(upsert_part_number_prefix),
        )
        .route(
            "/settings/part-number-prefixes/{category}",
            delete(delete_part_number_prefix),
        )
        .route("/projects", get(list_projects).post(create_project))
        .route(
            "/projects/{project_id}/onshape-folder",
            get(get_folder_sync).put(put_folder_sync),
        )
        .route(
            "/projects/{project_id}/onshape-folder/sync",
            post(start_folder_sync),
        )
        .route(
            "/projects/{project_id}",
            patch(update_project).delete(delete_project),
        )
        .route("/projects/{project_id}/archive", patch(archive_project))
        .route("/projects/{project_id}/unarchive", patch(unarchive_project))
        .route(
            "/projects/{project_id}/main-assembly",
            put(set_main_assembly),
        )
        .route(
            "/projects/{project_id}/items",
            get(list_items).post(create_item),
        )
        .route(
            "/projects/{project_id}/folders",
            get(list_folders).post(create_folder),
        )
        .route("/items", get(list_items_org))
        .route(
            "/items/{item_id}",
            get(get_item).put(update_item).delete(delete_item),
        )
        .route(
            "/items/{item_id}/lifecycle",
            patch(transition_lifecycle_handler),
        )
        .route("/items/{item_id}/archive", patch(archive_item))
        .route("/items/{item_id}/unarchive", patch(unarchive_item))
        .route(
            "/items/{item_id}/checkout",
            post(checkout).delete(undo_checkout),
        )
        .route("/items/{item_id}/checkin", post(checkin))
        .route("/items/{item_id}/versions", get(list_item_versions))
        .route("/items/{item_id}/properties", get(get_properties))
        .route("/items/{item_id}/properties/{key}", put(set_property))
        .route("/items/{item_id}/release", post(release))
        .route("/items/{item_id}/revisions", get(list_revisions))
        .route(
            "/items/{item_id}/comments",
            get(list_comments).post(add_comment),
        )
        .route("/blobs", put(upload_blob))
        .route("/versions/{version_id}/blob", get(download_version_blob))
        .route(
            "/versions/{version_id}/derived-step",
            get(download_version_derived_step),
        )
        .route(
            "/versions/{version_id}/thumbnail",
            get(download_version_thumbnail).put(upload_version_thumbnail),
        )
        .route("/items/{item_id}/thumbnail", get(download_item_thumbnail))
        .route(
            "/versions/{version_id}/solidcraft",
            get(solidcraft_conversion_status).post(request_solidcraft_conversion),
        )
        .route(
            "/versions/{version_id}/solidcraft/blob",
            get(download_solidcraft_artifact),
        )
        .route(
            "/versions/{version_id}/design-link",
            post(create_design_link),
        )
        .route("/versions/{version_id}/preview", post(regenerate_preview))
        .route("/jobs/{id}", get(get_job_handler))
        .route("/search", get(search))
        .route("/audit", get(audit))
        .route("/home", get(home))
        .route("/users", get(list_users_handler).post(create_user_handler))
        .route("/users/invite", post(invite_user_handler))
        .route("/users/{user_id}", put(update_user_handler))
        .route("/users/{user_id}/password", post(set_user_password_handler))
        .route("/users/{user_id}/tokens", post(create_token_handler))
        .route("/jobs", get(list_jobs_handler))
        .route(
            "/approvals",
            get(list_approvals_handler).post(create_approval_request_handler),
        )
        .route(
            "/approvals/{request_id}/decide",
            post(decide_approval_handler),
        )
        .route("/machines", get(list_machines).post(create_machine))
        .route("/tools", get(list_tools).post(create_tool))
        .route(
            "/tools/{id}",
            get(get_tool).put(update_tool).delete(delete_tool),
        )
        .route(
            "/tools/{id}/cutting-data",
            get(list_tool_cutting_data).post(create_tool_cutting_data),
        )
        .route(
            "/tools/{id}/cutting-data/{cid}",
            put(update_tool_cutting_data).delete(delete_tool_cutting_data),
        )
        .route(
            "/tools/{id}/documents",
            get(list_tool_documents).post(add_tool_document),
        )
        .route(
            "/tools/{id}/documents/{doc_id}/download",
            get(download_tool_document),
        )
        .route(
            "/tools/{id}/documents/{doc_id}",
            delete(delete_tool_document),
        )
        .route(
            "/post-processors",
            get(list_post_processors).post(create_post_processor),
        )
        .route("/mfg-setups", get(list_mfg_setups).post(create_mfg_setup))
        .route(
            "/cam-projects",
            get(list_cam_projects).post(create_cam_project),
        )
        .route(
            "/nc-programs",
            get(list_nc_programs).post(create_nc_program),
        )
        .route("/nc-programs/{id}/release", post(release_nc_program))
        .route(
            "/production-jobs",
            get(list_production_jobs).post(create_production_job),
        )
        .route(
            "/production-jobs/{id}/status",
            post(set_production_job_status),
        )
        .route(
            "/inspections",
            get(list_inspections).post(create_inspection),
        )
        .route("/items/{item_id}/manufacturing", get(item_manufacturing))
        .route("/manufacturing/dashboard", get(manufacturing_dashboard))
        .route("/print-jobs", get(list_print_jobs).post(create_print_job))
        .route("/print-jobs/{id}/status", post(set_print_job_status))
        .route(
            "/sheet-metal",
            get(list_sheet_metal).post(create_sheet_metal),
        )
        .route("/sheet-metal/flat-patterns", get(export_flat_patterns))
        .route(
            "/items/{item_id}/sheet-metal",
            get(item_sheet_metal).post(save_item_sheet_metal),
        )
        .route(
            "/items/{item_id}/flat-pattern",
            get(download_item_flat_pattern).post(upload_item_flat_pattern),
        )
        .route(
            "/items/{item_id}/flat-pattern/generate",
            post(generate_item_flat_pattern),
        )
        .route(
            "/items/{item_id}/flat-pattern/preview",
            get(flat_pattern_preview),
        )
        .route(
            "/sheet-materials",
            get(list_sheet_materials).post(create_sheet_material_standard),
        )
        .route("/gcode/parse", post(parse_gcode_endpoint))
        .route("/laser-nests/validate", post(validate_laser_nest))
        .route(
            "/laser-nests",
            get(list_laser_nests).post(create_laser_nest),
        )
        .route("/laser-nests/{id}", get(get_laser_nest))
        .route("/laser-nests/{id}/cancel", post(cancel_laser_nest))
        .route("/laser-nests/{id}/rerun", post(rerun_laser_nest))
        .route(
            "/laser-nests/{id}/sheets/{index}/dxf",
            get(download_laser_nest_sheet_dxf),
        )
        .route(
            "/laser-nests/{id}/sheets/{index}/preview",
            get(download_laser_nest_sheet_preview),
        )
        .route(
            "/laser-nests/{id}/package",
            get(download_laser_nest_package),
        )
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
}

#[cfg(test)]
mod tests {
    use super::{auto_revision_label, parse_onshape_part_studio_url, version_info};
    use serde::Deserialize;

    #[test]
    fn onshape_part_studio_url_parsing() {
        const DOC: &str = "0f3ca7ede7ca9aebc175ccaf";
        const ELEM: &str = "de5d16d475ff6db2d0a55436";
        let parsed = parse_onshape_part_studio_url(
            "https://cad.onshape.com/documents/0f3ca7ede7ca9aebc175ccaf/w/857a2d7bfb309c433ed57d15/e/de5d16d475ff6db2d0a55436",
        );
        assert!(
            matches!(&parsed, Ok((d, e)) if d.as_str() == DOC && e.as_str() == ELEM),
            "workspace link: {parsed:?}"
        );
        // The immutable version-pinned variant is accepted too.
        let parsed = parse_onshape_part_studio_url(
            "https://cad.onshape.com/documents/0f3ca7ede7ca9aebc175ccaf/v/abc000000000000000000000/e/de5d16d475ff6db2d0a55436",
        );
        assert!(
            matches!(&parsed, Ok((d, e)) if d.as_str() == DOC && e.as_str() == ELEM),
            "version link: {parsed:?}"
        );
        // Anything that is not a Part Studio link is refused.
        for url in [
            "https://cad.onshape.com/documents/0f3ca7ede7ca9aebc175ccaf",
            "http://cad.onshape.com/documents/0f3ca7ede7ca9aebc175ccaf/w/857a2d7bfb309c433ed57d15/e/de5d16d475ff6db2d0a55436",
            "https://cad.onshape.com/documents/0f3ca7ede7ca9aebc175ccaf/w/857a2d7bfb309c433ed57d15",
            "https://cad.onshape.com/documents/0f3ca7ede7ca9aebc175ccaf/w/857a2d7bfb309c433ed57d15/e/de5d16d475ff6db2d0a554",
            "https://other.example/documents/0f3ca7ede7ca9aebc175ccaf/w/857a2d7bfb309c433ed57d15/e/de5d16d475ff6db2d0a55436",
            "not a url",
        ] {
            assert!(parse_onshape_part_studio_url(url).is_err(), "{url}");
        }
    }

    /// A client written against the original /version schema still decodes a
    /// response with the additive `latest` / `update_available` fields, and a
    /// new client decoding an old response (fields absent) gets serde defaults.
    #[test]
    fn version_json_is_additive() -> serde_json::Result<()> {
        #[derive(Deserialize)]
        struct OldClient {
            name: String,
            version: String,
            build: Option<String>,
            commit: Option<String>,
            built_at: Option<String>,
        }
        #[derive(Deserialize)]
        struct NewClient {
            #[allow(dead_code)]
            name: String,
            #[serde(default)]
            latest: Option<String>,
            #[serde(default)]
            update_available: bool,
        }
        let mut body = version_info();
        body["latest"] = serde_json::json!("V.01-35");
        body["update_available"] = serde_json::json!(true);
        let old: OldClient = serde_json::from_value(body.clone())?;
        assert_eq!(old.name, "solidcraft-pdm");
        assert!(!old.version.is_empty());
        assert_eq!(old.build, body["build"].as_str().map(str::to_owned));
        assert_eq!(old.commit, body["commit"].as_str().map(str::to_owned));
        assert_eq!(old.built_at, body["built_at"].as_str().map(str::to_owned));
        let new: NewClient = serde_json::from_value(body)?;
        assert_eq!(new.latest.as_deref(), Some("V.01-35"));
        assert!(new.update_available);
        // Old wire shape: the new fields must have defaults.
        let legacy: NewClient = serde_json::from_value(version_info())?;
        assert_eq!(legacy.latest, None);
        assert!(!legacy.update_available);
        Ok(())
    }

    #[test]
    fn auto_labels_alpha_scheme() {
        assert_eq!(auto_revision_label(0), "A");
        assert_eq!(auto_revision_label(1), "B");
        assert_eq!(auto_revision_label(25), "Z");
        assert_eq!(auto_revision_label(26), "AA");
        assert_eq!(auto_revision_label(51), "AZ");
        assert_eq!(auto_revision_label(52), "BA");
    }
}
