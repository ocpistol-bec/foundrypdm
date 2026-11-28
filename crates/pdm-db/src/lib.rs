//! PDM database layer: migration runner and repository queries.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use chrono::{DateTime, Duration, Utc};
use pdm_core::*;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

pub mod migrate;
pub mod password;

pub use password::{hash_password, verify_password};

// ── Migrations ───────────────────────────────────────────────────────────

pub async fn migrate(pool: &PgPool) -> Result<(), PdmError> {
    migrate::run_migrations(pool).await
}

// ── Organizations ────────────────────────────────────────────────────────

pub async fn create_org(pool: &PgPool, name: &str) -> Result<Organization, PdmError> {
    let org = sqlx::query_as::<_, Organization>(
        "INSERT INTO organizations (name) VALUES ($1) RETURNING id, name, created_at",
    )
    .bind(name)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;
    // Orgs created after migration 0014 missed its one-time seed.
    seed_standard_lifecycle(pool, org.id).await?;
    Ok(org)
}

/// Seed the "Standard" lifecycle (wip -> in_review -> released -> obsolete) for one org.
/// Idempotent: safe to call where the migration's org-wide seed has not run for this org.
pub async fn seed_standard_lifecycle(pool: &PgPool, org_id: Uuid) -> Result<(), PdmError> {
    let def_id: Uuid = match sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO lifecycle_definitions (org_id, name)
         SELECT $1, 'Standard'
         WHERE NOT EXISTS (SELECT 1 FROM lifecycle_definitions d WHERE d.org_id = $1)
         RETURNING id",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?
    {
        Some(id) => id,
        None => sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM lifecycle_definitions WHERE org_id = $1 AND name = 'Standard'",
        )
        .bind(org_id)
        .fetch_one(pool)
        .await
        .map_err(PdmError::Db)?,
    };
    for (name, is_released, is_terminal) in [
        ("wip", false, false),
        ("in_review", false, false),
        ("released", true, false),
        ("obsolete", false, true),
    ] {
        sqlx::query(
            "INSERT INTO lifecycle_states (definition_id, name, is_released, is_terminal)
             SELECT $1, $2, $3, $4
             WHERE NOT EXISTS (
                 SELECT 1 FROM lifecycle_states st WHERE st.definition_id = $1 AND st.name = $2
             )",
        )
        .bind(def_id)
        .bind(name)
        .bind(is_released)
        .bind(is_terminal)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    }
    for (from, to) in [
        ("wip", "in_review"),
        ("in_review", "released"),
        ("released", "obsolete"),
    ] {
        sqlx::query(
            "INSERT INTO lifecycle_transitions (definition_id, from_state, to_state)
             SELECT d.id, fs.id, ts.id
             FROM lifecycle_definitions d
             JOIN lifecycle_states fs ON fs.definition_id = d.id AND fs.name = $1
             JOIN lifecycle_states ts ON ts.definition_id = d.id AND ts.name = $2
             WHERE d.org_id = $3
               AND NOT EXISTS (
                   SELECT 1 FROM lifecycle_transitions t
                   WHERE t.definition_id = d.id AND t.from_state = fs.id AND t.to_state = ts.id
               )",
        )
        .bind(from)
        .bind(to)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    }
    Ok(())
}

pub async fn get_org_by_name(pool: &PgPool, name: &str) -> Result<Option<Organization>, PdmError> {
    sqlx::query_as::<_, Organization>(
        "SELECT id, name, created_at FROM organizations WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_org_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Organization>, PdmError> {
    sqlx::query_as::<_, Organization>(
        "SELECT id, name, created_at FROM organizations WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub fn parse_gpu_policy(s: &str) -> Result<&'static str, PdmError> {
    match s.trim() {
        "auto" => Ok("auto"),
        "hardware" => Ok("hardware"),
        "software" => Ok("software"),
        "cpu" => Ok("cpu"),
        _ => Err(PdmError::BadRequest(
            "gpu_policy must be auto, hardware, software or cpu".into(),
        )),
    }
}

/// Preview renderer policy for this org (`auto` / `hardware` / `software` / `cpu`).
pub async fn get_gpu_policy(pool: &PgPool, org_id: Uuid) -> Result<String, PdmError> {
    let v: Option<String> = sqlx::query_scalar::<_, Option<String>>(
        "SELECT settings->>'gpu_policy' FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?
    .flatten();
    match v.as_deref().and_then(|s| parse_gpu_policy(s).ok()) {
        Some(p) => Ok(p.to_string()),
        None => Ok("auto".into()),
    }
}

pub fn parse_preview_style(style: &str) -> Result<&'static str, PdmError> {
    match style {
        "transparent" => Ok("transparent"),
        "viewport" => Ok("viewport"),
        _ => Err(PdmError::BadRequest(
            "preview_style must be transparent or viewport".into(),
        )),
    }
}

pub async fn get_preview_style(pool: &PgPool, org_id: Uuid) -> Result<String, PdmError> {
    let style: Option<String> =
        sqlx::query_scalar("SELECT settings->>'preview_style' FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(PdmError::Db)?
            .flatten();
    Ok(style
        .as_deref()
        .and_then(|s| parse_preview_style(s).ok())
        .unwrap_or("transparent")
        .to_string())
}

pub async fn set_preview_style(
    pool: &PgPool,
    org_id: Uuid,
    style: &str,
) -> Result<String, PdmError> {
    let style = parse_preview_style(style)?;
    sqlx::query("UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{preview_style}', to_jsonb($1::text), true) WHERE id = $2")
        .bind(style).bind(org_id).execute(pool).await.map_err(PdmError::Db)?;
    Ok(style.to_string())
}

// ── Organization default reading font ──────────────────────────────────

/// The org's default reading font (`settings->'reading_font'`); unset or unreadable faces read
/// as `system`.
pub async fn get_reading_font(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<pdm_core::reading_font::ReadingFont, PdmError> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT settings->'reading_font' FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(PdmError::Db)?
            .flatten();
    Ok(pdm_core::reading_font::ReadingFont::from_stored(v.as_ref()))
}

/// Store the org's default reading font (parsed by `ReadingFont::parse`); returns what reads back.
pub async fn set_reading_font(
    pool: &PgPool,
    org_id: Uuid,
    font: &pdm_core::reading_font::ReadingFont,
) -> Result<pdm_core::reading_font::ReadingFont, PdmError> {
    sqlx::query(
        "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{reading_font}', $1, true) WHERE id = $2",
    )
    .bind(font.to_json())
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    get_reading_font(pool, org_id).await
}

// ── Organization branding ──────────────────────────────────────────────

/// Stored branding as read back: an absent or unreadable object gives the default; entries that
/// no longer pass validation are dropped one by one (see `BrandingSettings::sanitized`).
fn parse_branding(v: Option<serde_json::Value>) -> pdm_core::branding::BrandingSettings {
    v.and_then(|v| serde_json::from_value::<pdm_core::branding::BrandingSettings>(v).ok())
        .map(pdm_core::branding::BrandingSettings::sanitized)
        .unwrap_or_default()
}

/// The org's branding (`settings->'branding'`).
pub async fn get_branding(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<pdm_core::branding::BrandingSettings, PdmError> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT settings->'branding' FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(PdmError::Db)?
            .flatten();
    Ok(parse_branding(v))
}

/// Apply a partial update to the org's branding (`BrandingSettings::apply_patch`) and store it,
/// holding the organization row locked from read to write so two admins' saves, or a save and
/// a reset, cannot overwrite each other. Returns the stored settings and what changed.
pub async fn update_branding(
    pool: &PgPool,
    org_id: Uuid,
    patch: &serde_json::Value,
) -> Result<
    (
        pdm_core::branding::BrandingSettings,
        pdm_core::branding::BrandingChange,
    ),
    PdmError,
> {
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;
    let row: Option<Option<serde_json::Value>> = sqlx::query_scalar(
        "SELECT settings->'branding' FROM organizations WHERE id = $1 FOR UPDATE",
    )
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    let Some(current) = row else {
        return Err(PdmError::NotFound("organization".into()));
    };
    let mut settings = parse_branding(current);
    let change = settings.apply_patch(patch)?;
    let json = serde_json::to_value(&settings).map_err(|e| PdmError::Internal(e.to_string()))?;
    sqlx::query(
        "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{branding}', $1, true) WHERE id = $2",
    )
    .bind(json)
    .bind(org_id)
    .execute(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    tx.commit().await.map_err(PdmError::Db)?;
    Ok((settings, change))
}

/// Store the org's branding (validated first; replaces the whole object). Returns what was
/// stored.
pub async fn set_branding(
    pool: &PgPool,
    org_id: Uuid,
    settings: &pdm_core::branding::BrandingSettings,
) -> Result<pdm_core::branding::BrandingSettings, PdmError> {
    let mut settings = settings.clone();
    settings.validate()?;
    let json = serde_json::to_value(&settings).map_err(|e| PdmError::Internal(e.to_string()))?;
    sqlx::query(
        "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{branding}', $1, true) WHERE id = $2",
    )
    .bind(json)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(settings)
}

/// Remove the org's branding: the settings and every branding image.
pub async fn clear_branding(pool: &PgPool, org_id: Uuid) -> Result<(), PdmError> {
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;
    sqlx::query("UPDATE organizations SET settings = settings - 'branding' WHERE id = $1")
        .bind(org_id)
        .execute(&mut *tx)
        .await
        .map_err(PdmError::Db)?;
    sqlx::query("DELETE FROM org_branding_assets WHERE org_id = $1")
        .bind(org_id)
        .execute(&mut *tx)
        .await
        .map_err(PdmError::Db)?;
    tx.commit().await.map_err(PdmError::Db)
}

/// A branding image's details without its bytes (`kind` is `logo` or `icon`).
pub async fn get_branding_asset_meta(
    pool: &PgPool,
    org_id: Uuid,
    kind: &str,
) -> Result<Option<pdm_core::branding::BrandingAssetMeta>, PdmError> {
    sqlx::query_as(
        "SELECT mime, sha256, width, height, octet_length(bytes) AS size_bytes, updated_at
         FROM org_branding_assets WHERE org_id = $1 AND kind = $2",
    )
    .bind(org_id)
    .bind(kind)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// A branding image with its bytes.
pub async fn get_branding_asset(
    pool: &PgPool,
    org_id: Uuid,
    kind: &str,
) -> Result<Option<pdm_core::branding::BrandingAsset>, PdmError> {
    sqlx::query_as(
        "SELECT mime, sha256, width, height, octet_length(bytes) AS size_bytes, updated_at, bytes
         FROM org_branding_assets WHERE org_id = $1 AND kind = $2",
    )
    .bind(org_id)
    .bind(kind)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// Store (or replace) a branding image. `info` comes from `pdm_core::branding::sniff_logo`.
pub async fn put_branding_asset(
    pool: &PgPool,
    org_id: Uuid,
    kind: &str,
    info: &pdm_core::branding::LogoInfo,
    bytes: &[u8],
    sha256: &str,
    updated_by: Option<Uuid>,
) -> Result<pdm_core::branding::BrandingAssetMeta, PdmError> {
    let width = i32::try_from(info.width).map_err(|e| PdmError::Validation(e.to_string()))?;
    let height = i32::try_from(info.height).map_err(|e| PdmError::Validation(e.to_string()))?;
    sqlx::query_as(
        "INSERT INTO org_branding_assets (org_id, kind, mime, bytes, sha256, width, height, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (org_id, kind) DO UPDATE SET
             mime = EXCLUDED.mime, bytes = EXCLUDED.bytes, sha256 = EXCLUDED.sha256,
             width = EXCLUDED.width, height = EXCLUDED.height,
             updated_at = now(), updated_by = EXCLUDED.updated_by
         RETURNING mime, sha256, width, height, octet_length(bytes) AS size_bytes, updated_at",
    )
    .bind(org_id)
    .bind(kind)
    .bind(info.mime)
    .bind(bytes)
    .bind(sha256)
    .bind(width)
    .bind(height)
    .bind(updated_by)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

/// Delete a branding image; `true` when there was one.
pub async fn delete_branding_asset(
    pool: &PgPool,
    org_id: Uuid,
    kind: &str,
) -> Result<bool, PdmError> {
    let done = sqlx::query("DELETE FROM org_branding_assets WHERE org_id = $1 AND kind = $2")
        .bind(org_id)
        .bind(kind)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    Ok(done.rows_affected() > 0)
}

/// The organization whose branding the sign-in page shows, before anyone has signed in:
/// - `name` given (the server's `PDM_SIGNIN_ORG`): the organization with that name, or none;
/// - otherwise the only organization, when the server hosts exactly one;
/// - otherwise none, so one tenant's name and logo are never shown to another's users.
pub async fn signin_org(pool: &PgPool, name: Option<&str>) -> Result<Option<Uuid>, PdmError> {
    if let Some(name) = name {
        return sqlx::query_scalar("SELECT id FROM organizations WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(PdmError::Db);
    }
    let ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM organizations ORDER BY created_at LIMIT 2")
            .fetch_all(pool)
            .await
            .map_err(PdmError::Db)?;
    Ok(match ids.as_slice() {
        [only] => Some(*only),
        _ => None,
    })
}

/// The org's Dutchman settings (release channel + conversion options), stored in
/// `organizations.settings`; absent or malformed values fall back to the defaults.
pub async fn get_dutchman_settings(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<pdm_core::DutchmanSettings, PdmError> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT settings->'dutchman' FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(PdmError::Db)?
            .flatten();
    let parsed = v
        .as_ref()
        .and_then(|v| serde_json::from_value::<pdm_core::DutchmanSettings>(v.clone()).ok());
    let settings = match parsed {
        Some(s) if s.validate().is_ok() => s,
        _ => pdm_core::DutchmanSettings::default(),
    };
    Ok(settings)
}

/// The org's saved laser-nest create-panel defaults, if any.
pub async fn get_org_nest_defaults(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Option<pdm_core::NestDefaults>, PdmError> {
    let v: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT settings->'nest_defaults' FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await
            .map_err(PdmError::Db)?
            .flatten();
    match v {
        Some(v) => Ok(serde_json::from_value(v).ok()),
        None => Ok(None),
    }
}

/// Store the org's laser-nest defaults (replaces the whole object).
pub async fn set_org_nest_defaults(
    pool: &PgPool,
    org_id: Uuid,
    defaults: &pdm_core::NestDefaults,
) -> Result<(), PdmError> {
    sqlx::query(
        "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{nest_defaults}', to_jsonb($1::jsonb), true) WHERE id = $2",
    )
    .bind(serde_json::to_value(defaults).map_err(|e| PdmError::Validation(e.to_string()))?)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

/// The Dutchman channel of the org created first (`None` when no org has a
/// channel set): the boot-time update loop has no org of its own, so it keeps
/// the oldest org's choice current on this host.
pub async fn first_org_dutchman_channel(pool: &PgPool) -> Option<String> {
    let v: Option<String> = sqlx::query_scalar(
        "SELECT settings->'dutchman'->>'channel' FROM organizations ORDER BY created_at LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()??;
    v
}

/// Store the org's Dutchman settings (validated; replaces the whole object).
pub async fn set_dutchman_settings(
    pool: &PgPool,
    org_id: Uuid,
    settings: &pdm_core::DutchmanSettings,
) -> Result<(), PdmError> {
    settings.validate()?;
    let json = serde_json::to_value(settings).map_err(|e| PdmError::Internal(e.to_string()))?;
    sqlx::query("UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{dutchman}', $1, true) WHERE id = $2")
        .bind(json)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    Ok(())
}

pub async fn get_mail_settings(pool: &PgPool, org_id: Uuid) -> Result<serde_json::Value, PdmError> {
    let v: Option<serde_json::Value> = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT settings->'mail' FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?
    .flatten();
    Ok(v.unwrap_or_else(|| serde_json::json!({})))
}

pub async fn get_onshape_settings(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<serde_json::Value, PdmError> {
    let v: Option<serde_json::Value> = sqlx::query_scalar::<_, Option<serde_json::Value>>(
        "SELECT settings->'onshape' FROM organizations WHERE id = $1",
    )
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?
    .flatten();
    Ok(v.unwrap_or_else(|| serde_json::json!({})))
}

pub async fn set_onshape_settings(
    pool: &PgPool,
    org_id: Uuid,
    value: &serde_json::Value,
) -> Result<(), PdmError> {
    sqlx::query(
        "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{onshape}', $1, true) WHERE id = $2",
    )
    .bind(value)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

pub async fn set_mail_settings(
    pool: &PgPool,
    org_id: Uuid,
    mail: &serde_json::Value,
) -> Result<(), PdmError> {
    sqlx::query(
        "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{mail}', $1, true) WHERE id = $2",
    )
    .bind(mail)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

pub async fn set_gpu_policy(pool: &PgPool, org_id: Uuid, policy: &str) -> Result<String, PdmError> {
    let policy = parse_gpu_policy(policy)?;
    sqlx::query(
        "UPDATE organizations SET settings = jsonb_set(COALESCE(settings, '{}'::jsonb), '{gpu_policy}', to_jsonb($1::text), true) WHERE id = $2",
    )
    .bind(policy)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(policy.to_string())
}

/// Read a boolean flag from the org's `settings` JSONB (missing → false).
pub async fn org_setting_bool(pool: &PgPool, org_id: Uuid, key: &str) -> Result<bool, PdmError> {
    // The org row always exists; the flag itself may be missing (NULL → false).
    let v: Option<bool> = sqlx::query_scalar::<_, Option<bool>>(
        "SELECT (settings->>$1)::bool FROM organizations WHERE id = $2",
    )
    .bind(key)
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(v.unwrap_or(false))
}

// ── Users ────────────────────────────────────────────────────────────────

pub async fn create_user(
    pool: &PgPool,
    org_id: Uuid,
    email: &str,
    display_name: &str,
    role: &str,
    password_hash: Option<&str>,
) -> Result<User, PdmError> {
    sqlx::query_as::<_, User>(
        "INSERT INTO users (org_id, email, display_name, role, password_hash)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING id, org_id, email, display_name, role, active, created_at, password_hash",
    )
    .bind(org_id)
    .bind(email)
    .bind(display_name)
    .bind(role)
    .bind(password_hash)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_user_by_email(
    pool: &PgPool,
    org_id: Uuid,
    email: &str,
) -> Result<Option<User>, PdmError> {
    sqlx::query_as::<_, User>(
        "SELECT id, org_id, email, display_name, role, active, created_at, password_hash
             FROM users WHERE org_id = $1 AND email = $2",
    )
    .bind(org_id)
    .bind(email)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_user_by_id(pool: &PgPool, id: Uuid) -> Result<Option<User>, PdmError> {
    sqlx::query_as::<_, User>(
        "SELECT id, org_id, email, display_name, role, active, created_at, password_hash
             FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn set_user_password(
    pool: &PgPool,
    user_id: Uuid,
    password_hash: &str,
) -> Result<(), PdmError> {
    sqlx::query("UPDATE users SET password_hash = $2 WHERE id = $1")
        .bind(user_id)
        .bind(password_hash)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    Ok(())
}

pub async fn update_user(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    display_name: &str,
    role: &str,
    active: bool,
) -> Result<User, PdmError> {
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;
    // Serialize account changes within an org so concurrent requests cannot
    // disable/demote its last two administrators at the same time.
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM organizations WHERE id = $1 FOR UPDATE")
        .bind(org_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(PdmError::Db)?
        .ok_or_else(|| PdmError::NotFound(format!("organization {org_id}")))?;
    let target = sqlx::query_as::<_, User>(
        "SELECT id, org_id, email, display_name, role, active, created_at, password_hash
         FROM users WHERE id = $1 AND org_id = $2",
    )
    .bind(user_id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(PdmError::Db)?
    .ok_or_else(|| PdmError::NotFound(format!("user {user_id}")))?;
    if target.active && target.role == "admin" && (!active || role != "admin") {
        let other_admin = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM users
             WHERE org_id = $1 AND id <> $2 AND active AND role = 'admin')",
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(PdmError::Db)?;
        if !other_admin {
            return Err(PdmError::Conflict(
                "cannot disable or demote the last active administrator".into(),
            ));
        }
    }
    let user = sqlx::query_as::<_, User>(
        "UPDATE users SET display_name = $2, role = $3, active = $4 WHERE id = $1 AND org_id = $5
         RETURNING id, org_id, email, display_name, role, active, created_at, password_hash",
    )
    .bind(user_id)
    .bind(display_name)
    .bind(role)
    .bind(active)
    .bind(org_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    tx.commit().await.map_err(PdmError::Db)?;
    Ok(user)
}

/// All users in an organization, for the workspace UI (member lists, name
/// resolution, administration).
pub async fn list_users(pool: &PgPool, org_id: Uuid) -> Result<Vec<User>, PdmError> {
    sqlx::query_as::<_, User>(
        "SELECT id, org_id, email, display_name, role, active, created_at, password_hash
             FROM users WHERE org_id = $1 ORDER BY created_at",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

/// All users with this email across every organization (email is only unique per org).
pub async fn get_users_by_email(pool: &PgPool, email: &str) -> Result<Vec<User>, PdmError> {
    sqlx::query_as::<_, User>(
        "SELECT id, org_id, email, display_name, role, active, created_at, password_hash
             FROM users WHERE email = $1",
    )
    .bind(email)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

// ── API Tokens ───────────────────────────────────────────────────────────

/// Create an API token (`kind = 'api'`): never ends unless `expires_at` is set or it is deleted.
pub async fn create_token(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Option<Uuid>,
    name: &str,
    token_hash: &str,
    scopes: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<ApiToken, PdmError> {
    sqlx::query_as::<_, ApiToken>(
        "INSERT INTO api_tokens (org_id, user_id, name, token_hash, scopes, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, org_id, user_id, name, scopes, expires_at, created_at",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(name)
    .bind(token_hash)
    .bind(scopes)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

/// Create a web sign-in token (`kind = 'web'`). Only these end on their own; see
/// [`session_expired`]. Every other token is made by [`create_token`] as `kind = 'api'`.
pub async fn create_session_token(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    token_hash: &str,
) -> Result<ApiToken, PdmError> {
    sqlx::query_as::<_, ApiToken>(
        "INSERT INTO api_tokens (org_id, user_id, name, token_hash, scopes, kind)
         VALUES ($1, $2, 'web-login', $3, 'full', 'web')
         RETURNING id, org_id, user_id, name, scopes, expires_at, created_at",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(token_hash)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

/// Whether a token is a web sign-in that has ended, by going `idle_minutes` without a request
/// or by being older than `lifetime_minutes` (0 turns a limit off). Always `false` for an API
/// token. `None` when the token no longer exists (deleted since it was looked up).
pub async fn session_expired(
    pool: &PgPool,
    token_id: Uuid,
    idle_minutes: i32,
    lifetime_minutes: i32,
) -> Result<Option<bool>, PdmError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT kind = 'web'
                AND (($2::int > 0
                      AND COALESCE(last_used_at, created_at)
                          < now() - make_interval(mins => $2::int))
                  OR ($3::int > 0 AND created_at < now() - make_interval(mins => $3::int)))
           FROM api_tokens WHERE id = $1",
    )
    .bind(token_id)
    .bind(idle_minutes)
    .bind(lifetime_minutes)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_token_by_hash(
    pool: &PgPool,
    hash: &str,
) -> Result<Option<(ApiToken, Option<User>)>, PdmError> {
    let token: Option<ApiToken> = sqlx::query_as::<_, ApiToken>(
        "SELECT id, org_id, user_id, name, scopes, expires_at, created_at
         FROM api_tokens WHERE token_hash = $1",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?;

    let Some(t) = token else { return Ok(None) };
    let user = if let Some(uid) = t.user_id {
        get_user_by_id(pool, uid).await?
    } else {
        None
    };
    Ok(Some((t, user)))
}

pub async fn delete_token(pool: &PgPool, id: Uuid) -> Result<(), PdmError> {
    sqlx::query("DELETE FROM api_tokens WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    Ok(())
}

pub async fn touch_token(pool: &PgPool, id: Uuid) -> Result<(), PdmError> {
    sqlx::query("UPDATE api_tokens SET last_used_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    Ok(())
}

// ── Projects ─────────────────────────────────────────────────────────────

pub async fn create_project(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    description: &str,
) -> Result<Project, PdmError> {
    sqlx::query_as::<_, Project>(
        "INSERT INTO projects (org_id, name, description)
         VALUES ($1, $2, $3)
         RETURNING id, org_id, name, description, created_at, archived_at, image_data, main_assembly_id",
    )
    .bind(org_id)
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

/// List a project for an org. Archived projects are hidden unless
/// `include_archived` is true.
pub async fn list_projects(
    pool: &PgPool,
    org_id: Uuid,
    include_archived: bool,
) -> Result<Vec<Project>, PdmError> {
    let archived = if include_archived {
        ""
    } else {
        "AND archived_at IS NULL"
    };
    let sql = format!(
        "SELECT id, org_id, name, description, created_at, archived_at, image_data, main_assembly_id
             FROM projects WHERE org_id = $1 {archived} ORDER BY created_at"
    );
    sqlx::query_as::<_, Project>(&sql)
        .bind(org_id)
        .fetch_all(pool)
        .await
        .map_err(PdmError::Db)
}

pub async fn get_project(
    pool: &PgPool,
    id: Uuid,
    org_id: Uuid,
) -> Result<Option<Project>, PdmError> {
    sqlx::query_as::<_, Project>(
        "SELECT id, org_id, name, description, created_at, archived_at, image_data, main_assembly_id
             FROM projects WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

const MAX_PROJECT_IMAGE: usize = 400_000;

fn check_project_image(image_data: Option<&str>) -> Result<(), PdmError> {
    let Some(img) = image_data else {
        return Ok(());
    };
    if img.is_empty() {
        return Ok(());
    }
    if !img.starts_with("data:image/") {
        return Err(PdmError::BadRequest(
            "image_data must be a data:image URL".into(),
        ));
    }
    if img.len() > MAX_PROJECT_IMAGE {
        return Err(PdmError::BadRequest("image is too large".into()));
    }
    Ok(())
}

/// Update a project's name, description and optional cover image.
/// `image_data` of `None` leaves the current image; `Some("")` clears it.
pub async fn update_project(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    name: &str,
    description: &str,
    image_data: Option<&str>,
) -> Result<Project, PdmError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(PdmError::BadRequest("name is required".into()));
    }
    check_project_image(image_data)?;
    let res = if let Some(img) = image_data {
        let img = if img.is_empty() { None } else { Some(img) };
        sqlx::query("UPDATE projects SET name = $3, description = $4, image_data = $5 WHERE id = $1 AND org_id = $2")
            .bind(id)
            .bind(org_id)
            .bind(name)
            .bind(description)
            .bind(img)
            .execute(pool)
            .await
            .map_err(PdmError::Db)?
    } else {
        sqlx::query("UPDATE projects SET name = $3, description = $4 WHERE id = $1 AND org_id = $2")
            .bind(id)
            .bind(org_id)
            .bind(name)
            .bind(description)
            .execute(pool)
            .await
            .map_err(PdmError::Db)?
    };
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("project {id}")));
    }
    get_project(pool, id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("project {id}")))
}

/// Set (or clear) a project's main assembly — the root item the bill of
/// materials is built off, and the source of the project's cover image.
/// An `item_id` of `None` clears it. The item must belong to the project.
pub async fn set_main_assembly(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Uuid,
    item_id: Option<Uuid>,
) -> Result<Project, PdmError> {
    get_project(pool, project_id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("project {project_id}")))?;
    if let Some(item_id) = item_id {
        let item = get_item(pool, item_id, org_id)
            .await?
            .ok_or_else(|| PdmError::NotFound(format!("item {item_id}")))?;
        if item.project_id != project_id {
            return Err(PdmError::BadRequest(
                "main assembly must belong to this project".into(),
            ));
        }
    }
    let res =
        sqlx::query("UPDATE projects SET main_assembly_id = $3 WHERE id = $1 AND org_id = $2")
            .bind(project_id)
            .bind(org_id)
            .bind(item_id)
            .execute(pool)
            .await
            .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("project {project_id}")));
    }
    get_project(pool, project_id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("project {project_id}")))
}

/// Soft-delete a project (archive): mark `archived_at = now()`.
pub async fn archive_project(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Project, PdmError> {
    let res = sqlx::query("UPDATE projects SET archived_at = now() WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("project {id}")));
    }
    get_project(pool, id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("project {id}")))
}

/// Undo a soft-delete (unarchive): clear `archived_at`.
pub async fn unarchive_project(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Project, PdmError> {
    let res = sqlx::query("UPDATE projects SET archived_at = NULL WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("project {id}")));
    }
    get_project(pool, id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("project {id}")))
}

/// Hard-delete a project. Child folders/items cascade via the foreign keys.
pub async fn delete_project(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Project, PdmError> {
    let project = get_project(pool, id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("project {id}")))?;
    let res = sqlx::query("DELETE FROM projects WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("project {id}")));
    }
    Ok(project)
}

// ── Folders ──────────────────────────────────────────────────────────────

pub async fn create_folder(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Uuid,
    parent_id: Option<Uuid>,
    name: &str,
) -> Result<Folder, PdmError> {
    sqlx::query_as::<_, Folder>(
        "INSERT INTO folders (org_id, project_id, parent_id, name)
         VALUES ($1, $2, $3, $4)
         RETURNING id, org_id, project_id, parent_id, name",
    )
    .bind(org_id)
    .bind(project_id)
    .bind(parent_id)
    .bind(name)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

/// Whether a folder is in the given project of the given organization.
pub async fn folder_in_project(
    pool: &PgPool,
    folder_id: Uuid,
    org_id: Uuid,
    project_id: Uuid,
) -> Result<bool, PdmError> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM folders WHERE id = $1 AND org_id = $2 AND project_id = $3)",
    )
    .bind(folder_id)
    .bind(org_id)
    .bind(project_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_folders(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Uuid,
) -> Result<Vec<Folder>, PdmError> {
    sqlx::query_as::<_, Folder>(
        "SELECT id, org_id, project_id, parent_id, name
             FROM folders WHERE org_id = $1 AND project_id = $2 ORDER BY name",
    )
    .bind(org_id)
    .bind(project_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

// ── Items ────────────────────────────────────────────────────────────────

// ── Pagination ───────────────────────────────────────────────────────────

/// Page/limit request params shared by the listing endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginationParams {
    pub page: Option<u32>,
    pub limit: Option<u32>,
}

/// Normalize `page`/`limit` into sane defaults (page 1, limit 50, cap 200).
pub fn pagination(page: Option<u32>, limit: Option<u32>) -> (u32, u32) {
    let page = page.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(50).clamp(1, 200);
    (page, limit)
}

/// A page of items with enough metadata for a load-more UI.
#[derive(Debug, Clone, Serialize)]
pub struct Paginated<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub limit: u32,
    pub total: i64,
    pub has_more: bool,
}

/// Filename globs used by the `kind` filter, matched ILIKE against
/// `files.filename`. MIME type is less reliable since CAD check-ins are often
/// `application/octet-stream`.
fn kind_filename_patterns(kind: &str) -> Option<Vec<&'static str>> {
    Some(match kind {
        "cad" => vec![
            "%.step",
            "%.stp",
            "%.igs",
            "%.iges",
            "%.ipt",
            "%.iam",
            "%.fcstd",
            "%.3dm",
            "%.solvecraft",
            "%.solidcraft",
            "%.sldprt",
            "%.sldasm",
            "%.dwg",
            "%.x_t",
            "%.sat",
        ],
        "img" => vec![
            "%.png", "%.jpg", "%.jpeg", "%.gif", "%.webp", "%.bmp", "%.tiff", "%.svg",
        ],
        "pdf" => vec!["%.pdf"],
        "dxf" => vec!["%.dxf"],
        "txt" => vec!["%.txt", "%.md", "%.csv", "%.json", "%.xml"],
        _ => return None,
    })
}

/// List an org's items with server-side filters and pagination.
/// `project_id: None` means org-wide (replaces the per-project fan-out).
///
/// Filters: `classification`, `state` (lifecycle_state), `kind` (filename
/// glob), `owner`, `folder`, `q` (full-text + part number + description),
/// `sheet_metal` (only items with a sheet-metal spec / all other items).
/// `sort` may be one of part_number/created_at/modified_at/classification;
/// `dir` is asc|desc.
#[allow(clippy::too_many_arguments)]
pub async fn list_items_paginated(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Option<Uuid>,
    include_archived: bool,
    classification: Option<&str>,
    state: Option<&str>,
    kind: Option<&str>,
    owner: Option<Uuid>,
    folder: Option<Uuid>,
    q: Option<&str>,
    sheet_metal: Option<bool>,
    sort: Option<&str>,
    dir: Option<&str>,
    page: u32,
    limit: u32,
) -> Result<Paginated<ItemSummary>, PdmError> {
    let offset = (page - 1) * limit;

    let sort_col = match sort.unwrap_or("part_number") {
        "created_at" => "i.created_at",
        "modified_at" => "hv.committed_at",
        "classification" => "i.classification",
        _ => "i.part_number",
    };
    let sort_dir = if dir.unwrap_or("asc").eq_ignore_ascii_case("desc") {
        "DESC"
    } else {
        "ASC"
    };

    // Build the WHERE clause, binding values in placeholder order ($1 = org_id).
    let mut where_clauses: Vec<String> = vec!["i.org_id = $1".to_string()];
    let mut param: i32 = 2;

    if project_id.is_some() {
        where_clauses.push(format!("AND i.project_id = ${param}"));
        param += 1;
    }
    if !include_archived {
        where_clauses.push("AND i.archived_at IS NULL".to_string());
    }
    if classification.is_some() {
        where_clauses.push(format!("AND i.classification ILIKE ${param}"));
        param += 1;
    }
    if state.is_some() {
        where_clauses.push(format!("AND i.lifecycle_state = ${param}"));
        param += 1;
    }
    if owner.is_some() {
        where_clauses.push(format!("AND i.owner_id = ${param}"));
        param += 1;
    }
    if folder.is_some() {
        where_clauses.push(format!("AND i.folder_id = ${param}"));
        param += 1;
    }
    if let Some(patterns) = kind.and_then(kind_filename_patterns) {
        let mut preds: Vec<String> = Vec::with_capacity(patterns.len());
        for _ in patterns {
            preds.push(format!("f.filename ILIKE ${param}"));
            param += 1;
        }
        where_clauses.push(format!(
            "AND EXISTS (SELECT 1 FROM files f WHERE f.item_id = i.id AND {})",
            preds.join(" OR ")
        ));
    }
    if q.is_some() {
        where_clauses.push(format!(
            "AND (i.search_tsv @@ plainto_tsquery('english', ${param}) \
             OR i.part_number ILIKE '%' || ${param} || '%' \
             OR i.description ILIKE '%' || ${param} || '%')"
        ));
        param += 1;
    }
    if let Some(sm) = sheet_metal {
        where_clauses.push(if sm {
            "AND EXISTS (SELECT 1 FROM sheet_metal_specs s WHERE s.item_id = i.id AND s.org_id = i.org_id)".into()
        } else {
            "AND NOT EXISTS (SELECT 1 FROM sheet_metal_specs s WHERE s.item_id = i.id AND s.org_id = i.org_id)".into()
        });
    }

    let where_sql = where_clauses.join(" ");

    // FROM fragment reused for the count query (split at the first "\nFROM").
    let from_part = ITEM_SUMMARY_COLUMNS
        .split_once("FROM items i")
        .map(|(_, rest)| format!("FROM items i{rest}"))
        .unwrap_or_else(|| ITEM_SUMMARY_COLUMNS.to_string());

    let count_sql = format!("SELECT count(*)::bigint {from_part} WHERE {where_sql}");
    let mut total_q = sqlx::query_scalar::<_, i64>(&count_sql);
    total_q = total_q.bind(org_id);
    if let Some(pid) = project_id {
        total_q = total_q.bind(pid);
    }
    if let Some(classification) = classification {
        total_q = total_q.bind(classification);
    }
    if let Some(state) = state {
        total_q = total_q.bind(state);
    }
    if let Some(owner) = owner {
        total_q = total_q.bind(owner);
    }
    if let Some(folder) = folder {
        total_q = total_q.bind(folder);
    }
    if let Some(patterns) = kind.and_then(kind_filename_patterns) {
        for pat in patterns {
            total_q = total_q.bind(pat);
        }
    }
    if let Some(query_text) = q {
        total_q = total_q.bind(query_text);
    }
    let total: i64 = total_q.fetch_one(pool).await.map_err(PdmError::Db)?;

    let data_sql = format!(
        "{ITEM_SUMMARY_COLUMNS} WHERE {where_sql} ORDER BY {sort_col} {sort_dir} \
         LIMIT ${param} OFFSET ${next}",
        next = param + 1
    );
    let mut query = sqlx::query_as::<_, ItemSummary>(&data_sql);
    query = query.bind(org_id);
    if let Some(pid) = project_id {
        query = query.bind(pid);
    }
    if let Some(classification) = classification {
        query = query.bind(classification);
    }
    if let Some(state) = state {
        query = query.bind(state);
    }
    if let Some(owner) = owner {
        query = query.bind(owner);
    }
    if let Some(folder) = folder {
        query = query.bind(folder);
    }
    if let Some(patterns) = kind.and_then(kind_filename_patterns) {
        for pat in patterns {
            query = query.bind(pat);
        }
    }
    if let Some(query_text) = q {
        query = query.bind(query_text);
    }
    let items = query
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(pool)
        .await
        .map_err(PdmError::Db)?;
    let has_more = i64::from(offset) + i64::try_from(items.len()).unwrap_or(i64::MAX) < total;

    Ok(Paginated {
        items,
        page,
        limit,
        total,
        has_more,
    })
}

/// List an org's items for a project. Archived items are hidden unless
/// `include_archived` is true.
pub async fn list_items(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Uuid,
    include_archived: bool,
) -> Result<Vec<ItemSummary>, PdmError> {
    let paginated = list_items_paginated(
        pool,
        org_id,
        Some(project_id),
        include_archived,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        1,
        500,
    )
    .await?;
    Ok(paginated.items)
}

/// Paginated search over items, reusing the listing's filters plus full-text `q`.
#[allow(clippy::too_many_arguments)]
pub async fn search_items_paginated(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Option<Uuid>,
    query: &str,
    include_archived: bool,
    classification: Option<&str>,
    state: Option<&str>,
    kind: Option<&str>,
    owner: Option<Uuid>,
    folder: Option<Uuid>,
    sheet_metal: Option<bool>,
    sort: Option<&str>,
    dir: Option<&str>,
    page: u32,
    limit: u32,
) -> Result<Paginated<ItemSummary>, PdmError> {
    list_items_paginated(
        pool,
        org_id,
        project_id,
        include_archived,
        classification,
        state,
        kind,
        owner,
        folder,
        Some(query),
        sheet_metal,
        sort,
        dir,
        page,
        limit,
    )
    .await
}

// ── Organization checks on the ids a write names ───────────────────────

/// A row that a new record points at. The foreign keys accept any organization's ids, so a
/// write checks each one against the writer's organization first.
#[derive(Debug, Clone, Copy)]
pub enum OrgRef {
    Project(Uuid),
    Item(Uuid),
    /// A version of this item.
    ItemVersion {
        item_id: Uuid,
        version_id: Uuid,
    },
    Machine(Uuid),
    PostProcessor(Uuid),
    Tool(Uuid),
    MfgSetup(Uuid),
    CamProject(Uuid),
    NcProgram(Uuid),
    ProductionJob(Uuid),
}

impl OrgRef {
    fn describe(&self) -> String {
        match self {
            OrgRef::Project(id) => format!("project {id}"),
            OrgRef::Item(id) => format!("item {id}"),
            OrgRef::ItemVersion {
                item_id,
                version_id,
            } => format!("version {version_id} of item {item_id}"),
            OrgRef::Machine(id) => format!("machine {id}"),
            OrgRef::PostProcessor(id) => format!("post-processor {id}"),
            OrgRef::Tool(id) => format!("tool {id}"),
            OrgRef::MfgSetup(id) => format!("manufacturing setup {id}"),
            OrgRef::CamProject(id) => format!("CAM project {id}"),
            OrgRef::NcProgram(id) => format!("NC program {id}"),
            OrgRef::ProductionJob(id) => format!("production job {id}"),
        }
    }
}

/// NotFound for the first reference that is not the organization's, so another organization's
/// ids look the same as ids that do not exist.
pub async fn require_org_refs(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    refs: &[OrgRef],
) -> Result<(), PdmError> {
    for r in refs {
        let (sql, id, item) = match *r {
            OrgRef::ItemVersion {
                item_id,
                version_id,
            } => (
                "SELECT EXISTS (SELECT 1 FROM file_versions v JOIN files f ON f.id = v.file_id
                   JOIN items i ON i.id = f.item_id
                  WHERE v.id = $1 AND i.org_id = $2 AND i.id = $3)",
                version_id,
                Some(item_id),
            ),
            OrgRef::Project(id) => (
                "SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
            OrgRef::Item(id) => (
                "SELECT EXISTS (SELECT 1 FROM items WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
            OrgRef::Machine(id) => (
                "SELECT EXISTS (SELECT 1 FROM machines WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
            OrgRef::PostProcessor(id) => (
                "SELECT EXISTS (SELECT 1 FROM post_processors WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
            OrgRef::Tool(id) => (
                "SELECT EXISTS (SELECT 1 FROM tools WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
            OrgRef::MfgSetup(id) => (
                "SELECT EXISTS (SELECT 1 FROM mfg_setups WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
            OrgRef::CamProject(id) => (
                "SELECT EXISTS (SELECT 1 FROM cam_projects WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
            OrgRef::NcProgram(id) => (
                "SELECT EXISTS (SELECT 1 FROM nc_programs WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
            OrgRef::ProductionJob(id) => (
                "SELECT EXISTS (SELECT 1 FROM production_jobs WHERE id = $1 AND org_id = $2)",
                id,
                None,
            ),
        };
        let mut q = sqlx::query_scalar::<_, bool>(sql).bind(id).bind(org_id);
        if let Some(item_id) = item {
            q = q.bind(item_id);
        }
        if !q.fetch_one(&mut *conn).await.map_err(PdmError::Db)? {
            return Err(PdmError::NotFound(r.describe()));
        }
    }
    Ok(())
}

// ── Items (continued) ──────────────────────────────────────────────────

/// Create an item. NotFound unless the project is the organization's and, when one is given,
/// the folder is in that project.
#[allow(clippy::too_many_arguments)]
pub async fn create_item(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Uuid,
    folder_id: Option<Uuid>,
    part_number: &str,
    description: &str,
    classification: &str,
    owner_id: Option<Uuid>,
) -> Result<Item, PdmError> {
    sqlx::query_as::<_, Item>(
        "INSERT INTO items (org_id, project_id, folder_id, part_number, description, classification, owner_id)
         SELECT $1, $2, $3, $4, $5, $6, $7
          WHERE EXISTS (SELECT 1 FROM projects WHERE id = $2 AND org_id = $1)
            AND ($3::uuid IS NULL
                 OR EXISTS (SELECT 1 FROM folders WHERE id = $3 AND org_id = $1 AND project_id = $2))
         RETURNING id, org_id, project_id, folder_id, part_number, description, classification, doc_id,
                   lifecycle_state, owner_id, created_at, updated_at, archived_at",
    )
    .bind(org_id)
    .bind(project_id)
    .bind(folder_id)
    .bind(part_number)
    .bind(description)
    .bind(classification)
    .bind(owner_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") {
            PdmError::Conflict(format!("part number {part_number} already exists"))
        } else {
            PdmError::Db(e)
        }
    })?
    .ok_or_else(|| match folder_id {
        Some(f) => PdmError::NotFound(format!("folder {f} in project {project_id}")),
        None => PdmError::NotFound(format!("project {project_id}")),
    })
}

/// The PDM item last sent from this Onshape document + tab, if any.
pub async fn find_item_by_onshape(
    pool: &PgPool,
    org_id: Uuid,
    document_id: &str,
    element_id: &str,
    part_id: Option<&str>,
) -> Result<Option<Item>, PdmError> {
    if let Some(pid) = part_id.map(str::trim).filter(|s| !s.is_empty()) {
        return sqlx::query_as::<_, Item>(
            "SELECT i.id, i.org_id, i.project_id, i.folder_id, i.part_number, i.description,
                    i.classification, i.doc_id, i.lifecycle_state, i.owner_id, i.created_at,
                    i.updated_at, i.archived_at
             FROM items i
             INNER JOIN properties p1 ON p1.item_id = i.id AND p1.prop_key = 'onshape.document_id'
             INNER JOIN properties p2 ON p2.item_id = i.id AND p2.prop_key = 'onshape.element_id'
             INNER JOIN properties p3 ON p3.item_id = i.id AND p3.prop_key = 'onshape.part_id'
             WHERE i.org_id = $1 AND i.archived_at IS NULL
               AND p1.value #>> '{}' = $2
               AND p2.value #>> '{}' = $3
               AND p3.value #>> '{}' = $4
             ORDER BY i.updated_at DESC
             LIMIT 1",
        )
        .bind(org_id)
        .bind(document_id)
        .bind(element_id)
        .bind(pid)
        .fetch_optional(pool)
        .await
        .map_err(PdmError::Db);
    }
    sqlx::query_as::<_, Item>(
        "SELECT i.id, i.org_id, i.project_id, i.folder_id, i.part_number, i.description,
                i.classification, i.doc_id, i.lifecycle_state, i.owner_id, i.created_at,
                i.updated_at, i.archived_at
         FROM items i
         INNER JOIN properties p1 ON p1.item_id = i.id AND p1.prop_key = 'onshape.document_id'
         INNER JOIN properties p2 ON p2.item_id = i.id AND p2.prop_key = 'onshape.element_id'
         LEFT JOIN properties p3 ON p3.item_id = i.id AND p3.prop_key = 'onshape.part_id'
         WHERE i.org_id = $1 AND i.archived_at IS NULL
           AND p1.value #>> '{}' = $2
           AND p2.value #>> '{}' = $3
           AND COALESCE(p3.value #>> '{}', '') = ''
         ORDER BY i.updated_at DESC
         LIMIT 1",
    )
    .bind(org_id)
    .bind(document_id)
    .bind(element_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_item(pool: &PgPool, id: Uuid, org_id: Uuid) -> Result<Option<Item>, PdmError> {
    sqlx::query_as::<_, Item>(
        "SELECT id, org_id, project_id, folder_id, part_number, description, classification,
                    doc_id, lifecycle_state, owner_id, created_at, updated_at, archived_at
             FROM items WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// Summary SELECT shared by the item listing endpoints: item columns plus the
/// owner's name, the head version's commit time/author, the highest revision
/// label, the live lock holder, and the head version seq / version / revision.
const ITEM_SUMMARY_COLUMNS: &str = "SELECT i.id, i.org_id, i.project_id, i.folder_id, i.part_number,
               i.description, i.classification, i.doc_id, i.lifecycle_state, i.owner_id,
               i.created_at, i.updated_at, i.archived_at,
               COALESCE(NULLIF(ou.display_name, ''), ou.email) AS owner_name,
               hv.committed_at AS modified_at,
               COALESCE(NULLIF(mu.display_name, ''), mu.email) AS modified_by,
               (SELECT MAX(fr.label) FROM file_revisions fr WHERE fr.item_id = i.id) AS latest_revision,
                COALESCE(NULLIF(lk.display_name, ''), lk.email) AS locked_by,
                hv.seq AS head_version_seq,
                hv.version AS head_version,
                hv.revision AS head_revision,
                (SELECT f.filename FROM files f WHERE f.item_id = i.id LIMIT 1) AS filename,
                (SELECT f.mime_type FROM files f WHERE f.item_id = i.id LIMIT 1) AS mime_type,
                (SELECT EXISTS (SELECT 1 FROM sheet_metal_specs sm WHERE sm.item_id = i.id AND sm.org_id = i.org_id)) AS sheet_metal,
                (SELECT sm.material FROM sheet_metal_specs sm WHERE sm.item_id = i.id AND sm.org_id = i.org_id
                 ORDER BY sm.created_at DESC LIMIT 1) AS sheet_metal_material,
                (SELECT sm.thickness_mm FROM sheet_metal_specs sm WHERE sm.item_id = i.id AND sm.org_id = i.org_id
                 ORDER BY sm.created_at DESC LIMIT 1) AS sheet_metal_thickness_mm,
                (SELECT sm.flat_pattern_sha IS NOT NULL FROM sheet_metal_specs sm
                 WHERE sm.item_id = i.id AND sm.org_id = i.org_id ORDER BY sm.created_at DESC LIMIT 1) AS sheet_metal_flat_pattern
         FROM items i
        LEFT JOIN users ou ON ou.id = i.owner_id
         LEFT JOIN LATERAL (
             SELECT v.seq, v.version, v.revision, v.committed_at, v.author_id
             FROM file_versions v
            WHERE v.file_id = (SELECT f.id FROM files f WHERE f.item_id = i.id LIMIT 1)
            ORDER BY v.seq DESC
            LIMIT 1
        ) hv ON TRUE
        LEFT JOIN users mu ON mu.id = hv.author_id
        LEFT JOIN file_locks fl ON fl.item_id = i.id AND fl.lease_expires_at > now()
        LEFT JOIN users lk ON lk.id = fl.user_id";

pub async fn get_item_summary(
    pool: &PgPool,
    id: Uuid,
    org_id: Uuid,
) -> Result<Option<ItemSummary>, PdmError> {
    let sql = format!("{ITEM_SUMMARY_COLUMNS} WHERE i.id = $1 AND i.org_id = $2");
    sqlx::query_as::<_, ItemSummary>(&sql)
        .bind(id)
        .bind(org_id)
        .fetch_optional(pool)
        .await
        .map_err(PdmError::Db)
}

pub async fn search_items(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Option<Uuid>,
    query: &str,
    include_archived: bool,
) -> Result<Vec<ItemSummary>, PdmError> {
    let archived = if include_archived {
        ""
    } else {
        "AND i.archived_at IS NULL"
    };
    let sql = format!(
        "{ITEM_SUMMARY_COLUMNS}
         WHERE i.org_id = $1 AND (
             i.search_tsv @@ plainto_tsquery('english', $2)
             OR i.part_number ILIKE '%' || $2 || '%'
             OR i.description ILIKE '%' || $2 || '%'
         )
         {archived}
         {}
         ORDER BY i.part_number
         LIMIT 100",
        if project_id.is_some() {
            "AND i.project_id = $3"
        } else {
            ""
        }
    );

    let mut q = sqlx::query_as::<_, ItemSummary>(&sql)
        .bind(org_id)
        .bind(query);
    if let Some(pid) = project_id {
        q = q.bind(pid);
    }
    q.fetch_all(pool).await.map_err(PdmError::Db)
}

/// Atomically get and increment the next part-number sequence for a prefix.
pub async fn next_part_number(pool: &PgPool, org_id: Uuid, prefix: &str) -> Result<i64, PdmError> {
    let seq = sqlx::query_scalar::<_, i64>(
        "INSERT INTO number_sequences (org_id, prefix, next_value)
         VALUES ($1, $2, 1)
         ON CONFLICT (org_id, prefix)
         DO UPDATE SET next_value = number_sequences.next_value + 1
         RETURNING next_value",
    )
    .bind(org_id)
    .bind(prefix)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(seq)
}

/// Soft-delete an item (archive): mark `archived_at = now()`.
pub async fn archive_item(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Item, PdmError> {
    let res = sqlx::query("UPDATE items SET archived_at = now() WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("item {id}")));
    }
    get_item(pool, id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("item {id}")))
}

/// Undo a soft-delete (unarchive): clear `archived_at`.
pub async fn unarchive_item(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Item, PdmError> {
    let res = sqlx::query("UPDATE items SET archived_at = NULL WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("item {id}")));
    }
    get_item(pool, id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("item {id}")))
}

/// Hard-delete an item. Child files/versions/locks/etc. cascade via the foreign
/// keys. Refuses (Conflict) when the item still holds a live checkout lock.
pub async fn delete_item(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Item, PdmError> {
    let locked: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM file_locks WHERE item_id = $1 AND lease_expires_at > now())",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;
    if locked {
        return Err(PdmError::Conflict(
            "item has an active checkout lock".into(),
        ));
    }
    let item = get_item(pool, id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("item {id}")))?;
    let res = sqlx::query("DELETE FROM items WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("item {id}")));
    }
    Ok(item)
}

// ── Lifecycle state machine ──────────────────────────────────────────────

pub async fn list_lifecycle_definitions(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<pdm_core::LifecycleDefinition>, PdmError> {
    sqlx::query_as::<_, pdm_core::LifecycleDefinition>(
        "SELECT id, org_id, name FROM lifecycle_definitions WHERE org_id = $1 ORDER BY name",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_lifecycle_states(
    pool: &PgPool,
    definition_id: Uuid,
) -> Result<Vec<pdm_core::LifecycleState>, PdmError> {
    sqlx::query_as::<_, pdm_core::LifecycleState>(
        "SELECT id, definition_id, name, is_released, is_terminal
             FROM lifecycle_states WHERE definition_id = $1 ORDER BY name",
    )
    .bind(definition_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_lifecycle_transitions(
    pool: &PgPool,
    definition_id: Uuid,
) -> Result<Vec<pdm_core::LifecycleTransition>, PdmError> {
    sqlx::query_as::<_, pdm_core::LifecycleTransition>(
        "SELECT id, definition_id, from_state, to_state
             FROM lifecycle_transitions WHERE definition_id = $1 ORDER BY id",
    )
    .bind(definition_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

/// True when `from_state -> to_state` is a defined transition of the org's
/// lifecycle definition. State names are matched literally (items carry their
/// denormalized state name in `items.lifecycle_state`).
pub async fn lifecycle_transition_exists(
    pool: &PgPool,
    org_id: Uuid,
    from_state: &str,
    to_state: &str,
) -> Result<bool, PdmError> {
    let ok: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM lifecycle_transitions t
             JOIN lifecycle_states fs ON fs.id = t.from_state AND fs.name = $2
             JOIN lifecycle_states ts ON ts.id = t.to_state AND ts.name = $3
             JOIN lifecycle_definitions d ON d.id = t.definition_id
             WHERE d.org_id = $1
         )",
    )
    .bind(org_id)
    .bind(from_state)
    .bind(to_state)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(ok)
}

/// Flip an item's lifecycle state. Caller must have validated the transition
/// (or be performing an authorized approval/rejection side effect).
pub async fn set_item_lifecycle_state(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    state: &str,
) -> Result<Item, PdmError> {
    let _item = get_item(pool, item_id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("item {item_id}")))?;
    let res = sqlx::query(
        "UPDATE items SET lifecycle_state = $1, updated_at = now() WHERE id = $2 AND org_id = $3",
    )
    .bind(state)
    .bind(item_id)
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("item {item_id}")));
    }
    get_item(pool, item_id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("item {item_id}")))
}

// ── Files & Blobs ────────────────────────────────────────────────────────

pub async fn ensure_file(
    pool: &PgPool,
    item_id: Uuid,
    filename: &str,
    mime: &str,
) -> Result<FileRecord, PdmError> {
    // Try to find existing
    let existing: Option<FileRecord> = sqlx::query_as::<_, FileRecord>(
        "SELECT id, item_id, filename, mime_type, created_at FROM files WHERE item_id = $1",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?;

    if let Some(f) = existing {
        return Ok(f);
    }

    sqlx::query_as::<_, FileRecord>(
        "INSERT INTO files (item_id, filename, mime_type)
         VALUES ($1, $2, $3)
         RETURNING id, item_id, filename, mime_type, created_at",
    )
    .bind(item_id)
    .bind(filename)
    .bind(mime)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_file(pool: &PgPool, item_id: Uuid) -> Result<Option<FileRecord>, PdmError> {
    sqlx::query_as::<_, FileRecord>(
        "SELECT id, item_id, filename, mime_type, created_at FROM files WHERE item_id = $1",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_file_by_id(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<pdm_core::FileRecord>, PdmError> {
    sqlx::query_as::<_, pdm_core::FileRecord>(
        "SELECT id, item_id, filename, mime_type, created_at FROM files WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn upload_blob(
    pool: &PgPool,
    org_id: Uuid,
    sha256: &str,
    size_bytes: i64,
    storage_key: &str,
) -> Result<Blob, PdmError> {
    // Dedup: check if blob already exists
    let existing: Option<Blob> = sqlx::query_as::<_, Blob>(
        "SELECT id, org_id, sha256, size_bytes, storage_key, created_at
         FROM file_blobs WHERE org_id = $1 AND sha256 = $2",
    )
    .bind(org_id)
    .bind(sha256)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?;

    if let Some(b) = existing {
        return Ok(b);
    }

    // Two uploads of the same bytes at once can both miss the look-up above; the second insert
    // then failed the (org_id, sha256) unique constraint (a 500). It now inserts nothing and takes
    // the row the first one stored.
    let inserted = sqlx::query_as::<_, Blob>(
        "INSERT INTO file_blobs (org_id, sha256, size_bytes, storage_key)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (org_id, sha256) DO NOTHING
         RETURNING id, org_id, sha256, size_bytes, storage_key, created_at",
    )
    .bind(org_id)
    .bind(sha256)
    .bind(size_bytes)
    .bind(storage_key)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?;
    if let Some(b) = inserted {
        return Ok(b);
    }
    sqlx::query_as::<_, Blob>(
        "SELECT id, org_id, sha256, size_bytes, storage_key, created_at
         FROM file_blobs WHERE org_id = $1 AND sha256 = $2",
    )
    .bind(org_id)
    .bind(sha256)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_blob_by_sha(
    pool: &PgPool,
    org_id: Uuid,
    sha256: &str,
) -> Result<Option<Blob>, PdmError> {
    sqlx::query_as::<_, Blob>(
        "SELECT id, org_id, sha256, size_bytes, storage_key, created_at
             FROM file_blobs WHERE org_id = $1 AND sha256 = $2",
    )
    .bind(org_id)
    .bind(sha256)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_blob_by_id(pool: &PgPool, id: Uuid) -> Result<Option<Blob>, PdmError> {
    sqlx::query_as::<_, Blob>(
        "SELECT id, org_id, sha256, size_bytes, storage_key, created_at
             FROM file_blobs WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

// ── File Versions ────────────────────────────────────────────────────────

/// Create the next version row for a file.
///
/// Every check-in advances `seq` (the global per-save order). With
/// `new_version = false` the save is a new *revision* of the current major
/// version; with `new_version = true` the major version is bumped and the
/// revision resets to 1.
#[allow(clippy::too_many_arguments)]
pub async fn create_version(
    pool: &PgPool,
    file_id: Uuid,
    blob_id: Uuid,
    author_id: Uuid,
    commit_message: &str,
    base_version_id: Option<Uuid>,
    native_format: &str,
    new_version: bool,
) -> Result<FileVersion, PdmError> {
    let head: Option<(i32, i32, i32)> = sqlx::query_as(
        "SELECT seq, version, revision FROM file_versions WHERE file_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?;

    let (next_seq, version, revision) = match head {
        None => (1, 1, 1),
        Some((s, v, _)) if new_version => (s + 1, v + 1, 1),
        Some((s, v, r)) => (s + 1, v, r + 1),
    };

    sqlx::query_as::<_, FileVersion>(
        "INSERT INTO file_versions (file_id, seq, version, revision, blob_id, author_id, commit_message, base_version_id, native_format)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id, file_id, seq, version, revision, blob_id, author_id, committed_at, commit_message,
                   base_version_id, native_format, measurements,
                   thumbnail_sha256, thumbnail_size_bytes,
                   thumbnail_error",
    )
    .bind(file_id)
    .bind(next_seq)
    .bind(version)
    .bind(revision)
    .bind(blob_id)
    .bind(author_id)
    .bind(commit_message)
    .bind(base_version_id)
    .bind(native_format)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

/// What [`checkin_version`] made: the file record, the head before it, and the new version.
pub struct CheckedIn {
    pub file: FileRecord,
    pub previous_head: Option<FileVersion>,
    pub version: FileVersion,
}

/// A check-in as one transaction. The item's lease row is locked first, so check-ins of one item
/// run one at a time. Inside it the lease must be live and the caller's, the base must be the head,
/// and the version is written and the lease released together. Parallel check-ins now give one new
/// version and "no active checkout" for the rest; before, they all passed the lease check and then
/// clashed on the version number (a 500).
#[allow(clippy::too_many_arguments)]
pub async fn checkin_version(
    pool: &PgPool,
    item_id: Uuid,
    user_id: Uuid,
    filename: &str,
    mime: &str,
    base_version: Option<i32>,
    blob_id: Uuid,
    commit_message: &str,
    native_format: &str,
    new_version: bool,
) -> Result<CheckedIn, PdmError> {
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;
    let holder: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM file_locks WHERE item_id = $1 AND lease_expires_at > now() FOR UPDATE",
    )
    .bind(item_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    match holder {
        None => return Err(PdmError::Conflict("no active checkout".into())),
        Some(h) if h != user_id => {
            return Err(PdmError::Conflict("lock held by another user".into()));
        }
        Some(_) => {}
    }
    // The file record is made on the first check-in.
    sqlx::query(
        "INSERT INTO files (item_id, filename, mime_type) VALUES ($1, $2, $3)
         ON CONFLICT (item_id) DO NOTHING",
    )
    .bind(item_id)
    .bind(filename)
    .bind(mime)
    .execute(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    let file = sqlx::query_as::<_, FileRecord>(
        "SELECT id, item_id, filename, mime_type, created_at FROM files WHERE item_id = $1",
    )
    .bind(item_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    let previous_head = sqlx::query_as::<_, FileVersion>(
        "SELECT id, file_id, seq, version, revision, blob_id, author_id, committed_at, commit_message,
                    base_version_id, native_format, measurements,
                    thumbnail_sha256, thumbnail_size_bytes,
                    thumbnail_error
             FROM file_versions WHERE file_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(file.id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    pdm_core::validate_checkin(base_version, previous_head.as_ref().map(|v| v.seq))?;
    let (next_seq, version_no, revision) = match &previous_head {
        None => (1, 1, 1),
        Some(h) if new_version => (h.seq + 1, h.version + 1, 1),
        Some(h) => (h.seq + 1, h.version, h.revision + 1),
    };
    let version = sqlx::query_as::<_, FileVersion>(
        "INSERT INTO file_versions (file_id, seq, version, revision, blob_id, author_id, commit_message, base_version_id, native_format)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id, file_id, seq, version, revision, blob_id, author_id, committed_at, commit_message,
                    base_version_id, native_format, measurements,
                    thumbnail_sha256, thumbnail_size_bytes,
                    thumbnail_error",
    )
    .bind(file.id)
    .bind(next_seq)
    .bind(version_no)
    .bind(revision)
    .bind(blob_id)
    .bind(user_id)
    .bind(commit_message)
    .bind(previous_head.as_ref().map(|v| v.id))
    .bind(native_format)
    .fetch_one(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    sqlx::query("DELETE FROM file_locks WHERE item_id = $1 AND user_id = $2")
        .bind(item_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(PdmError::Db)?;
    tx.commit().await.map_err(PdmError::Db)?;
    Ok(CheckedIn { file, previous_head, version })
}

pub async fn get_head_version(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Option<FileVersion>, PdmError> {
    sqlx::query_as::<_, FileVersion>(
        "SELECT id, file_id, seq, version, revision, blob_id, author_id, committed_at, commit_message,
                    base_version_id, native_format, measurements,
                    thumbnail_sha256, thumbnail_size_bytes,
                    thumbnail_error
             FROM file_versions WHERE file_id = $1 ORDER BY seq DESC",
    )
    .bind(file_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_versions(pool: &PgPool, file_id: Uuid) -> Result<Vec<FileVersion>, PdmError> {
    sqlx::query_as::<_, FileVersion>(
        "SELECT id, file_id, seq, version, revision, blob_id, author_id, committed_at, commit_message,
                    base_version_id, native_format, measurements,
                    thumbnail_sha256, thumbnail_size_bytes,
                    thumbnail_error
             FROM file_versions WHERE file_id = $1 ORDER BY seq DESC",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

/// Version history joined with the blob's SHA-256 and size (for listing + the portal).
pub async fn list_version_details(
    pool: &PgPool,
    file_id: Uuid,
) -> Result<Vec<pdm_core::VersionDetail>, PdmError> {
    let mut versions = sqlx::query_as::<_, pdm_core::VersionDetail>(
        "SELECT v.id, v.file_id, v.seq, v.version, v.revision, v.blob_id, v.author_id, v.committed_at,
                v.commit_message, v.base_version_id, v.native_format,
                b.sha256, b.size_bytes, v.thumbnail_sha256,
                v.thumbnail_error,
                v.derived_step_sha256, v.derived_step_size_bytes,
                v.derived_solidcraft_sha256, v.derived_solidcraft_size_bytes,
                v.derived_solidcraft_error, v.derived_solidcraft_job_id,
                f.mime_type, f.filename,
                COALESCE(NULLIF(au.display_name, ''), au.email) AS author_name
         FROM file_versions v
         JOIN file_blobs b ON b.id = v.blob_id
         JOIN files f ON f.id = v.file_id
         LEFT JOIN users au ON au.id = v.author_id
         WHERE v.file_id = $1
         ORDER BY v.seq DESC",
    )
    .bind(file_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)?;
    for version in &mut versions {
        version.filename = pdm_core::version_filename(&version.filename, &version.native_format);
    }
    Ok(versions)
}

pub async fn get_version(pool: &PgPool, id: Uuid) -> Result<Option<FileVersion>, PdmError> {
    sqlx::query_as::<_, FileVersion>(
        "SELECT id, file_id, seq, version, revision, blob_id, author_id, committed_at, commit_message,
                    base_version_id, native_format, measurements,
                    thumbnail_sha256, thumbnail_size_bytes,
                    thumbnail_error
             FROM file_versions WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_version_by_seq(
    pool: &PgPool,
    file_id: Uuid,
    seq: i32,
) -> Result<Option<FileVersion>, PdmError> {
    sqlx::query_as::<_, FileVersion>(
        "SELECT id, file_id, seq, version, revision, blob_id, author_id, committed_at, commit_message,
                    base_version_id, native_format, measurements,
                    thumbnail_sha256, thumbnail_size_bytes,
                    thumbnail_error
             FROM file_versions WHERE file_id = $1 AND seq = $2",
    )
    .bind(file_id)
    .bind(seq)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// The converted STEP of a version, if the `cad.convert` job stored one.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DerivedStep {
    pub sha256: String,
    pub size_bytes: i64,
}

pub async fn get_derived_step(
    pool: &PgPool,
    version_id: Uuid,
) -> Result<Option<DerivedStep>, PdmError> {
    sqlx::query_as::<_, DerivedStep>(
        "SELECT derived_step_sha256 AS sha256, derived_step_size_bytes AS size_bytes FROM file_versions
          WHERE id = $1 AND derived_step_sha256 IS NOT NULL",
    )
    .bind(version_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct VersionSource {
    pub item_id: Uuid,
    pub filename: String,
    pub seq: i32,
    pub native_format: String,
}

/// Tenant-scoped source lookup for artifact creation and download.
pub async fn version_source(
    pool: &PgPool,
    org: Uuid,
    version_id: Uuid,
) -> Result<Option<VersionSource>, PdmError> {
    let mut source: Option<VersionSource> = sqlx::query_as("SELECT f.item_id, f.filename, v.seq, v.native_format FROM file_versions v JOIN files f ON f.id = v.file_id JOIN items i ON i.id = f.item_id WHERE v.id = $1 AND i.org_id = $2")
        .bind(version_id).bind(org).fetch_optional(pool).await.map_err(PdmError::Db)?;
    if let Some(s) = &mut source {
        s.filename = pdm_core::version_filename(&s.filename, &s.native_format);
    }
    Ok(source)
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct SolidcraftArtifact {
    pub sha256: Option<String>,
    pub size_bytes: Option<i64>,
    pub error: Option<String>,
    pub job_id: Option<Uuid>,
}

pub async fn get_solidcraft_artifact(
    pool: &PgPool,
    version_id: Uuid,
) -> Result<SolidcraftArtifact, PdmError> {
    sqlx::query_as("SELECT derived_solidcraft_sha256 AS sha256, derived_solidcraft_size_bytes AS size_bytes, derived_solidcraft_error AS error, derived_solidcraft_job_id AS job_id FROM file_versions WHERE id = $1")
        .bind(version_id).fetch_one(pool).await.map_err(PdmError::Db)
}

/// Row locking makes repeated/concurrent clicks reuse the same active job.
pub async fn queue_solidcraft_conversion(
    pool: &PgPool,
    org: Uuid,
    version_id: Uuid,
    source: &VersionSource,
) -> Result<(), PdmError> {
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;
    let ready: Option<String> = sqlx::query_scalar(
        "SELECT derived_solidcraft_sha256 FROM file_versions WHERE id = $1 FOR UPDATE",
    )
    .bind(version_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    let active: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE org_id = $1 AND kind = 'cad.solidcraft' AND payload->>'version_id' = $2 AND status IN ('queued', 'running'))")
        .bind(org).bind(version_id.to_string()).fetch_one(&mut *tx).await.map_err(PdmError::Db)?;
    if ready.is_none() && !active {
        let payload = serde_json::json!({"version_id":version_id,"item_id":source.item_id,"filename":source.filename});
        let job_id: Uuid = sqlx::query_scalar("INSERT INTO jobs (org_id, kind, payload, max_attempts) VALUES ($1, 'cad.solidcraft', $2, 2) RETURNING id")
            .bind(org).bind(payload).fetch_one(&mut *tx).await.map_err(PdmError::Db)?;
        sqlx::query("UPDATE file_versions SET derived_solidcraft_job_id = $2, derived_solidcraft_error = NULL WHERE id = $1")
            .bind(version_id).bind(job_id).execute(&mut *tx).await.map_err(PdmError::Db)?;
    }
    tx.commit().await.map_err(PdmError::Db)
}

pub async fn store_solidcraft_artifact(
    pool: &PgPool,
    version_id: Uuid,
    job_id: Uuid,
    sha: &str,
    size: i64,
) -> Result<(), PdmError> {
    let changed = sqlx::query("UPDATE file_versions SET derived_solidcraft_sha256 = $3, derived_solidcraft_size_bytes = $4, derived_solidcraft_error = NULL WHERE id = $1 AND derived_solidcraft_job_id = $2")
        .bind(version_id).bind(job_id).bind(sha).bind(size).execute(pool).await.map_err(PdmError::Db)?;
    if changed.rows_affected() != 1 {
        return Err(PdmError::Conflict("conversion was superseded".into()));
    }
    Ok(())
}

pub async fn queue_preview_render(
    pool: &PgPool,
    org: Uuid,
    version_id: Uuid,
    source: &VersionSource,
) -> Result<Job, PdmError> {
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;
    let _: Uuid = sqlx::query_scalar("SELECT id FROM file_versions WHERE id = $1 FOR UPDATE")
        .bind(version_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(PdmError::Db)?;
    let pending: Option<Job> = sqlx::query_as("SELECT id, org_id, kind, payload, status, attempts, max_attempts, last_error, created_at FROM jobs WHERE org_id = $1 AND kind = 'preview.render' AND payload->>'version_id' = $2 AND status IN ('queued', 'running') ORDER BY created_at DESC LIMIT 1")
        .bind(org).bind(version_id.to_string()).fetch_optional(&mut *tx).await.map_err(PdmError::Db)?;
    let job = match pending {
        Some(job) => job,
        None => {
            let payload = serde_json::json!({"version_id":version_id,"item_id":source.item_id,"filename":source.filename});
            sqlx::query_as("INSERT INTO jobs (org_id, kind, payload) VALUES ($1, 'preview.render', $2) RETURNING id, org_id, kind, payload, status, attempts, max_attempts, last_error, created_at")
                .bind(org).bind(payload).fetch_one(&mut *tx).await.map_err(PdmError::Db)?
        }
    };
    tx.commit().await.map_err(PdmError::Db)?;
    Ok(job)
}

/// Record a converted STEP (content-addressed blob) on a version.
pub async fn store_derived_step(
    pool: &PgPool,
    version_id: Uuid,
    sha256: &str,
    size_bytes: i64,
) -> Result<(), PdmError> {
    sqlx::query(
        "UPDATE file_versions SET derived_step_sha256 = $2, derived_step_size_bytes = $3 WHERE id = $1",
    )
    .bind(version_id)
    .bind(sha256)
    .bind(size_bytes)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

/// Record why a version has no thumbnail (the CLI could not open or convert the
/// file), so the portal shows the reason instead of an indefinitely-pending state.
pub async fn store_thumbnail_error(
    pool: &PgPool,
    version_id: Uuid,
    error: &str,
) -> Result<(), PdmError> {
    sqlx::query("UPDATE file_versions SET thumbnail_error = $2 WHERE id = $1")
        .bind(version_id)
        .bind(error)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    Ok(())
}

/// Record a rendered thumbnail (content-addressed PNG blob) on a version.
pub async fn store_thumbnail(
    pool: &PgPool,
    version_id: Uuid,
    sha256: &str,
    size_bytes: i64,
) -> Result<(), PdmError> {
    sqlx::query(
        "UPDATE file_versions SET thumbnail_sha256 = $2, thumbnail_size_bytes = $3, thumbnail_error = NULL WHERE id = $1",
    )
    .bind(version_id)
    .bind(sha256)
    .bind(size_bytes)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

// ── Locks ────────────────────────────────────────────────────────────────

pub async fn acquire_lock(
    pool: &PgPool,
    item_id: Uuid,
    user_id: Uuid,
    workstation: &str,
    base_version_id: Option<Uuid>,
    lease_hours: i64,
) -> Result<FileLock, PdmError> {
    let lease = Duration::hours(lease_hours);
    let row = sqlx::query_as::<_, FileLock>(
        "INSERT INTO file_locks (item_id, user_id, workstation, base_version_id, lease_expires_at)
         VALUES ($1, $2, $3, $4, now() + $5)
         ON CONFLICT (item_id) DO UPDATE
           SET user_id = EXCLUDED.user_id,
               workstation = EXCLUDED.workstation,
               base_version_id = EXCLUDED.base_version_id,
               acquired_at = now(),
               lease_expires_at = EXCLUDED.lease_expires_at
           WHERE file_locks.lease_expires_at < now() OR file_locks.user_id = EXCLUDED.user_id
         RETURNING id, item_id, user_id, workstation, base_version_id, acquired_at, lease_expires_at",
    )
    .bind(item_id)
    .bind(user_id)
    .bind(workstation)
    .bind(base_version_id)
    .bind(lease)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?;

    match row {
        Some(lock) => Ok(lock),
        None => Err(PdmError::Conflict("item is locked by another user".into())),
    }
}

pub async fn release_lock(pool: &PgPool, item_id: Uuid, user_id: Uuid) -> Result<(), PdmError> {
    let result = sqlx::query("DELETE FROM file_locks WHERE item_id = $1 AND user_id = $2")
        .bind(item_id)
        .bind(user_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if result.rows_affected() == 0 {
        return Err(PdmError::Conflict(
            "lock not found or owned by another user".into(),
        ));
    }
    Ok(())
}

pub async fn get_lock(pool: &PgPool, item_id: Uuid) -> Result<Option<FileLock>, PdmError> {
    sqlx::query_as::<_, FileLock>(
        "SELECT id, item_id, user_id, workstation, base_version_id, acquired_at, lease_expires_at
             FROM file_locks WHERE item_id = $1 AND lease_expires_at > now()",
    )
    .bind(item_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

// ── Audit ────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub async fn record_audit(
    pool: &PgPool,
    org_id: Uuid,
    actor_id: Option<Uuid>,
    agent_id: &str,
    action: &str,
    object_type: &str,
    object_id: Option<Uuid>,
    detail: Option<serde_json::Value>,
) -> Result<AuditEvent, PdmError> {
    sqlx::query_as::<_, AuditEvent>(
        "INSERT INTO audit_events (org_id, actor_id, agent_id, action, object_type, object_id, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         RETURNING id, org_id, actor_id, agent_id, action, object_type, object_id, detail, at",
    )
    .bind(org_id)
    .bind(actor_id)
    .bind(agent_id)
    .bind(action)
    .bind(object_type)
    .bind(object_id)
    .bind(detail)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_audit(
    pool: &PgPool,
    org_id: Uuid,
    object_id: Option<Uuid>,
    limit: u32,
) -> Result<Vec<AuditEvent>, PdmError> {
    let sql = format!(
        "SELECT id, org_id, actor_id, agent_id, action, object_type, object_id, detail, at
         FROM audit_events
         WHERE org_id = $1 {}
         ORDER BY at DESC
         LIMIT {}",
        if object_id.is_some() {
            "AND object_id = $2"
        } else {
            ""
        },
        limit
    );

    let mut q = sqlx::query_as::<_, AuditEvent>(&sql).bind(org_id);
    if let Some(oid) = object_id {
        q = q.bind(oid);
    }
    q.fetch_all(pool).await.map_err(PdmError::Db)
}

/// Home aggregate: KPIs + recent audit + my approvals + active jobs in one
/// round trip (replaces the dashboard's 5+ separate calls).
pub async fn home_dashboard(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
) -> Result<serde_json::Value, PdmError> {
    let mine_json = serde_json::json!([user_id]);

    let items: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM items WHERE org_id = $1 AND archived_at IS NULL",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let open_checkouts: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM file_locks fl
         JOIN items i ON i.id = fl.item_id
         WHERE i.org_id = $1 AND fl.lease_expires_at > now()",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let open_checkouts_mine: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM file_locks fl
         JOIN items i ON i.id = fl.item_id
         WHERE i.org_id = $1 AND fl.user_id = $2 AND fl.lease_expires_at > now()",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let pending_approvals: i64 = sqlx::query_scalar::<_, i64>(&format!(
        "SELECT count(*)::bigint FROM approval_requests ar
         WHERE ar.org_id = $1 AND ar.status = 'pending' AND {OWN_ITEM_REQUEST}"
    ))
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let pending_approvals_mine: i64 = sqlx::query_scalar::<_, i64>(&format!(
        "SELECT count(*)::bigint FROM approval_requests ar
         WHERE ar.org_id = $1 AND ar.status = 'pending' AND {OWN_ITEM_REQUEST}
           AND (ar.requested_by = $2 OR ar.assignees @> $3::jsonb)"
    ))
    .bind(org_id)
    .bind(user_id)
    .bind(mine_json.clone())
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let released_items: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM items WHERE org_id = $1 AND lifecycle_state = 'released'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let queued_jobs: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM jobs WHERE org_id = $1 AND status = 'queued'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let running_jobs: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM jobs WHERE org_id = $1 AND status = 'running'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let failed_jobs: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)::bigint FROM jobs WHERE org_id = $1 AND status = 'failed'",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let storage_bytes: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(sum(size_bytes)::bigint, 0) FROM file_blobs WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)?;

    let recent_audit = list_audit(pool, org_id, None, 10).await?;
    let my_approvals = list_approval_requests(pool, org_id, Some("pending"), Some(user_id), 1, 5)
        .await?
        .items;
    let active_jobs = list_active_jobs(pool, org_id, 10).await?;

    Ok(serde_json::json!({
        "kpis": {
            "items": items,
            "open_checkouts": open_checkouts,
            "open_checkouts_mine": open_checkouts_mine,
            "pending_approvals": pending_approvals,
            "pending_approvals_mine": pending_approvals_mine,
            "released_items": released_items,
            "queued_jobs": queued_jobs,
            "running_jobs": running_jobs,
            "failed_jobs": failed_jobs,
            "storage_bytes": storage_bytes,
        },
        "recent_audit": recent_audit,
        "my_approvals": my_approvals,
        "active_jobs": active_jobs,
    }))
}

// ── Jobs ─────────────────────────────────────────────────────────────────

pub async fn enqueue_job(
    pool: &PgPool,
    org_id: Uuid,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<Job, PdmError> {
    sqlx::query_as::<_, Job>(
        "INSERT INTO jobs (org_id, kind, payload) VALUES ($1, $2, $3)
         RETURNING id, org_id, kind, payload, status, attempts, max_attempts, last_error, created_at",
    )
    .bind(org_id)
    .bind(kind)
    .bind(payload)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn claim_jobs(pool: &PgPool, worker_id: &str, limit: i64) -> Result<Vec<Job>, PdmError> {
    sqlx::query_as::<_, Job>(
        "UPDATE jobs SET status = 'running', locked_by = $1, updated_at = now()
             WHERE id IN (
                 SELECT id FROM jobs
                 WHERE status = 'queued' AND run_after <= now()
                 ORDER BY run_after
                 LIMIT $2
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING id, org_id, kind, payload, status, attempts, max_attempts, last_error, created_at",
    )
    .bind(worker_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn complete_job(
    pool: &PgPool,
    job_id: Uuid,
    ok: bool,
    error: &str,
) -> Result<(), PdmError> {
    let new_status = if ok { "done" } else { "failed" };
    sqlx::query(
        "UPDATE jobs SET status = $2, last_error = $3, updated_at = now()
         WHERE id = $1",
    )
    .bind(job_id)
    .bind(new_status)
    .bind(error)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

pub async fn get_job(pool: &PgPool, id: Uuid, org_id: Uuid) -> Result<Option<Job>, PdmError> {
    sqlx::query_as::<_, Job>(
        "SELECT id, org_id, kind, payload, status, attempts, max_attempts, last_error, created_at
             FROM jobs WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// Recent jobs for an organization (dashboard "processing jobs" widget).
pub async fn list_jobs(pool: &PgPool, org_id: Uuid, limit: i32) -> Result<Vec<Job>, PdmError> {
    sqlx::query_as::<_, Job>(
        "SELECT id, org_id, kind, payload, status, attempts, max_attempts, last_error, created_at
             FROM jobs WHERE org_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(org_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

/// Non-terminal jobs (queued/running), newest first — dashboard "active jobs".
pub async fn list_active_jobs(
    pool: &PgPool,
    org_id: Uuid,
    limit: i32,
) -> Result<Vec<Job>, PdmError> {
    sqlx::query_as::<_, Job>(
        "SELECT id, org_id, kind, payload, status, attempts, max_attempts, last_error, created_at
             FROM jobs WHERE org_id = $1 AND status IN ('queued', 'running')
             ORDER BY created_at DESC LIMIT $2",
    )
    .bind(org_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

/// Append an event to the integration outbox (at-least-once delivery to
/// downstream consumers; consumers mark rows `sent` after processing).
pub async fn enqueue_outbox(
    pool: &PgPool,
    org_id: Uuid,
    kind: &str,
    payload: &serde_json::Value,
) -> Result<pdm_core::IntegrationOutbox, PdmError> {
    sqlx::query_as::<_, pdm_core::IntegrationOutbox>(
        "INSERT INTO integration_outbox (org_id, kind, payload) VALUES ($1, $2, $3)
             RETURNING id, org_id, kind, payload, status, attempts, sent_at, created_at",
    )
    .bind(org_id)
    .bind(kind)
    .bind(payload)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn record_job_attempt(
    pool: &PgPool,
    job_id: Uuid,
    ok: bool,
    error: &str,
) -> Result<(), PdmError> {
    sqlx::query(
        "INSERT INTO job_attempts (job_id, ended_at, ok, error) VALUES ($1, now(), $2, $3)",
    )
    .bind(job_id)
    .bind(ok)
    .bind(error)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    // Also increment attempts on the job
    sqlx::query("UPDATE jobs SET attempts = attempts + 1 WHERE id = $1")
        .bind(job_id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    Ok(())
}

// ── Properties ───────────────────────────────────────────────────────────

/// Set a property on one of the organization's items; NotFound for any other item.
pub async fn upsert_property(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    key: &str,
    value_type: &str,
    value: &serde_json::Value,
) -> Result<Property, PdmError> {
    sqlx::query_as::<_, Property>(
        "INSERT INTO properties (item_id, prop_key, value_type, value)
         SELECT $1, $2, $3, $4
          WHERE EXISTS (SELECT 1 FROM items WHERE id = $1 AND org_id = $5)
         ON CONFLICT (item_id, prop_key)
         DO UPDATE SET value_type = EXCLUDED.value_type, value = EXCLUDED.value, updated_at = now()
         RETURNING id, item_id, prop_key, value_type, value, computed, updated_at",
    )
    .bind(item_id)
    .bind(key)
    .bind(value_type)
    .bind(value)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?
    .ok_or_else(|| PdmError::NotFound(format!("item {item_id}")))
}

pub async fn list_properties(pool: &PgPool, item_id: Uuid) -> Result<Vec<Property>, PdmError> {
    sqlx::query_as::<_, Property>(
        "SELECT id, item_id, prop_key, value_type, value, computed, updated_at
             FROM properties WHERE item_id = $1 ORDER BY prop_key",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

// ── Revisions ────────────────────────────────────────────────────────────

/// Release a version of one of the organization's items: record the revision and mark the item
/// released, together or not at all. NotFound when the item is not the organization's or the
/// version is not the item's.
#[allow(clippy::too_many_arguments)]
pub async fn release_revision(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    label: &str,
    version_id: Uuid,
    released_by: Option<Uuid>,
    release_note: &str,
    scheme: &str,
) -> Result<FileRevision, PdmError> {
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;
    let owned: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM items WHERE id = $1 AND org_id = $2 FOR UPDATE")
            .bind(item_id)
            .bind(org_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(PdmError::Db)?;
    if owned.is_none() {
        return Err(PdmError::NotFound(format!("item {item_id}")));
    }
    require_org_refs(
        &mut tx,
        org_id,
        &[OrgRef::ItemVersion {
            item_id,
            version_id,
        }],
    )
    .await?;
    let revision = sqlx::query_as::<_, FileRevision>(
        "INSERT INTO file_revisions (item_id, label, version_id, released_by, release_note, scheme)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, item_id, label, version_id, released_by, released_at, release_note, scheme",
    )
    .bind(item_id)
    .bind(label)
    .bind(version_id)
    .bind(released_by)
    .bind(release_note)
    .bind(scheme)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") {
            PdmError::Conflict(format!("revision label '{label}' already exists"))
        } else {
            PdmError::Db(e)
        }
    })?;
    sqlx::query(
        "UPDATE items SET lifecycle_state = 'released', updated_at = now()
          WHERE id = $1 AND org_id = $2",
    )
    .bind(item_id)
    .bind(org_id)
    .execute(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    tx.commit().await.map_err(PdmError::Db)?;
    Ok(revision)
}

pub async fn list_revisions(pool: &PgPool, item_id: Uuid) -> Result<Vec<FileRevision>, PdmError> {
    sqlx::query_as::<_, FileRevision>(
        "SELECT id, item_id, label, version_id, released_by, released_at, release_note, scheme
             FROM file_revisions WHERE item_id = $1 ORDER BY released_at",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

// ── Comments ─────────────────────────────────────────────────────────────

/// Comment on one of the organization's items; NotFound for any other item.
pub async fn add_comment(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    user_id: Uuid,
    body: &str,
) -> Result<ItemComment, PdmError> {
    sqlx::query_as::<_, ItemComment>(
        "INSERT INTO item_comments (item_id, user_id, body)
         SELECT $1, $2, $3 WHERE EXISTS (SELECT 1 FROM items WHERE id = $1 AND org_id = $4)
         RETURNING item_comments.id,
                   item_comments.item_id,
                   item_comments.user_id,
                   (SELECT COALESCE(NULLIF(u.display_name, ''), u.email)
                      FROM users u WHERE u.id = item_comments.user_id) AS author_name,
                   item_comments.body,
                   item_comments.created_at",
    )
    .bind(item_id)
    .bind(user_id)
    .bind(body)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?
    .ok_or_else(|| PdmError::NotFound(format!("item {item_id}")))
}

pub async fn list_comments(pool: &PgPool, item_id: Uuid) -> Result<Vec<ItemComment>, PdmError> {
    sqlx::query_as::<_, ItemComment>(
        "SELECT c.id, c.item_id, c.user_id,
                COALESCE(NULLIF(u.display_name, ''), u.email) AS author_name,
                c.body, c.created_at
             FROM item_comments c JOIN users u ON u.id = c.user_id
            WHERE c.item_id = $1 ORDER BY c.created_at",
    )
    .bind(item_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

// ── Item updates (web workspace: edit metadata, move, reassign owner) ───

/// The SET clauses of [`update_item`], numbered as it binds them: `$1` is the item and `$2` the
/// organization, so the values start at `$3`. (They started at `$2`, so the first value was the
/// organization's id: a move wrote it into `folder_id` and every change failed.)
fn item_update_sets(
    description: bool,
    folder_id: Option<Option<Uuid>>,
    owner_id: Option<Option<Uuid>>,
) -> Vec<String> {
    let mut sets: Vec<String> = vec!["updated_at = now()".to_string()];
    let mut param = 3usize;
    if description {
        sets.push(format!("description = ${param}"));
        param += 1;
    }
    match folder_id {
        Some(Some(_)) => {
            sets.push(format!("folder_id = ${param}"));
            param += 1;
        }
        Some(None) => sets.push("folder_id = NULL".into()),
        None => {}
    }
    match owner_id {
        Some(Some(_)) => sets.push(format!("owner_id = ${param}")),
        Some(None) => sets.push("owner_id = NULL".into()),
        None => {}
    }
    sets
}

/// Update an item's mutable metadata. `None` fields are left unchanged;
/// `Some(None)` clears an optional field (folder / owner).
pub async fn update_item(
    pool: &PgPool,
    item_id: Uuid,
    org_id: Uuid,
    description: Option<&str>,
    folder_id: Option<Option<Uuid>>,
    owner_id: Option<Option<Uuid>>,
) -> Result<ItemSummary, PdmError> {
    // A folder must be in the item's own project and an owner in its organization: the foreign
    // keys alone accept any organization's.
    if let Some(Some(fid)) = folder_id {
        let in_project: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM folders f JOIN items i ON i.project_id = f.project_id
                             WHERE f.id = $1 AND f.org_id = $2 AND i.id = $3 AND i.org_id = $2)",
        )
        .bind(fid)
        .bind(org_id)
        .bind(item_id)
        .fetch_one(pool)
        .await
        .map_err(PdmError::Db)?;
        if !in_project {
            return Err(PdmError::Validation("the folder is not in this item's project".into()));
        }
    }
    if let Some(Some(oid)) = owner_id {
        let in_org: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND org_id = $2)")
                .bind(oid)
                .bind(org_id)
                .fetch_one(pool)
                .await
                .map_err(PdmError::Db)?;
        if !in_org {
            return Err(PdmError::Validation("the owner is not in this organization".into()));
        }
    }
    let sets = item_update_sets(description.is_some(), folder_id, owner_id);
    let bind_description: Option<String> = description.map(str::to_string);
    let bind_folder: Option<Uuid> = folder_id.flatten();
    let bind_owner: Option<Uuid> = owner_id.flatten();
    if sets.len() == 1 {
        return get_item_summary(pool, item_id, org_id)
            .await?
            .ok_or_else(|| PdmError::NotFound(format!("item {item_id}")));
    }
    let sql = format!(
        "UPDATE items SET {} WHERE id = $1 AND org_id = $2",
        sets.join(", ")
    );
    let mut q = sqlx::query(&sql).bind(item_id).bind(org_id);
    if let Some(d) = bind_description {
        q = q.bind(d);
    }
    if let Some(f) = bind_folder {
        q = q.bind(f);
    }
    if let Some(o) = bind_owner {
        q = q.bind(o);
    }
    let res = q.execute(pool).await.map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound(format!("item {item_id}")));
    }
    get_item_summary(pool, item_id, org_id)
        .await?
        .ok_or_else(|| PdmError::NotFound(format!("item {item_id}")))
}

#[cfg(test)]
mod item_update_tests {
    use super::*;

    #[test]
    fn values_are_numbered_after_the_item_and_the_organization() {
        let f = Some(Uuid::nil());
        assert_eq!(
            item_update_sets(true, Some(f), Some(f)),
            vec!["updated_at = now()", "description = $3", "folder_id = $4", "owner_id = $5"]
        );
        assert_eq!(item_update_sets(false, Some(f), None), vec!["updated_at = now()", "folder_id = $3"]);
        assert_eq!(
            item_update_sets(false, Some(None), Some(f)),
            vec!["updated_at = now()", "folder_id = NULL", "owner_id = $3"]
        );
        assert_eq!(item_update_sets(true, None, Some(None)), vec!["updated_at = now()", "description = $3", "owner_id = NULL"]);
        assert_eq!(item_update_sets(false, None, None), vec!["updated_at = now()"]);
    }
}

// ── Approvals (dashboard widget) ────────────────────────────────────────

pub async fn list_pending_approvals(
    pool: &PgPool,
    org_id: Uuid,
    limit: i32,
) -> Result<Vec<serde_json::Value>, PdmError> {
    type ApprovalRow = (
        Uuid,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        DateTime<Utc>,
    );
    let rows: Vec<ApprovalRow> = sqlx::query_as(
        "SELECT ar.id, ar.item_id, i.part_number, i.description, ar.created_at
                 FROM approval_requests ar
                 LEFT JOIN items i ON i.id = ar.item_id AND i.org_id = ar.org_id
                WHERE ar.org_id = $1 AND ar.status = 'pending'
                  AND (ar.item_id IS NULL OR i.id IS NOT NULL)
                ORDER BY ar.created_at DESC LIMIT $2",
    )
    .bind(org_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(rows
        .into_iter()
        .map(|(id, item_id, part_number, description, created_at)| {
            serde_json::json!({
                "id": id,
                "item_id": item_id,
                "part_number": part_number,
                "description": description,
                "created_at": created_at,
            })
        })
        .collect())
}

// ── Approvals (decisions + inbox) ────────────────────────────────────────

/// SQL condition on `approval_requests ar`: the request names no item, or one of its own
/// organization's. Lists and counts leave out requests that name another organization's item.
const OWN_ITEM_REQUEST: &str = "(ar.item_id IS NULL OR EXISTS (SELECT 1 FROM items it
      WHERE it.id = ar.item_id AND it.org_id = ar.org_id))";

/// Raise an approval request for one of the organization's items; NotFound for any other item.
pub async fn create_approval_request(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    requested_by: Option<Uuid>,
    assignees: &[Uuid],
) -> Result<pdm_core::ApprovalRequest, PdmError> {
    let assignees_json = serde_json::to_string(assignees).unwrap_or_else(|_| "[]".to_string());
    type CreatedRow = (Uuid, Uuid, Uuid, Option<Uuid>, String, DateTime<Utc>);
    let row: Option<CreatedRow> = sqlx::query_as(
        "INSERT INTO approval_requests (org_id, item_id, requested_by, status, assignees)
             SELECT $1, $2, $3, 'pending', $4::jsonb
              WHERE EXISTS (SELECT 1 FROM items WHERE id = $2 AND org_id = $1)
             RETURNING id, org_id, item_id, requested_by, status, created_at",
    )
    .bind(org_id)
    .bind(item_id)
    .bind(requested_by)
    .bind(assignees_json)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)?;
    let Some((id, _org, req_item, requested_by, status, created_at)) = row else {
        return Err(PdmError::NotFound(format!("item {item_id}")));
    };
    Ok(pdm_core::ApprovalRequest {
        id,
        org_id,
        item_id: Some(req_item),
        requested_by,
        status,
        assignees: assignees.to_vec(),
        created_at,
    })
}

/// List approval requests for an org, optionally filtered by status and by
/// "mine" (requested by or assigned to `user_id`).
#[allow(clippy::too_many_arguments)]
pub async fn list_approval_requests(
    pool: &PgPool,
    org_id: Uuid,
    status: Option<&str>,
    mine: Option<Uuid>,
    page: u32,
    limit: u32,
) -> Result<Paginated<pdm_core::ApprovalRequestSummary>, PdmError> {
    let offset = (page - 1) * limit;

    // A request naming another organization's item is left out.
    let mut where_clauses: Vec<String> = vec![format!("ar.org_id = $1 AND {OWN_ITEM_REQUEST}")];
    let mut param: i32 = 2;
    if status.is_some() {
        where_clauses.push(format!("AND ar.status = ${param}"));
        param += 1;
    }
    if mine.is_some() {
        where_clauses.push(format!(
            "AND (ar.requested_by = ${param} OR ar.assignees @> ${}::jsonb)",
            param + 1
        ));
        param += 2;
    }
    let where_sql = where_clauses.join(" ");
    let limit_ph = param;
    let offset_ph = param + 1;

    let data_sql = format!(
        "SELECT ar.id, ar.item_id, i.part_number, ar.requested_by,
                COALESCE(NULLIF(u.display_name, ''), u.email),
                ar.assignees::text, ar.status, ar.created_at
             FROM approval_requests ar
             LEFT JOIN items i ON i.id = ar.item_id AND i.org_id = ar.org_id
             LEFT JOIN users u ON u.id = ar.requested_by
             WHERE {where_sql}
             ORDER BY ar.created_at DESC
             LIMIT ${limit_ph} OFFSET ${offset_ph}"
    );
    let mut q = sqlx::query_as::<
        _,
        (
            Uuid,
            Option<Uuid>,
            Option<String>,
            Option<Uuid>,
            Option<String>,
            String,
            String,
            DateTime<Utc>,
        ),
    >(&data_sql);
    q = q.bind(org_id);
    if let Some(s) = status {
        q = q.bind(s);
    }
    if let Some(user_id) = mine {
        q = q.bind(user_id);
        q = q.bind(serde_json::json!([user_id]));
    }
    let rows = q
        .bind(i64::from(limit))
        .bind(i64::from(offset))
        .fetch_all(pool)
        .await
        .map_err(PdmError::Db)?;

    let count_sql = format!("SELECT count(*)::bigint FROM approval_requests ar WHERE {where_sql}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    cq = cq.bind(org_id);
    if let Some(s) = status {
        cq = cq.bind(s);
    }
    if let Some(user_id) = mine {
        cq = cq.bind(user_id);
        cq = cq.bind(serde_json::json!([user_id]));
    }
    let total: i64 = cq.fetch_one(pool).await.map_err(PdmError::Db)?;

    let items: Vec<pdm_core::ApprovalRequestSummary> = rows
        .into_iter()
        .map(
            |(
                id,
                item_id,
                part_number,
                requested_by,
                requested_by_name,
                assignees_json,
                status,
                created_at,
            )| {
                let assignees: Vec<Uuid> =
                    serde_json::from_str(&assignees_json).unwrap_or_default();
                pdm_core::ApprovalRequestSummary {
                    id,
                    item_id,
                    item_part_number: part_number,
                    requested_by,
                    requested_by_name,
                    assignees,
                    status,
                    created_at,
                }
            },
        )
        .collect();
    let has_more = i64::from(offset) + i64::try_from(items.len()).unwrap_or(i64::MAX) < total;

    Ok(Paginated {
        items,
        page,
        limit,
        total,
        has_more,
    })
}

/// Record a decision on a pending request and flip its status, atomically.
/// `is_lead` authorizes non-assignees (org admin/editor) when a request has
/// no assignees.
pub async fn decide_approval_request(
    pool: &PgPool,
    org_id: Uuid,
    request_id: Uuid,
    user_id: Uuid,
    is_lead: bool,
    decision: &str,
    comment: &str,
) -> Result<pdm_core::ApprovalRequest, PdmError> {
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;

    type LockedRow = (
        Uuid,
        Option<Uuid>,
        Option<Uuid>,
        String,
        String,
        DateTime<Utc>,
    );
    // A request naming another organization's item is treated as missing.
    let row: Option<LockedRow> = sqlx::query_as(
        "SELECT ar.id, ar.item_id, ar.requested_by, ar.status, ar.assignees::text, ar.created_at
             FROM approval_requests ar
            WHERE ar.id = $1 AND ar.org_id = $2
              AND (ar.item_id IS NULL
                   OR EXISTS (SELECT 1 FROM items i WHERE i.id = ar.item_id AND i.org_id = ar.org_id))
              FOR UPDATE",
    )
    .bind(request_id)
    .bind(org_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    let Some((id, item_id, requested_by, status, assignees_json, created_at)) = row else {
        return Err(PdmError::NotFound(format!("approval request {request_id}")));
    };
    if status != "pending" {
        return Err(PdmError::Conflict(format!(
            "approval request {request_id} is already {status}"
        )));
    }
    if decision != "approve" && decision != "reject" {
        return Err(PdmError::Validation(
            "decision must be 'approve' or 'reject'".into(),
        ));
    }
    let assignees: Vec<Uuid> = serde_json::from_str(&assignees_json).unwrap_or_default();
    if !assignees.contains(&user_id) && !is_lead {
        return Err(PdmError::Forbidden(
            "you are not an assignee of this approval request".into(),
        ));
    }

    sqlx::query(
        "INSERT INTO approval_decisions (request_id, user_id, decision, comment)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(request_id)
    .bind(user_id)
    .bind(decision)
    .bind(comment)
    .execute(&mut *tx)
    .await
    .map_err(PdmError::Db)?;
    let _ = sqlx::query("UPDATE approval_requests SET status = $1 WHERE id = $2")
        .bind(decision)
        .bind(request_id)
        .execute(&mut *tx)
        .await
        .map_err(PdmError::Db)?;

    tx.commit().await.map_err(PdmError::Db)?;

    Ok(pdm_core::ApprovalRequest {
        id,
        org_id,
        item_id,
        requested_by,
        status: decision.to_string(),
        assignees,
        created_at,
    })
}

// ── Materials ────────────────────────────────────────────────────────────

pub async fn get_material(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
) -> Result<Option<Material>, PdmError> {
    sqlx::query_as::<_, Material>(
        "SELECT id, org_id, name, density FROM materials WHERE org_id = $1 AND name = $2",
    )
    .bind(org_id)
    .bind(name)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn upsert_material(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    density: Option<f64>,
) -> Result<Material, PdmError> {
    sqlx::query_as::<_, Material>(
        "INSERT INTO materials (org_id, name, density)
         VALUES ($1, $2, $3)
         ON CONFLICT (org_id, name)
         DO UPDATE SET density = EXCLUDED.density
         RETURNING id, org_id, name, density",
    )
    .bind(org_id)
    .bind(name)
    .bind(density)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

// ── Part number prefixes (org → category → part-number prefix) ────────

pub async fn list_part_number_prefixes(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<PartNumberPrefix>, PdmError> {
    sqlx::query_as::<_, PartNumberPrefix>(
        "SELECT id, org_id, category, prefix, created_at, updated_at
             FROM part_number_prefixes WHERE org_id = $1 ORDER BY category",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn upsert_part_number_prefix(
    pool: &PgPool,
    org_id: Uuid,
    category: &str,
    prefix: &str,
) -> Result<PartNumberPrefix, PdmError> {
    sqlx::query_as::<_, PartNumberPrefix>(
        "INSERT INTO part_number_prefixes (org_id, category, prefix)
         VALUES ($1, $2, $3)
         ON CONFLICT (org_id, category)
         DO UPDATE SET prefix = EXCLUDED.prefix, updated_at = now()
         RETURNING id, org_id, category, prefix, created_at, updated_at",
    )
    .bind(org_id)
    .bind(category)
    .bind(prefix)
    .fetch_one(pool)
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") {
            PdmError::Conflict(format!(
                "prefix {prefix} is already used by another category"
            ))
        } else {
            PdmError::Db(e)
        }
    })
}

pub async fn delete_part_number_prefix(
    pool: &PgPool,
    org_id: Uuid,
    category: &str,
) -> Result<bool, PdmError> {
    let result =
        sqlx::query("DELETE FROM part_number_prefixes WHERE org_id = $1 AND category = $2")
            .bind(org_id)
            .bind(category)
            .execute(pool)
            .await
            .map_err(PdmError::Db)?;
    Ok(result.rows_affected() > 0)
}

// ── Manufacturing registry (machines, post-processors, tools) ───────────

pub async fn list_machines(pool: &PgPool, org_id: Uuid) -> Result<Vec<Machine>, PdmError> {
    sqlx::query_as::<_, Machine>("SELECT * FROM machines WHERE org_id = $1 ORDER BY name")
        .bind(org_id)
        .fetch_all(pool)
        .await
        .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_machine(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    kind: &str,
    manufacturer: Option<&str>,
    model: Option<&str>,
    controller: Option<&str>,
    capabilities: Option<&serde_json::Value>,
    work_envelope: Option<&serde_json::Value>,
    axes: &[String],
    materials: &[String],
    maintenance_status: Option<&str>,
) -> Result<Machine, PdmError> {
    sqlx::query_as::<_, Machine>(
        "INSERT INTO machines (org_id, name, kind, manufacturer, model, controller, capabilities,
                               work_envelope, axes, materials, maintenance_status)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11) RETURNING *",
    )
    .bind(org_id)
    .bind(name)
    .bind(kind)
    .bind(manufacturer)
    .bind(model)
    .bind(controller)
    .bind(capabilities)
    .bind(work_envelope)
    .bind(axes)
    .bind(materials)
    .bind(maintenance_status)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_machine(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
) -> Result<Option<Machine>, PdmError> {
    sqlx::query_as::<_, Machine>("SELECT * FROM machines WHERE org_id = $1 AND id = $2")
        .bind(org_id)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(PdmError::Db)
}

pub async fn list_tools(pool: &PgPool, org_id: Uuid) -> Result<Vec<Tool>, PdmError> {
    sqlx::query_as::<_, Tool>("SELECT * FROM tools WHERE org_id = $1 ORDER BY name")
        .bind(org_id)
        .fetch_all(pool)
        .await
        .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_tool(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    kind: &str,
    description: Option<&str>,
    vendor: Option<&str>,
    part_number: Option<&str>,
    link: Option<&str>,
    units: &str,
    rotation: &str,
    flutes: Option<i32>,
    tool_material: Option<&str>,
    tool_number: Option<i32>,
    dimensions: Option<&serde_json::Value>,
    cutting_params: Option<&serde_json::Value>,
    material_compatibility: &[String],
    inventory: i32,
    tool_life: Option<&serde_json::Value>,
) -> Result<Tool, PdmError> {
    sqlx::query_as::<_, Tool>(
        "INSERT INTO tools (org_id, name, kind, description, vendor, part_number, link, units,
                            rotation, flutes, tool_material, tool_number, dimensions,
                            cutting_params, material_compatibility, inventory, tool_life)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)
         RETURNING *",
    )
    .bind(org_id)
    .bind(name)
    .bind(kind)
    .bind(description)
    .bind(vendor)
    .bind(part_number)
    .bind(link)
    .bind(units)
    .bind(rotation)
    .bind(flutes)
    .bind(tool_material)
    .bind(tool_number)
    .bind(dimensions)
    .bind(cutting_params)
    .bind(material_compatibility)
    .bind(inventory)
    .bind(tool_life)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_tool(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<Option<Tool>, PdmError> {
    sqlx::query_as::<_, Tool>("SELECT * FROM tools WHERE org_id = $1 AND id = $2")
        .bind(org_id)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn update_tool(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    name: &str,
    kind: &str,
    description: Option<&str>,
    vendor: Option<&str>,
    part_number: Option<&str>,
    link: Option<&str>,
    units: &str,
    rotation: &str,
    flutes: Option<i32>,
    tool_material: Option<&str>,
    tool_number: Option<i32>,
    dimensions: Option<&serde_json::Value>,
    cutting_params: Option<&serde_json::Value>,
    material_compatibility: &[String],
    inventory: i32,
    tool_life: Option<&serde_json::Value>,
) -> Result<Tool, PdmError> {
    sqlx::query_as::<_, Tool>(
        "UPDATE tools SET name = $3, kind = $4, description = $5, vendor = $6, part_number = $7,
                          link = $8, units = $9, rotation = $10, flutes = $11, tool_material = $12,
                          tool_number = $13, dimensions = $14, cutting_params = $15,
                          material_compatibility = $16, inventory = $17, tool_life = $18,
                          updated_at = now()
         WHERE org_id = $1 AND id = $2 RETURNING *",
    )
    .bind(org_id)
    .bind(id)
    .bind(name)
    .bind(kind)
    .bind(description)
    .bind(vendor)
    .bind(part_number)
    .bind(link)
    .bind(units)
    .bind(rotation)
    .bind(flutes)
    .bind(tool_material)
    .bind(tool_number)
    .bind(dimensions)
    .bind(cutting_params)
    .bind(material_compatibility)
    .bind(inventory)
    .bind(tool_life)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn delete_tool(pool: &PgPool, org_id: Uuid, id: Uuid) -> Result<(), PdmError> {
    let res = sqlx::query("DELETE FROM tools WHERE org_id = $1 AND id = $2")
        .bind(org_id)
        .bind(id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound("tool".into()));
    }
    Ok(())
}

// ── Tool cutting data (speeds + feedrates per material/finish) ──────────

#[allow(clippy::too_many_arguments)]
pub async fn create_cutting_data(
    pool: &PgPool,
    tool_id: Uuid,
    label: Option<&str>,
    material: Option<&str>,
    finish: Option<&str>,
    spindle_speed: Option<f64>,
    surface_speed: Option<f64>,
    ramp_spindle_speed: Option<f64>,
    cutting_feedrate: Option<f64>,
    feed_per_tooth: Option<f64>,
    lead_in_feedrate: Option<f64>,
    lead_out_feedrate: Option<f64>,
    transition_feedrate: Option<f64>,
    ramp_feedrate: Option<f64>,
    ramp_angle: Option<f64>,
    plunge_feedrate: Option<f64>,
    plunge_feed_per_rev: Option<f64>,
) -> Result<ToolCuttingData, PdmError> {
    sqlx::query_as::<_, ToolCuttingData>(
        "INSERT INTO tool_cutting_data (tool_id, label, material, finish, spindle_speed,
             surface_speed, ramp_spindle_speed, cutting_feedrate, feed_per_tooth,
             lead_in_feedrate, lead_out_feedrate, transition_feedrate, ramp_feedrate,
             ramp_angle, plunge_feedrate, plunge_feed_per_rev)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
         RETURNING *",
    )
    .bind(tool_id)
    .bind(label)
    .bind(material)
    .bind(finish)
    .bind(spindle_speed)
    .bind(surface_speed)
    .bind(ramp_spindle_speed)
    .bind(cutting_feedrate)
    .bind(feed_per_tooth)
    .bind(lead_in_feedrate)
    .bind(lead_out_feedrate)
    .bind(transition_feedrate)
    .bind(ramp_feedrate)
    .bind(ramp_angle)
    .bind(plunge_feedrate)
    .bind(plunge_feed_per_rev)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_cutting_data(
    pool: &PgPool,
    tool_id: Uuid,
) -> Result<Vec<ToolCuttingData>, PdmError> {
    sqlx::query_as::<_, ToolCuttingData>(
        "SELECT * FROM tool_cutting_data WHERE tool_id = $1 ORDER BY created_at",
    )
    .bind(tool_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn update_cutting_data(
    pool: &PgPool,
    tool_id: Uuid,
    id: Uuid,
    label: Option<&str>,
    material: Option<&str>,
    finish: Option<&str>,
    spindle_speed: Option<f64>,
    surface_speed: Option<f64>,
    ramp_spindle_speed: Option<f64>,
    cutting_feedrate: Option<f64>,
    feed_per_tooth: Option<f64>,
    lead_in_feedrate: Option<f64>,
    lead_out_feedrate: Option<f64>,
    transition_feedrate: Option<f64>,
    ramp_feedrate: Option<f64>,
    ramp_angle: Option<f64>,
    plunge_feedrate: Option<f64>,
    plunge_feed_per_rev: Option<f64>,
) -> Result<ToolCuttingData, PdmError> {
    sqlx::query_as::<_, ToolCuttingData>(
        "UPDATE tool_cutting_data SET label = $3, material = $4, finish = $5, spindle_speed = $6,
             surface_speed = $7, ramp_spindle_speed = $8, cutting_feedrate = $9, feed_per_tooth = $10,
             lead_in_feedrate = $11, lead_out_feedrate = $12, transition_feedrate = $13,
             ramp_feedrate = $14, ramp_angle = $15, plunge_feedrate = $16,
             plunge_feed_per_rev = $17, updated_at = now()
         WHERE tool_id = $1 AND id = $2 RETURNING *",
    )
    .bind(tool_id)
    .bind(id)
    .bind(label)
    .bind(material)
    .bind(finish)
    .bind(spindle_speed)
    .bind(surface_speed)
    .bind(ramp_spindle_speed)
    .bind(cutting_feedrate)
    .bind(feed_per_tooth)
    .bind(lead_in_feedrate)
    .bind(lead_out_feedrate)
    .bind(transition_feedrate)
    .bind(ramp_feedrate)
    .bind(ramp_angle)
    .bind(plunge_feedrate)
    .bind(plunge_feed_per_rev)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn delete_cutting_data(pool: &PgPool, tool_id: Uuid, id: Uuid) -> Result<(), PdmError> {
    let res = sqlx::query("DELETE FROM tool_cutting_data WHERE tool_id = $1 AND id = $2")
        .bind(tool_id)
        .bind(id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound("cutting data".into()));
    }
    Ok(())
}

// ── Tool documents ──────────────────────────────────────────────────────

pub async fn create_tool_document(
    pool: &PgPool,
    tool_id: Uuid,
    filename: &str,
    blob_id: Uuid,
    size_bytes: i64,
    content_type: Option<&str>,
) -> Result<ToolDocument, PdmError> {
    sqlx::query_as::<_, ToolDocument>(
        "INSERT INTO tool_documents (tool_id, filename, blob_id, size_bytes, content_type)
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(tool_id)
    .bind(filename)
    .bind(blob_id)
    .bind(size_bytes)
    .bind(content_type)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_tool_documents(
    pool: &PgPool,
    tool_id: Uuid,
) -> Result<Vec<ToolDocument>, PdmError> {
    sqlx::query_as::<_, ToolDocument>(
        "SELECT * FROM tool_documents WHERE tool_id = $1 ORDER BY created_at",
    )
    .bind(tool_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_tool_document(
    pool: &PgPool,
    tool_id: Uuid,
    id: Uuid,
) -> Result<Option<ToolDocument>, PdmError> {
    sqlx::query_as::<_, ToolDocument>("SELECT * FROM tool_documents WHERE tool_id = $1 AND id = $2")
        .bind(tool_id)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(PdmError::Db)
}

pub async fn delete_tool_document(pool: &PgPool, tool_id: Uuid, id: Uuid) -> Result<(), PdmError> {
    let res = sqlx::query("DELETE FROM tool_documents WHERE tool_id = $1 AND id = $2")
        .bind(tool_id)
        .bind(id)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    if res.rows_affected() == 0 {
        return Err(PdmError::NotFound("document".into()));
    }
    Ok(())
}

pub async fn list_post_processors(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<PostProcessor>, PdmError> {
    sqlx::query_as::<_, PostProcessor>(
        "SELECT * FROM post_processors WHERE org_id = $1 ORDER BY name",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn create_post_processor(
    pool: &PgPool,
    org_id: Uuid,
    name: &str,
    version: Option<&str>,
    dialect: Option<&str>,
    machine_kind: Option<&str>,
) -> Result<PostProcessor, PdmError> {
    sqlx::query_as::<_, PostProcessor>(
        "INSERT INTO post_processors (org_id, name, version, dialect, machine_kind)
         VALUES ($1, $2, $3, $4, $5) RETURNING *",
    )
    .bind(org_id)
    .bind(name)
    .bind(version)
    .bind(dialect)
    .bind(machine_kind)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

// ── Manufacturing execution chain (setup → CAM → NC → job → inspection) ─

pub async fn list_mfg_setups(pool: &PgPool, org_id: Uuid) -> Result<Vec<MfgSetup>, PdmError> {
    sqlx::query_as::<_, MfgSetup>(
        "SELECT * FROM mfg_setups WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_mfg_setup(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    file_version_id: Option<Uuid>,
    name: &str,
    stock: Option<&serde_json::Value>,
    workholding: Option<&str>,
    wcs: Option<&str>,
) -> Result<MfgSetup, PdmError> {
    let mut conn = pool.acquire().await.map_err(PdmError::Db)?;
    let mut refs = vec![OrgRef::Item(item_id)];
    refs.extend(file_version_id.map(|version_id| OrgRef::ItemVersion {
        item_id,
        version_id,
    }));
    require_org_refs(&mut conn, org_id, &refs).await?;
    sqlx::query_as::<_, MfgSetup>(
        "INSERT INTO mfg_setups (org_id, item_id, file_version_id, name, stock, workholding, wcs)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING *",
    )
    .bind(org_id)
    .bind(item_id)
    .bind(file_version_id)
    .bind(name)
    .bind(stock)
    .bind(workholding)
    .bind(wcs)
    .fetch_one(&mut *conn)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_cam_projects(pool: &PgPool, org_id: Uuid) -> Result<Vec<CamProject>, PdmError> {
    sqlx::query_as::<_, CamProject>(
        "SELECT * FROM cam_projects WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn create_cam_project(
    pool: &PgPool,
    org_id: Uuid,
    mfg_setup_id: Uuid,
    name: &str,
    source: Option<&str>,
    tool_ids: &[Uuid],
) -> Result<CamProject, PdmError> {
    let mut conn = pool.acquire().await.map_err(PdmError::Db)?;
    let mut refs = vec![OrgRef::MfgSetup(mfg_setup_id)];
    refs.extend(tool_ids.iter().copied().map(OrgRef::Tool));
    require_org_refs(&mut conn, org_id, &refs).await?;
    sqlx::query_as::<_, CamProject>(
        "INSERT INTO cam_projects (org_id, mfg_setup_id, name, source, tool_ids)
         VALUES ($1,$2,$3,$4,$5) RETURNING *",
    )
    .bind(org_id)
    .bind(mfg_setup_id)
    .bind(name)
    .bind(source)
    .bind(tool_ids)
    .fetch_one(&mut *conn)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_nc_programs(pool: &PgPool, org_id: Uuid) -> Result<Vec<NcProgram>, PdmError> {
    sqlx::query_as::<_, NcProgram>(
        "SELECT * FROM nc_programs WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_nc_program(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    file_version_id: Option<Uuid>,
    mfg_setup_id: Option<Uuid>,
    cam_project_id: Option<Uuid>,
    machine_id: Option<Uuid>,
    post_processor_id: Option<Uuid>,
    post_processor_version: Option<&str>,
    gcode_sha256: Option<&str>,
) -> Result<NcProgram, PdmError> {
    let mut conn = pool.acquire().await.map_err(PdmError::Db)?;
    let mut refs = vec![OrgRef::Item(item_id)];
    refs.extend(file_version_id.map(|version_id| OrgRef::ItemVersion {
        item_id,
        version_id,
    }));
    refs.extend(mfg_setup_id.map(OrgRef::MfgSetup));
    refs.extend(cam_project_id.map(OrgRef::CamProject));
    refs.extend(machine_id.map(OrgRef::Machine));
    refs.extend(post_processor_id.map(OrgRef::PostProcessor));
    require_org_refs(&mut conn, org_id, &refs).await?;
    sqlx::query_as::<_, NcProgram>(
        "INSERT INTO nc_programs (org_id, item_id, file_version_id, mfg_setup_id, cam_project_id,
            machine_id, post_processor_id, post_processor_version, gcode_sha256)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING *",
    )
    .bind(org_id)
    .bind(item_id)
    .bind(file_version_id)
    .bind(mfg_setup_id)
    .bind(cam_project_id)
    .bind(machine_id)
    .bind(post_processor_id)
    .bind(post_processor_version)
    .bind(gcode_sha256)
    .fetch_one(&mut *conn)
    .await
    .map_err(PdmError::Db)
}

pub async fn release_nc_program(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
) -> Result<NcProgram, PdmError> {
    sqlx::query_as::<_, NcProgram>(
        "UPDATE nc_programs SET released = TRUE, released_at = now() WHERE org_id = $1 AND id = $2 RETURNING *",
    )
    .bind(org_id)
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

/// Mark this item's NC programs that are **not** based on `new_version_id` as potentially
/// outdated — the source design just changed. Best-effort (returns the number flagged).
pub async fn flag_outdated_nc_programs(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    new_version_id: Uuid,
) -> Result<u64, PdmError> {
    let res = sqlx::query(
        "UPDATE nc_programs SET outdated = TRUE WHERE org_id = $1 AND item_id = $2
         AND (file_version_id IS NULL OR file_version_id <> $3)",
    )
    .bind(org_id)
    .bind(item_id)
    .bind(new_version_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(res.rows_affected())
}

pub async fn list_production_jobs(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<ProductionJob>, PdmError> {
    sqlx::query_as::<_, ProductionJob>(
        "SELECT * FROM production_jobs WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn create_production_job(
    pool: &PgPool,
    org_id: Uuid,
    nc_program_id: Uuid,
    machine_id: Option<Uuid>,
    operator: Option<&str>,
) -> Result<ProductionJob, PdmError> {
    let mut conn = pool.acquire().await.map_err(PdmError::Db)?;
    let mut refs = vec![OrgRef::NcProgram(nc_program_id)];
    refs.extend(machine_id.map(OrgRef::Machine));
    require_org_refs(&mut conn, org_id, &refs).await?;
    sqlx::query_as::<_, ProductionJob>(
        "INSERT INTO production_jobs (org_id, nc_program_id, machine_id, operator) VALUES ($1,$2,$3,$4) RETURNING *",
    )
    .bind(org_id)
    .bind(nc_program_id)
    .bind(machine_id)
    .bind(operator)
    .fetch_one(&mut *conn)
    .await
    .map_err(PdmError::Db)
}

pub async fn set_production_job_status(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    status: &str,
) -> Result<ProductionJob, PdmError> {
    sqlx::query_as::<_, ProductionJob>(
        "UPDATE production_jobs SET status = $3,
               completed_at = CASE WHEN $3 = 'done' THEN now() ELSE completed_at END
         WHERE org_id = $1 AND id = $2 RETURNING *",
    )
    .bind(org_id)
    .bind(id)
    .bind(status)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_inspections(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<InspectionRecord>, PdmError> {
    sqlx::query_as::<_, InspectionRecord>(
        "SELECT * FROM inspection_records WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn create_inspection(
    pool: &PgPool,
    org_id: Uuid,
    production_job_id: Option<Uuid>,
    item_id: Option<Uuid>,
    passed: Option<bool>,
    notes: Option<&str>,
    measured: Option<&serde_json::Value>,
) -> Result<InspectionRecord, PdmError> {
    let mut conn = pool.acquire().await.map_err(PdmError::Db)?;
    let refs: Vec<OrgRef> = production_job_id
        .map(OrgRef::ProductionJob)
        .into_iter()
        .chain(item_id.map(OrgRef::Item))
        .collect();
    require_org_refs(&mut conn, org_id, &refs).await?;
    sqlx::query_as::<_, InspectionRecord>(
        "INSERT INTO inspection_records (org_id, production_job_id, item_id, passed, notes, measured)
         VALUES ($1,$2,$3,$4,$5,$6) RETURNING *",
    )
    .bind(org_id)
    .bind(production_job_id)
    .bind(item_id)
    .bind(passed)
    .bind(notes)
    .bind(measured)
    .fetch_one(&mut *conn)
    .await
    .map_err(PdmError::Db)
}

// ── 3D printing + sheet metal (MFG/CAM remaining domains) ───────────────

pub async fn list_print_jobs(pool: &PgPool, org_id: Uuid) -> Result<Vec<PrintJob>, PdmError> {
    sqlx::query_as::<_, PrintJob>(
        "SELECT * FROM print_jobs WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_print_job(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    file_version_id: Option<Uuid>,
    printer_id: Option<Uuid>,
    material: Option<&str>,
    slicer_profile: Option<&str>,
    orientation: Option<&str>,
    supports: bool,
    layer_height_mm: Option<f64>,
    estimated_minutes: Option<i32>,
    machine_file_sha: Option<&str>,
) -> Result<PrintJob, PdmError> {
    let mut conn = pool.acquire().await.map_err(PdmError::Db)?;
    let mut refs = vec![OrgRef::Item(item_id)];
    refs.extend(file_version_id.map(|version_id| OrgRef::ItemVersion {
        item_id,
        version_id,
    }));
    refs.extend(printer_id.map(OrgRef::Machine));
    require_org_refs(&mut conn, org_id, &refs).await?;
    sqlx::query_as::<_, PrintJob>(
        "INSERT INTO print_jobs (org_id, item_id, file_version_id, printer_id, material, slicer_profile,
                                 orientation, supports, layer_height_mm, estimated_minutes, machine_file_sha)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *",
    )
    .bind(org_id)
    .bind(item_id)
    .bind(file_version_id)
    .bind(printer_id)
    .bind(material)
    .bind(slicer_profile)
    .bind(orientation)
    .bind(supports)
    .bind(layer_height_mm)
    .bind(estimated_minutes)
    .bind(machine_file_sha)
    .fetch_one(&mut *conn)
    .await
    .map_err(PdmError::Db)
}

pub async fn set_print_job_status(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    status: &str,
) -> Result<PrintJob, PdmError> {
    sqlx::query_as::<_, PrintJob>(
        "UPDATE print_jobs SET status = $3 WHERE org_id = $1 AND id = $2 RETURNING *",
    )
    .bind(org_id)
    .bind(id)
    .bind(status)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_sheet_metal(
    pool: &PgPool,
    org_id: Uuid,
) -> Result<Vec<SheetMetalSpec>, PdmError> {
    sqlx::query_as::<_, SheetMetalSpec>(
        "SELECT * FROM sheet_metal_specs WHERE org_id = $1 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_sheet_metal(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    file_version_id: Option<Uuid>,
    material: Option<&str>,
    thickness_mm: Option<f64>,
    bend_radius_mm: Option<f64>,
    k_factor: Option<f64>,
    bend_table: Option<&serde_json::Value>,
    flat_pattern_sha: Option<&str>,
    cutting_operation: Option<&str>,
    forming_operation: Option<&str>,
) -> Result<SheetMetalSpec, PdmError> {
    let mut conn = pool.acquire().await.map_err(PdmError::Db)?;
    let mut refs = vec![OrgRef::Item(item_id)];
    refs.extend(file_version_id.map(|version_id| OrgRef::ItemVersion {
        item_id,
        version_id,
    }));
    require_org_refs(&mut conn, org_id, &refs).await?;
    sqlx::query_as::<_, SheetMetalSpec>(
        "INSERT INTO sheet_metal_specs (org_id, item_id, file_version_id, material, thickness_mm, bend_radius_mm,
                                        k_factor, bend_table, flat_pattern_sha, cutting_operation, forming_operation)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *",
    )
    .bind(org_id)
    .bind(item_id)
    .bind(file_version_id)
    .bind(material)
    .bind(thickness_mm)
    .bind(bend_radius_mm)
    .bind(k_factor)
    .bind(bend_table)
    .bind(flat_pattern_sha)
    .bind(cutting_operation)
    .bind(forming_operation)
    .fetch_one(&mut *conn)
    .await
    .map_err(PdmError::Db)
}

/// Auto-flag an item as a sheet-metal part after analysis found sheet-metal
/// features in its design. Creates an empty spec row only when the item has
/// none yet, so it never overwrites a spec an engineer has filled in.
/// Returns `true` when a spec was created.
pub async fn auto_flag_sheet_metal(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
) -> Result<bool, PdmError> {
    if get_item_sheet_metal(pool, org_id, item_id).await?.is_some() {
        return Ok(false);
    }
    create_sheet_metal(
        pool, org_id, item_id, None, None, None, None, None, None, None, None, None,
    )
    .await?;
    Ok(true)
}

/// The item's most recent sheet-metal spec, if it has one.
pub async fn get_item_sheet_metal(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
) -> Result<Option<SheetMetalSpec>, PdmError> {
    sqlx::query_as::<_, SheetMetalSpec>(
        "SELECT * FROM sheet_metal_specs
         WHERE org_id = $1 AND item_id = $2
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(org_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// Set (or create) the item's sheet-metal spec. Updates the most recent
/// existing row when one is present; otherwise inserts a new row.
pub async fn upsert_item_sheet_metal(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    material: Option<&str>,
    thickness_mm: Option<f64>,
    bend_radius_mm: Option<f64>,
    k_factor: Option<f64>,
) -> Result<SheetMetalSpec, PdmError> {
    match get_item_sheet_metal(pool, org_id, item_id).await? {
        Some(s) => sqlx::query_as::<_, SheetMetalSpec>(
            "UPDATE sheet_metal_specs
                 SET material = $3, thickness_mm = $4, bend_radius_mm = $5, k_factor = $6,
                     updated_at = now()
                 WHERE id = $1 RETURNING *",
        )
        .bind(s.id)
        .bind(org_id)
        .bind(material)
        .bind(thickness_mm)
        .bind(bend_radius_mm)
        .bind(k_factor)
        .fetch_one(pool)
        .await
        .map_err(PdmError::Db),
        None => sqlx::query_as::<_, SheetMetalSpec>(
            "INSERT INTO sheet_metal_specs
                    (org_id, item_id, material, thickness_mm, bend_radius_mm, k_factor)
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
        )
        .bind(org_id)
        .bind(item_id)
        .bind(material)
        .bind(thickness_mm)
        .bind(bend_radius_mm)
        .bind(k_factor)
        .fetch_one(pool)
        .await
        .map_err(PdmError::Db),
    }
}

/// Record a flat-pattern DXF (content-addressed sha) on the item's
/// sheet-metal spec, creating the spec row when missing.
pub async fn set_item_flat_pattern(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    flat_pattern_sha: &str,
) -> Result<SheetMetalSpec, PdmError> {
    match get_item_sheet_metal(pool, org_id, item_id).await? {
        Some(s) => sqlx::query_as::<_, SheetMetalSpec>(
            "UPDATE sheet_metal_specs SET flat_pattern_sha = $3, updated_at = now()
                 WHERE id = $1 RETURNING *",
        )
        .bind(s.id)
        .bind(org_id)
        .bind(flat_pattern_sha)
        .fetch_one(pool)
        .await
        .map_err(PdmError::Db),
        None => sqlx::query_as::<_, SheetMetalSpec>(
            "INSERT INTO sheet_metal_specs (org_id, item_id, flat_pattern_sha)
                 VALUES ($1, $2, $3) RETURNING *",
        )
        .bind(org_id)
        .bind(item_id)
        .bind(flat_pattern_sha)
        .fetch_one(pool)
        .await
        .map_err(PdmError::Db),
    }
}

/// Clear the flat pattern from the item's sheet-metal spec.
pub async fn clear_item_flat_pattern(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
) -> Result<Option<SheetMetalSpec>, PdmError> {
    match get_item_sheet_metal(pool, org_id, item_id).await? {
        Some(s) => {
            let updated = sqlx::query_as::<_, SheetMetalSpec>(
                "UPDATE sheet_metal_specs SET flat_pattern_sha = NULL, updated_at = now()
                 WHERE id = $1 RETURNING *",
            )
            .bind(s.id)
            .bind(org_id)
            .fetch_one(pool)
            .await
            .map_err(PdmError::Db)?;
            Ok(Some(updated))
        }
        None => Ok(None),
    }
}

/// Resolve several items' flat patterns at once: for each item in order,
/// its part number, revision label, and the flat-pattern blob (if present).
#[derive(Debug, sqlx::FromRow)]
pub struct FlatPatternEntry {
    pub item_id: Uuid,
    pub part_number: String,
    pub revision: Option<String>,
    pub flat_pattern_sha: Option<String>,
    pub flat_pattern_size: Option<i64>,
}

pub async fn list_flat_patterns(
    pool: &PgPool,
    org_id: Uuid,
    item_ids: &[Uuid],
) -> Result<Vec<FlatPatternEntry>, PdmError> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: Vec<String> = item_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("${}", i + 1))
        .collect();
    let org_param = item_ids.len() + 1;
    let spec_sha_sql = format!(
        "(SELECT sm.flat_pattern_sha FROM sheet_metal_specs sm
          WHERE sm.item_id = i.id AND sm.org_id = ${org_param}
          ORDER BY sm.created_at DESC LIMIT 1)"
    );
    let sql = format!(
        "SELECT i.id AS item_id, i.part_number,
                (SELECT MAX(fr.label) FROM file_revisions fr WHERE fr.item_id = i.id) AS revision,
                {spec_sha_sql} AS flat_pattern_sha,
                (SELECT fb.size_bytes FROM file_blobs fb
                 WHERE fb.org_id = ${org_param} AND fb.sha256 = {spec_sha_sql}) AS flat_pattern_size
         FROM items i
         WHERE i.org_id = ${org_param} AND i.id IN ({placeholders})",
        placeholders = placeholders.join(", ")
    );
    let mut q = sqlx::query_as::<_, FlatPatternEntry>(&sql);
    for id in item_ids {
        q = q.bind(*id);
    }
    q = q.bind(org_id).bind(org_id);
    q.fetch_all(pool).await.map_err(PdmError::Db)
}

// ── Laser nesting (Sparrow integration) ─────────────────────────────────

/// Inputs for creating a nesting run (the settings snapshot).
#[derive(Debug, Clone)]
pub struct LaserNestParams {
    pub name: String,
    pub project_id: Option<Uuid>,
    pub machine_id: Option<Uuid>,
    pub material: Option<String>,
    pub thickness_mm: Option<f64>,
    pub stock_width_mm: f64,
    pub stock_height_mm: f64,
    pub edge_margin_mm: f64,
    pub part_gap_mm: f64,
    pub spacing_x_mm: f64,
    pub spacing_y_mm: f64,
    pub grain_direction: String,
    pub nesting_corner: String,
    pub origin_x_mm: f64,
    pub origin_y_mm: f64,
    pub sheet_sheet_distance_mm: f64,
    pub guillotine: bool,
    pub add_part_labels: bool,
    pub fill_direction: String,
    pub auto_sheet_origins: bool,
    pub fill_all_sheets: bool,
    pub optimized_pairs: bool,
    pub sheet_defs: Vec<pdm_core::SheetDef>,
    pub chord_tol_mm: f64,
    pub join_tol_mm: f64,
    pub default_part_unit: String,
    pub allowed_rotations: Vec<f64>,
    pub time_budget_sec: i32,
    pub attempt_limit_sec: i32,
    pub max_sheets: i32,
    pub seed: Option<i64>,
    pub solver_version: Option<String>,
    pub engine_build: Option<String>,
    pub created_by: Option<Uuid>,
    pub parent_nest_id: Option<Uuid>,
}

/// A source part for a nesting run (exact version + flat-pattern snapshot).
#[derive(Debug, Clone)]
pub struct LaserNestPartInput {
    pub item_id: Uuid,
    pub file_version_id: Uuid,
    pub artifact_id: Option<Uuid>,
    pub dxf_sha: String,
    pub dxf_size: i64,
    pub part_number: String,
    pub revision_label: Option<String>,
    pub quantity: i32,
    pub layer_cut: String,
    pub layers_mark: Vec<String>,
    pub unit: String,
    pub mirror: bool,
    pub allow_mirror: bool,
    pub part_grain: String,
    pub ordinal: i32,
    pub step_angle: f64,
    pub priority: i32,
    pub label: String,
    pub use_bounding_box: bool,
    pub common_edge: bool,
    pub min_qty: i32,
    pub filler_qty: i32,
}

/// Create a nesting run: the nest row, its part snapshots, and the queued
/// `laser.nest` job — atomically. Returns the nest and the job.
pub async fn create_laser_nest_run(
    pool: &PgPool,
    org_id: Uuid,
    params: &LaserNestParams,
    parts: &[LaserNestPartInput],
) -> Result<(pdm_core::LaserNest, pdm_core::Job), PdmError> {
    if parts.is_empty() {
        return Err(PdmError::Validation(
            "a nest needs at least one part".into(),
        ));
    }
    let mut tx = pool.begin().await.map_err(PdmError::Db)?;
    let refs: Vec<OrgRef> = params
        .project_id
        .map(OrgRef::Project)
        .into_iter()
        .chain(params.machine_id.map(OrgRef::Machine))
        .collect();
    require_org_refs(&mut tx, org_id, &refs).await?;

    let nest: pdm_core::LaserNest = sqlx::query_as(
        r#"INSERT INTO laser_nests (
              org_id, project_id, name, machine_id, material, thickness_mm,
              stock_width_mm, stock_height_mm, edge_margin_mm, part_gap_mm,
              spacing_x_mm, spacing_y_mm, grain_direction,
              nesting_corner, origin_x_mm, origin_y_mm, sheet_sheet_distance_mm,
              guillotine, add_part_labels, fill_direction, auto_sheet_origins,
              fill_all_sheets, optimized_pairs, sheet_defs,
              chord_tol_mm, join_tol_mm, default_part_unit, allowed_rotations,
              time_budget_sec, attempt_limit_sec, max_sheets, seed,
              solver_version, engine_build, created_by, parent_nest_id)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,
                   $18,$19,$20,$21,$22,$23,$24,$25,$26,$27,$28,$29,$30,$31,$32,$33,$34,$35,$36)
           RETURNING *"#,
    )
    .bind(org_id)
    .bind(params.project_id)
    .bind(&params.name)
    .bind(params.machine_id)
    .bind(&params.material)
    .bind(params.thickness_mm)
    .bind(params.stock_width_mm)
    .bind(params.stock_height_mm)
    .bind(params.edge_margin_mm)
    .bind(params.part_gap_mm)
    .bind(params.spacing_x_mm)
    .bind(params.spacing_y_mm)
    .bind(&params.grain_direction)
    .bind(&params.nesting_corner)
    .bind(params.origin_x_mm)
    .bind(params.origin_y_mm)
    .bind(params.sheet_sheet_distance_mm)
    .bind(params.guillotine)
    .bind(params.add_part_labels)
    .bind(&params.fill_direction)
    .bind(params.auto_sheet_origins)
    .bind(params.fill_all_sheets)
    .bind(params.optimized_pairs)
    .bind(serde_json::json!(params.sheet_defs))
    .bind(params.chord_tol_mm)
    .bind(params.join_tol_mm)
    .bind(&params.default_part_unit)
    .bind(serde_json::json!(params.allowed_rotations))
    .bind(params.time_budget_sec)
    .bind(params.attempt_limit_sec)
    .bind(params.max_sheets)
    .bind(params.seed)
    .bind(&params.solver_version)
    .bind(&params.engine_build)
    .bind(params.created_by)
    .bind(params.parent_nest_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(PdmError::Db)?;

    for p in parts {
        sqlx::query(
            r#"INSERT INTO laser_nest_parts (
                 nest_id, org_id, item_id, file_version_id, artifact_id,
                 dxf_sha, dxf_size, part_number, revision_label, quantity,
                 layer_cut, layers_mark, unit, mirror, allow_mirror, part_grain, ordinal,
                 step_angle, priority, label, use_bounding_box, common_edge, min_qty, filler_qty)
              VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,
                      $18,$19,$20,$21,$22,$23,$24)"#,
        )
        .bind(nest.id)
        .bind(org_id)
        .bind(p.item_id)
        .bind(p.file_version_id)
        .bind(p.artifact_id)
        .bind(&p.dxf_sha)
        .bind(p.dxf_size)
        .bind(&p.part_number)
        .bind(&p.revision_label)
        .bind(p.quantity)
        .bind(&p.layer_cut)
        .bind(serde_json::json!(p.layers_mark))
        .bind(&p.unit)
        .bind(p.mirror)
        .bind(p.allow_mirror)
        .bind(&p.part_grain)
        .bind(p.ordinal)
        .bind(p.step_angle)
        .bind(p.priority)
        .bind(&p.label)
        .bind(p.use_bounding_box)
        .bind(p.common_edge)
        .bind(p.min_qty)
        .bind(p.filler_qty)
        .execute(&mut *tx)
        .await
        .map_err(PdmError::Db)?;
    }

    let payload = serde_json::json!({ "nest_id": nest.id });
    let job: pdm_core::Job = sqlx::query_as(
        "INSERT INTO jobs (org_id, kind, payload) VALUES ($1, 'laser.nest', $2)
         RETURNING id, org_id, kind, payload, status, attempts, max_attempts, last_error, created_at",
    )
    .bind(org_id)
    .bind(&payload)
    .fetch_one(&mut *tx)
    .await
    .map_err(PdmError::Db)?;

    sqlx::query("UPDATE laser_nests SET job_id = $2 WHERE id = $1")
        .bind(nest.id)
        .bind(job.id)
        .execute(&mut *tx)
        .await
        .map_err(PdmError::Db)?;

    tx.commit().await.map_err(PdmError::Db)?;
    let mut nest = nest;
    nest.job_id = Some(job.id);
    Ok((nest, job))
}

pub async fn get_laser_nest(
    pool: &PgPool,
    id: Uuid,
    org_id: Uuid,
) -> Result<Option<pdm_core::LaserNest>, PdmError> {
    sqlx::query_as::<_, pdm_core::LaserNest>(
        "SELECT * FROM laser_nests WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_laser_nests(
    pool: &PgPool,
    org_id: Uuid,
    project_id: Option<Uuid>,
    limit: i32,
) -> Result<Vec<pdm_core::LaserNest>, PdmError> {
    let sql = format!(
        "SELECT * FROM laser_nests WHERE org_id = $1 {} ORDER BY created_at DESC LIMIT ${}",
        if project_id.is_some() {
            "AND project_id = $2"
        } else {
            ""
        },
        if project_id.is_some() { 3 } else { 2 }
    );
    let mut q = sqlx::query_as::<_, pdm_core::LaserNest>(&sql).bind(org_id);
    if let Some(p) = project_id {
        q = q.bind(p);
    }
    q = q.bind(limit);
    q.fetch_all(pool).await.map_err(PdmError::Db)
}

/// Transition a nest's status. `completed` sets completed_at. Idempotent-safe:
/// a terminal nest (completed/cancelled) is not overwritten by running/queued.
/// Record the hash of a finished run's manifest blob (`pdm.laserNestManifest`).
pub async fn set_laser_nest_manifest(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    manifest_sha: &str,
) -> Result<(), PdmError> {
    sqlx::query("UPDATE laser_nests SET manifest_sha = $3 WHERE org_id = $1 AND id = $2")
        .bind(org_id)
        .bind(id)
        .bind(manifest_sha)
        .execute(pool)
        .await
        .map_err(PdmError::Db)?;
    Ok(())
}

pub async fn set_laser_nest_status(
    pool: &PgPool,
    org_id: Uuid,
    id: Uuid,
    status: &str,
    error: Option<&str>,
) -> Result<Option<pdm_core::LaserNest>, PdmError> {
    let terminal = matches!(status, "completed" | "failed" | "cancelled");
    // Never overwrite a user-cancelled nest; a terminal target may settle a
    // queued/running/failed nest, a non-terminal one may only advance it.
    sqlx::query_as::<_, pdm_core::LaserNest>(
        r#"UPDATE laser_nests SET
             status = $3,
             error = CASE WHEN $4::text IS NOT NULL THEN $4 ELSE error END,
             completed_at = CASE WHEN $3 IN ('completed','failed') THEN now() ELSE completed_at END
          WHERE org_id = $1 AND id = $2
            AND status <> 'cancelled'
            AND ($5 OR status NOT IN ('completed','failed'))
          RETURNING *"#,
    )
    .bind(org_id)
    .bind(id)
    .bind(status)
    .bind(error)
    .bind(terminal)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// Mark nests that reference a given CAD version as stale (a newer revision or
/// a replaced flat pattern invalidates the result, but does not rewrite it).
pub async fn mark_laser_nests_stale_for_version(
    pool: &PgPool,
    org_id: Uuid,
    version_id: Uuid,
    reason: &str,
) -> Result<u64, PdmError> {
    let res = sqlx::query(
        r#"UPDATE laser_nests SET stale = TRUE, stale_reason = $3
          WHERE org_id = $1 AND status NOT IN ('cancelled')
            AND id IN (
               SELECT nest_id FROM laser_nest_parts
               WHERE org_id = $1 AND file_version_id = $2
            )"#,
    )
    .bind(org_id)
    .bind(version_id)
    .bind(reason)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(res.rows_affected())
}

pub async fn list_laser_nest_parts(
    pool: &PgPool,
    nest_id: Uuid,
    org_id: Uuid,
) -> Result<Vec<pdm_core::LaserNestPart>, PdmError> {
    sqlx::query_as::<_, pdm_core::LaserNestPart>(
        "SELECT * FROM laser_nest_parts WHERE nest_id = $1 AND org_id = $2
         ORDER BY ordinal, item_id",
    )
    .bind(nest_id)
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_laser_nest_sheets(
    pool: &PgPool,
    nest_id: Uuid,
    org_id: Uuid,
) -> Result<Vec<pdm_core::LaserNestSheet>, PdmError> {
    sqlx::query_as::<_, pdm_core::LaserNestSheet>(
        "SELECT * FROM laser_nest_sheets WHERE nest_id = $1 AND org_id = $2 ORDER BY sheet_index",
    )
    .bind(nest_id)
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_laser_nest_sheet(
    pool: &PgPool,
    nest_id: Uuid,
    org_id: Uuid,
    sheet_index: i32,
) -> Result<Option<pdm_core::LaserNestSheet>, PdmError> {
    sqlx::query_as::<_, pdm_core::LaserNestSheet>(
        "SELECT * FROM laser_nest_sheets WHERE nest_id = $1 AND org_id = $2 AND sheet_index = $3",
    )
    .bind(nest_id)
    .bind(org_id)
    .bind(sheet_index)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_laser_nest_placements(
    pool: &PgPool,
    nest_id: Uuid,
    org_id: Uuid,
) -> Result<Vec<pdm_core::LaserNestPlacement>, PdmError> {
    sqlx::query_as::<_, pdm_core::LaserNestPlacement>(
        r#"SELECT pl.* FROM laser_nest_placements pl
           JOIN laser_nest_sheets sh ON sh.id = pl.sheet_id
           WHERE pl.nest_id = $1 AND pl.org_id = $2
           ORDER BY sh.sheet_index, pl.part_index, pl.copy_index"#,
    )
    .bind(nest_id)
    .bind(org_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

/// The full nest detail (nest + parts + sheets + placements).
pub async fn get_laser_nest_detail(
    pool: &PgPool,
    nest_id: Uuid,
    org_id: Uuid,
) -> Result<Option<pdm_core::LaserNestDetail>, PdmError> {
    let nest = match get_laser_nest(pool, nest_id, org_id).await? {
        Some(n) => n,
        None => return Ok(None),
    };
    let parts = list_laser_nest_parts(pool, nest_id, org_id).await?;
    let sheets = list_laser_nest_sheets(pool, nest_id, org_id).await?;
    let placements = list_laser_nest_placements(pool, nest_id, org_id).await?;
    Ok(Some(pdm_core::LaserNestDetail {
        nest,
        parts,
        sheets,
        placements,
    }))
}

// Sheet outputs (written by the worker after a validated solve).

pub async fn create_laser_nest_sheet(
    pool: &PgPool,
    org_id: Uuid,
    nest_id: Uuid,
    sheet_index: i32,
) -> Result<pdm_core::LaserNestSheet, PdmError> {
    sqlx::query_as::<_, pdm_core::LaserNestSheet>(
        r#"INSERT INTO laser_nest_sheets (nest_id, org_id, sheet_index)
           VALUES ($1,$2,$3) RETURNING *"#,
    )
    .bind(nest_id)
    .bind(org_id)
    .bind(sheet_index)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn set_laser_nest_sheet_output(
    pool: &PgPool,
    org_id: Uuid,
    sheet_id: Uuid,
    dxf_sha: &str,
    dxf_size: i64,
    preview_sha: Option<&str>,
    strip_width_mm: Option<f64>,
    utilization: Option<f64>,
    solver_input_sha: Option<&str>,
    solver_output_sha: Option<&str>,
    placed_count: i32,
) -> Result<(), PdmError> {
    sqlx::query(
        r#"UPDATE laser_nest_sheets SET
             dxf_sha = $3, dxf_size = $4, preview_sha = $5,
             strip_width_mm = $6, utilization = $7,
             solver_input_sha = $8, solver_output_sha = $9, placed_count = $10
          WHERE org_id = $1 AND id = $2"#,
    )
    .bind(org_id)
    .bind(sheet_id)
    .bind(dxf_sha)
    .bind(dxf_size)
    .bind(preview_sha)
    .bind(strip_width_mm)
    .bind(utilization)
    .bind(solver_input_sha)
    .bind(solver_output_sha)
    .bind(placed_count)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn insert_laser_nest_placement(
    pool: &PgPool,
    org_id: Uuid,
    sheet_id: Uuid,
    nest_id: Uuid,
    part_index: i32,
    copy_index: i32,
    item_id: Uuid,
    file_version_id: Uuid,
    reflected: bool,
    rotation_deg: f64,
    tx: f64,
    ty: f64,
) -> Result<(), PdmError> {
    sqlx::query(
        r#"INSERT INTO laser_nest_placements
             (sheet_id, nest_id, org_id, part_index, copy_index,
              item_id, file_version_id, reflected, rotation_deg, tx, ty)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)"#,
    )
    .bind(sheet_id)
    .bind(nest_id)
    .bind(org_id)
    .bind(part_index)
    .bind(copy_index)
    .bind(item_id)
    .bind(file_version_id)
    .bind(reflected)
    .bind(rotation_deg)
    .bind(tx)
    .bind(ty)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

// Revision-bound flat-pattern artifacts.

#[allow(clippy::too_many_arguments)]
pub async fn create_flat_pattern_artifact(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    file_version_id: Uuid,
    sha256: &str,
    size_bytes: i64,
    source_kind: &str,
    generator: &str,
    unit: &str,
    notes: &str,
) -> Result<pdm_core::FlatPatternArtifact, PdmError> {
    sqlx::query_as::<_, pdm_core::FlatPatternArtifact>(
        r#"INSERT INTO flat_pattern_artifacts
             (org_id, item_id, file_version_id, sha256, size_bytes,
              source_kind, generator, unit, notes)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING *"#,
    )
    .bind(org_id)
    .bind(item_id)
    .bind(file_version_id)
    .bind(sha256)
    .bind(size_bytes)
    .bind(source_kind)
    .bind(generator)
    .bind(unit)
    .bind(notes)
    .fetch_one(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn get_flat_pattern_artifact(
    pool: &PgPool,
    id: Uuid,
    org_id: Uuid,
) -> Result<Option<pdm_core::FlatPatternArtifact>, PdmError> {
    sqlx::query_as::<_, pdm_core::FlatPatternArtifact>(
        "SELECT * FROM flat_pattern_artifacts WHERE id = $1 AND org_id = $2",
    )
    .bind(id)
    .bind(org_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// The newest artifact for an item's exact version (for nest part resolution).
pub async fn latest_flat_pattern_artifact(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
    file_version_id: Uuid,
) -> Result<Option<pdm_core::FlatPatternArtifact>, PdmError> {
    sqlx::query_as::<_, pdm_core::FlatPatternArtifact>(
        r#"SELECT * FROM flat_pattern_artifacts
           WHERE org_id = $1 AND item_id = $2 AND file_version_id = $3
           ORDER BY created_at DESC LIMIT 1"#,
    )
    .bind(org_id)
    .bind(item_id)
    .bind(file_version_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

pub async fn list_flat_pattern_artifacts(
    pool: &PgPool,
    org_id: Uuid,
    item_id: Uuid,
) -> Result<Vec<pdm_core::FlatPatternArtifact>, PdmError> {
    sqlx::query_as::<_, pdm_core::FlatPatternArtifact>(
        "SELECT * FROM flat_pattern_artifacts WHERE org_id = $1 AND item_id = $2 ORDER BY created_at DESC",
    )
    .bind(org_id)
    .bind(item_id)
    .fetch_all(pool)
    .await
    .map_err(PdmError::Db)
}

// ── Sheet-metal standards library (MFG reference data) ──────────────────

/// Idempotently seed an organization's sheet-metal standards library from the
/// global template catalog. Safe to call on every read; copies nothing when
/// the organization already has rows for a material x thickness.
pub async fn ensure_sheet_material_standards(pool: &PgPool, org_id: Uuid) -> Result<(), PdmError> {
    sqlx::query(
        "INSERT INTO sheet_material_standards (org_id, material_group, material, thickness_mm,
                                               thickness_gauge, density_g_cm3, min_bend_radius_mm,
                                               k_factor, bend_deduction_mm, bend_allowance_mm, notes)
         SELECT $1, material_group, material, thickness_mm, thickness_gauge, density_g_cm3,
                min_bend_radius_mm, k_factor, bend_deduction_mm, bend_allowance_mm, notes
         FROM sheet_material_standard_templates
         ON CONFLICT (org_id, material, thickness_mm) DO NOTHING",
    )
    .bind(org_id)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

/// List the organization's sheet-metal standards. `unit` filters by unit
/// system: `Some("saes")` (or `"imperial"`) returns only rows that carry a US
/// gauge label (`thickness_gauge IS NOT NULL`), i.e. the SAE/imperial set;
/// `Some("metric")` or `None` returns the full catalog (thicknesses are
/// always stored in millimetres, gauge labels being the SAE view of the same
/// physical thicknesses).
pub async fn list_sheet_material_standards(
    pool: &PgPool,
    org_id: Uuid,
    unit: Option<&str>,
) -> Result<Vec<SheetMaterialStandard>, PdmError> {
    let saes = matches!(
        unit.map(str::to_ascii_lowercase).as_deref(),
        Some("saes") | Some("imperial") | Some("in")
    );
    let sql = if saes {
        "SELECT * FROM sheet_material_standards
         WHERE org_id = $1 AND thickness_gauge IS NOT NULL
         ORDER BY material_group, material, thickness_mm"
    } else {
        "SELECT * FROM sheet_material_standards WHERE org_id = $1
         ORDER BY material_group, material, thickness_mm"
    };
    sqlx::query_as::<_, SheetMaterialStandard>(sql)
        .bind(org_id)
        .fetch_all(pool)
        .await
        .map_err(PdmError::Db)
}

#[allow(clippy::too_many_arguments)]
pub async fn create_sheet_material_standard(
    pool: &PgPool,
    org_id: Uuid,
    material_group: &str,
    material: &str,
    thickness_mm: f64,
    thickness_gauge: Option<&str>,
    density_g_cm3: Option<f64>,
    min_bend_radius_mm: Option<f64>,
    k_factor: Option<f64>,
    bend_deduction_mm: Option<f64>,
    bend_allowance_mm: Option<f64>,
    notes: Option<&str>,
) -> Result<SheetMaterialStandard, PdmError> {
    let note = notes.unwrap_or("");
    sqlx::query_as::<_, SheetMaterialStandard>(
        "INSERT INTO sheet_material_standards (org_id, material_group, material, thickness_mm,
                                               thickness_gauge, density_g_cm3, min_bend_radius_mm,
                                               k_factor, bend_deduction_mm, bend_allowance_mm, notes)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING *",
    )
    .bind(org_id)
    .bind(material_group)
    .bind(material)
    .bind(thickness_mm)
    .bind(thickness_gauge)
    .bind(density_g_cm3)
    .bind(min_bend_radius_mm)
    .bind(k_factor)
    .bind(bend_deduction_mm)
    .bind(bend_allowance_mm)
    .bind(note)
    .fetch_one(pool)
    .await
    .map_err(|e| {
        if e.to_string().contains("duplicate key") {
            PdmError::Conflict(format!(
                "standard for {material} at {thickness_mm} mm already exists"
            ))
        } else {
            PdmError::Db(e)
        }
    })
}

// ── Client settings ─────────────────────────────────────────────────────

/// The client app's settings for a user (SolidCraft's Prefs JSON), if any.
pub async fn get_client_settings(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<serde_json::Value>, PdmError> {
    sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT payload FROM client_settings WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
    .map_err(PdmError::Db)
}

/// Upsert the client app's settings for a user (one row per user, last write wins).
pub async fn put_client_settings(
    pool: &PgPool,
    user_id: Uuid,
    payload: &serde_json::Value,
) -> Result<(), PdmError> {
    sqlx::query(
        "INSERT INTO client_settings (user_id, payload) VALUES ($1, $2)
         ON CONFLICT (user_id) DO UPDATE SET payload = EXCLUDED.payload, updated_at = now()",
    )
    .bind(user_id)
    .bind(payload)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    Ok(())
}

// ── Server secrets ──────────────────────────────────────────────────────

/// The server-wide secret `name`: `len` random bytes, made by whichever process asks first and
/// then kept in the database, so every API process on this database gets the same value and it
/// survives restarts. Deleting the row makes a new one at the next start.
pub async fn server_secret(pool: &PgPool, name: &str, len: usize) -> Result<Vec<u8>, PdmError> {
    use rand::RngCore;
    let mut fresh = vec![0u8; len];
    rand::rng().fill_bytes(&mut fresh);
    sqlx::query(
        "INSERT INTO server_secrets (name, secret) VALUES ($1, $2) ON CONFLICT (name) DO NOTHING",
    )
    .bind(name)
    .bind(&fresh)
    .execute(pool)
    .await
    .map_err(PdmError::Db)?;
    sqlx::query_scalar("SELECT secret FROM server_secrets WHERE name = $1")
        .bind(name)
        .fetch_one(pool)
        .await
        .map_err(PdmError::Db)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    #[tokio::test]
    #[ignore = "requires PDM_TEST_DATABASE_URL pointing to an isolated Postgres database"]
    async fn account_updates_preserve_an_active_admin() -> Result<(), Box<dyn std::error::Error>> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&std::env::var("PDM_TEST_DATABASE_URL")?)
            .await?;
        migrate(&pool).await?;
        let org = create_org(&pool, &format!("admin-test-{}", Uuid::new_v4())).await?;
        let first = create_user(&pool, org.id, "first@test.dev", "First", "admin", None).await?;

        for (role, active) in [("admin", false), ("viewer", true)] {
            let result = update_user(&pool, org.id, first.id, "First", role, active).await;
            assert!(matches!(result, Err(PdmError::Conflict(_))));
        }
        // Profile edits remain possible for the sole administrator.
        let edited = update_user(&pool, org.id, first.id, "Renamed", "admin", true).await?;
        assert_eq!(edited.display_name, "Renamed");
        assert!(matches!(
            update_user(&pool, org.id, Uuid::new_v4(), "", "viewer", false).await,
            Err(PdmError::NotFound(_))
        ));

        let second = create_user(&pool, org.id, "second@test.dev", "Second", "admin", None).await?;
        let (a, b) = tokio::join!(
            update_user(&pool, org.id, first.id, "First", "admin", false),
            update_user(&pool, org.id, second.id, "Second", "viewer", true),
        );
        assert!(
            (a.is_ok() && matches!(&b, Err(PdmError::Conflict(_))))
                || (b.is_ok() && matches!(&a, Err(PdmError::Conflict(_))))
        );
        let users = list_users(&pool, org.id).await?;
        assert_eq!(
            users
                .iter()
                .filter(|u| u.active && u.role == "admin")
                .count(),
            1
        );
        let restored = update_user(&pool, org.id, first.id, "First", "admin", true).await?;
        assert!(restored.active);
        update_user(&pool, org.id, second.id, "Second", "viewer", true).await?;

        let other = create_org(&pool, &format!("other-test-{}", Uuid::new_v4())).await?;
        assert!(matches!(
            update_user(&pool, other.id, first.id, "", "viewer", false).await,
            Err(PdmError::NotFound(_))
        ));
        sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
            .bind(vec![org.id, other.id])
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires PDM_TEST_DATABASE_URL pointing to an isolated Postgres database"]
    async fn writes_refuse_another_organizations_ids() -> Result<(), Box<dyn std::error::Error>> {
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&std::env::var("PDM_TEST_DATABASE_URL")?)
            .await?;
        migrate(&pool).await?;
        let stamp = Uuid::new_v4();
        let a = create_org(&pool, &format!("refs-a-{stamp}")).await?;
        let b = create_org(&pool, &format!("refs-b-{stamp}")).await?;
        let user_b = create_user(&pool, b.id, "b@test.invalid", "B", "editor", None).await?;
        let project_a = create_project(&pool, a.id, "A", "").await?;
        let project_b = create_project(&pool, b.id, "B", "").await?;
        let folder_a = create_folder(&pool, a.id, project_a.id, None, "A").await?;
        let item_a = create_item(&pool, a.id, project_a.id, None, "A-1", "", "BE", None).await?;
        let item_b = create_item(&pool, b.id, project_b.id, None, "B-1", "", "BE", None).await?;
        let machine_a = create_machine(
            &pool,
            a.id,
            "A",
            "printer",
            None,
            None,
            None,
            None,
            None,
            &[],
            &[],
            None,
        )
        .await?;
        let not_found = |r: Result<(), PdmError>| matches!(r, Err(PdmError::NotFound(_)));

        // Items go only into the writer's own projects and folders.
        let r = create_item(&pool, b.id, project_a.id, None, "B-2", "", "BE", None).await;
        assert!(not_found(r.map(|_| ())), "item in another org's project");
        let r = create_item(
            &pool,
            b.id,
            project_b.id,
            Some(folder_a.id),
            "B-3",
            "",
            "BE",
            None,
        );
        assert!(
            not_found(r.await.map(|_| ())),
            "item in another org's folder"
        );

        // Properties, comments, approval requests and releases only touch own items.
        let v = serde_json::json!("x");
        let r = upsert_property(&pool, b.id, item_a.id, "k", "text", &v).await;
        assert!(not_found(r.map(|_| ())), "property");
        let r = add_comment(&pool, b.id, item_a.id, user_b.id, "x").await;
        assert!(not_found(r.map(|_| ())), "comment");
        let r = create_approval_request(&pool, b.id, item_a.id, Some(user_b.id), &[]).await;
        assert!(not_found(r.map(|_| ())), "approval request");
        // A request an older server stored for another org's item is neither listed nor decided.
        let legacy: Uuid = sqlx::query_scalar(
            "INSERT INTO approval_requests (org_id, item_id, status, assignees)
             VALUES ($1, $2, 'pending', '[]'::jsonb) RETURNING id",
        )
        .bind(b.id)
        .bind(item_a.id)
        .fetch_one(&pool)
        .await?;
        let listed = list_approval_requests(&pool, b.id, None, None, 1, 50).await?;
        assert!(listed.items.iter().all(|r| r.id != legacy), "legacy listed");
        assert_eq!(listed.total, 0);
        let r = decide_approval_request(&pool, b.id, legacy, user_b.id, true, "approve", "").await;
        assert!(not_found(r.map(|_| ())), "decide a legacy request");
        let decisions: i64 =
            sqlx::query_scalar("SELECT count(*) FROM approval_decisions WHERE request_id = $1")
                .bind(legacy)
                .fetch_one(&pool)
                .await?;
        assert_eq!(decisions, 0);
        let r = release_revision(
            &pool,
            b.id,
            item_a.id,
            "X",
            Uuid::new_v4(),
            None,
            "",
            "alpha",
        );
        assert!(not_found(r.await.map(|_| ())), "release another org's item");
        // A version that is not the item's leaves no revision and no state change behind.
        let r = release_revision(
            &pool,
            a.id,
            item_a.id,
            "A",
            Uuid::new_v4(),
            None,
            "",
            "alpha",
        );
        assert!(
            not_found(r.await.map(|_| ())),
            "release with a foreign version"
        );
        assert!(list_revisions(&pool, item_a.id).await?.is_empty());
        let item = get_item(&pool, item_a.id, a.id).await?.ok_or("item")?;
        assert_eq!(item.lifecycle_state, "wip");

        // Manufacturing records only point at the writer's own rows.
        let r = create_mfg_setup(&pool, b.id, item_a.id, None, "x", None, None, None).await;
        assert!(not_found(r.map(|_| ())), "setup for another org's item");
        let r = create_print_job(
            &pool,
            b.id,
            item_b.id,
            None,
            Some(machine_a.id),
            None,
            None,
            None,
            false,
            None,
            None,
            None,
        )
        .await;
        assert!(
            not_found(r.map(|_| ())),
            "print job on another org's printer"
        );
        let r = create_sheet_metal(
            &pool, b.id, item_a.id, None, None, None, None, None, None, None, None, None,
        )
        .await;
        assert!(
            not_found(r.map(|_| ())),
            "sheet metal for another org's item"
        );

        // The same writes inside one organization still work.
        create_mfg_setup(&pool, a.id, item_a.id, None, "own", None, None, None).await?;
        upsert_property(&pool, a.id, item_a.id, "k", "text", &v).await?;
        create_print_job(
            &pool,
            a.id,
            item_a.id,
            None,
            Some(machine_a.id),
            None,
            None,
            None,
            false,
            None,
            None,
            None,
        )
        .await?;

        sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
            .bind(vec![a.id, b.id])
            .execute(&pool)
            .await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires PDM_TEST_DATABASE_URL pointing to an isolated Postgres database"]
    async fn branding_is_kept_per_organization() -> Result<(), Box<dyn std::error::Error>> {
        use pdm_core::branding::{BrandingSettings, LOGO, LogoInfo};
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&std::env::var("PDM_TEST_DATABASE_URL")?)
            .await?;
        migrate(&pool).await?;
        let a_name = format!("branding-a-{}", Uuid::new_v4());
        let a = create_org(&pool, &a_name).await?;
        let b = create_org(&pool, &format!("branding-b-{}", Uuid::new_v4())).await?;

        // Settings round-trip, validated and normalised on the way in.
        let none = BrandingSettings::default();
        assert_eq!(get_branding(&pool, a.id).await?, none);
        let mut wanted = BrandingSettings::default();
        wanted.apply_patch(&serde_json::json!({
            "display_name": "Example Works",
            "show_name_with_logo": false,
            "colors": { "dark": { "acc": "#3B82F6" } }
        }))?;
        set_branding(&pool, a.id, &wanted).await?;
        let got = get_branding(&pool, a.id).await?;
        assert_eq!(got, wanted);
        assert_eq!(got.colors["dark"]["acc"], "#3b82f6");
        let mut invalid = BrandingSettings::default();
        invalid
            .colors
            .entry("dark".into())
            .or_default()
            .insert("acc".into(), "red".into());
        assert!(matches!(
            set_branding(&pool, a.id, &invalid).await,
            Err(PdmError::BadRequest(_))
        ));
        // Other settings keys are untouched, and the other organization sees nothing.
        set_preview_style(&pool, a.id, "viewport").await?;
        assert_eq!(get_branding(&pool, a.id).await?, wanted);
        assert_eq!(get_branding(&pool, b.id).await?, none);

        // A partial update merges into what is stored; a refused one stores nothing.
        let (stored, change) = update_branding(
            &pool,
            a.id,
            &serde_json::json!({ "colors": { "light": { "acc": "#1D4ED8" } } }),
        )
        .await?;
        assert_eq!(change.themes, vec!["light"]);
        assert_eq!(stored.display_name.as_deref(), Some("Example Works"));
        assert_eq!(get_branding(&pool, a.id).await?, stored);
        let refused = update_branding(
            &pool,
            a.id,
            &serde_json::json!({ "colors": { "dark": { "acc": "red" } } }),
        )
        .await;
        assert!(matches!(refused, Err(PdmError::BadRequest(_))));
        assert_eq!(get_branding(&pool, a.id).await?, stored);
        set_branding(&pool, a.id, &wanted).await?;

        // A stored entry that no longer passes (e.g. a token since dropped from the allow-list)
        // is dropped on its own; the rest of the branding is kept.
        sqlx::query(
            "UPDATE organizations SET settings = jsonb_set(settings, '{branding,colors,dark,retired-token}', '\"#000000\"') WHERE id = $1",
        )
        .bind(a.id)
        .execute(&pool)
        .await?;
        assert_eq!(get_branding(&pool, a.id).await?, wanted);

        // One row per (org, kind): a second upload replaces the first.
        let info = LogoInfo {
            mime: "image/png",
            width: 480,
            height: 120,
        };
        put_branding_asset(&pool, a.id, LOGO, &info, b"first", "1111", None).await?;
        let webp = LogoInfo {
            mime: "image/webp",
            ..info
        };
        let meta = put_branding_asset(&pool, a.id, LOGO, &webp, b"second!", "2222", None).await?;
        assert_eq!((meta.mime.as_str(), meta.size_bytes), ("image/webp", 7));
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM org_branding_assets WHERE org_id = $1")
                .bind(a.id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(rows, 1);
        let asset = get_branding_asset(&pool, a.id, LOGO).await?;
        assert!(asset.is_some_and(|x| x.bytes == b"second!" && x.sha256 == "2222"));
        assert!(get_branding_asset_meta(&pool, b.id, LOGO).await?.is_none());
        // The database refuses formats and sizes the API would never send.
        let svg = LogoInfo {
            mime: "image/svg+xml",
            ..info
        };
        let refused = put_branding_asset(&pool, b.id, LOGO, &svg, b"x", "3", None).await;
        assert!(refused.is_err());
        let big = vec![0u8; 262_145];
        let refused = put_branding_asset(&pool, b.id, LOGO, &info, &big, "4", None).await;
        assert!(refused.is_err());

        assert!(delete_branding_asset(&pool, a.id, LOGO).await?);
        assert!(!delete_branding_asset(&pool, a.id, LOGO).await?);
        put_branding_asset(&pool, a.id, LOGO, &info, b"third", "5555", None).await?;
        clear_branding(&pool, a.id).await?;
        assert_eq!(get_branding(&pool, a.id).await?, none);
        assert!(get_branding_asset_meta(&pool, a.id, LOGO).await?.is_none());
        assert_eq!(get_preview_style(&pool, a.id).await?, "viewport");

        // Sign-in organization: a name picks that organization; an unknown name or (with at
        // least two organizations here) no name picks none. The single-organization case
        // depends on the whole database and is covered by the live end-to-end test.
        assert_eq!(signin_org(&pool, Some(&a_name)).await?, Some(a.id));
        assert_eq!(signin_org(&pool, Some("no-such-organization")).await?, None);
        assert_eq!(signin_org(&pool, None).await?, None);

        // Deleting an organization deletes its images.
        put_branding_asset(&pool, b.id, LOGO, &info, b"b-logo", "6666", None).await?;
        sqlx::query("DELETE FROM organizations WHERE id = ANY($1)")
            .bind(vec![a.id, b.id])
            .execute(&pool)
            .await?;
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM org_branding_assets WHERE org_id = ANY($1)")
                .bind(vec![a.id, b.id])
                .fetch_one(&pool)
                .await?;
        assert_eq!(left, 0);
        Ok(())
    }

    /// Connect to a live Postgres for integration tests; `None` when
    /// `PDM_TEST_DATABASE_URL` is unset (tests skip gracefully).
    async fn live_pool() -> Option<PgPool> {
        let url = std::env::var("PDM_TEST_DATABASE_URL").ok()?;
        PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .ok()
    }

    #[tokio::test]
    async fn store_thumbnail_persists_on_version() {
        let Some(pool) = live_pool().await else {
            eprintln!("PDM_TEST_DATABASE_URL not set; skipping store_thumbnail test");
            return;
        };
        if migrate(&pool).await.is_err() {
            return;
        }
        let stamp = Uuid::new_v4().to_string();

        let Some(org) = create_org(&pool, &format!("thumb-test-{stamp}")).await.ok() else {
            eprintln!("setup: create_org failed");
            return;
        };
        let Some(user) = create_user(
            &pool,
            org.id,
            &format!("t-{stamp}@test.dev"),
            "Thumb Test",
            "editor",
            None,
        )
        .await
        .ok() else {
            eprintln!("setup: create_user failed");
            return;
        };
        let Some(project) = create_project(&pool, org.id, &format!("thumb-proj-{stamp}"), "")
            .await
            .ok()
        else {
            eprintln!("setup: create_project failed");
            return;
        };
        let Some(item) = create_item(
            &pool,
            org.id,
            project.id,
            None,
            &format!("BE-THUMB-{stamp}"),
            "",
            "BE",
            Some(user.id),
        )
        .await
        .ok() else {
            eprintln!("setup: create_item failed");
            return;
        };
        let Some(file) = ensure_file(
            &pool,
            item.id,
            "thumb.solvecraft",
            "application/octet-stream",
        )
        .await
        .ok() else {
            eprintln!("setup: ensure_file failed");
            return;
        };
        let Some(blob) = upload_blob(
            &pool,
            org.id,
            "1111111111111111111111111111111111111111111111111111111111111111",
            1,
            "1111111111111111111111111111111111111111111111111111111111111111",
        )
        .await
        .ok() else {
            eprintln!("setup: upload_blob failed");
            return;
        };
        let Some(version) = create_version(
            &pool,
            file.id,
            blob.id,
            user.id,
            "thumb test",
            None,
            "",
            false,
        )
        .await
        .ok() else {
            eprintln!("setup: create_version failed");
            return;
        };

        assert!(
            store_thumbnail(
                &pool,
                version.id,
                "2222222222222222222222222222222222222222222222222222222222222222",
                4321
            )
            .await
            .is_ok()
        );

        let got = get_version(&pool, version.id).await.ok().flatten();
        let Some(got) = got else {
            eprintln!("re-read version after store_thumbnail failed");
            return;
        };
        assert_eq!(
            got.thumbnail_sha256.as_deref(),
            Some("2222222222222222222222222222222222222222222222222222222222222222")
        );
        assert_eq!(got.thumbnail_size_bytes, Some(4321));

        // The joined version-detail query must surface the thumbnail SHA too.
        let details = list_version_details(&pool, file.id).await.ok();
        let Some(details) = details else {
            eprintln!("list_version_details failed");
            return;
        };
        let head = details.first();
        assert!(head.is_some_and(|d| d.thumbnail_sha256.as_deref()
            == Some("2222222222222222222222222222222222222222222222222222222222222222")));
    }
}
