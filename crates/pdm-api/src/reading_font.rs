//! Organization default reading font: `GET/PUT /settings/reading-font`.
//!
//! Any member can read it: the portal uses it for members who have not picked a font of their
//! own. Only admins change it. Values are the faces the portal bundles
//! (`pdm_core::reading_font`). Changes are audited as `org.reading_font_updated`.

use axum::Json;
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::routing::{MethodRouter, get};
use bytes::Bytes;
use serde_json::Value;

use pdm_core::PdmError;
use pdm_core::reading_font::ReadingFont;

use crate::auth::{Authed, require_user};
use crate::error::AppError;
use crate::state::AppState;

/// A `{ ui, mono }` body is a few dozen bytes; anything larger is refused before it is read.
const MAX_BODY_BYTES: usize = 4 * 1024;

/// The route's handlers, with the body limit.
pub fn method_router() -> MethodRouter<AppState> {
    get(get_reading_font)
        .put(put_reading_font)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

/// GET /settings/reading-font → `{ ui, mono }` (default `system` for both).
pub async fn get_reading_font(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
) -> Result<Json<Value>, AppError> {
    let font = pdm_db::get_reading_font(&state.pool, authed.org_id).await?;
    Ok(Json(font.to_json()))
}

/// PUT /settings/reading-font `{ ui, mono }` (admin) → the stored value. The role is checked
/// before the body is parsed, so a member who is not an admin always gets 403.
pub async fn put_reading_font(
    State(state): State<AppState>,
    Extension(authed): Extension<Authed>,
    body: Bytes,
) -> Result<Json<Value>, AppError> {
    let user = require_user(&authed)?;
    if user.role != "admin" {
        return Err(AppError(PdmError::Forbidden("admin only".into())));
    }
    let body: Value = serde_json::from_slice(&body)
        .map_err(|e| AppError(PdmError::BadRequest(format!("reading font: {e}"))))?;
    let font = ReadingFont::parse(&body)?;
    let stored = pdm_db::set_reading_font(&state.pool, authed.org_id, &font).await?;
    pdm_db::record_audit(
        &state.pool,
        authed.org_id,
        Some(user.id),
        "",
        "org.reading_font_updated",
        "organization",
        Some(authed.org_id),
        Some(stored.to_json()),
    )
    .await?;
    Ok(Json(stored.to_json()))
}
