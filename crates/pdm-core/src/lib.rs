//! PDM core: domain entities, errors, and pure business logic.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use thiserror::Error;
use uuid::Uuid;

pub mod branding;
pub mod gcode;
pub mod reading_font;

// ── Error ────────────────────────────────────────────────────────────────

#[derive(Error, Debug)]
pub enum PdmError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("authentication failed")]
    AuthFailed,
    /// A web sign-in that was idle too long or reached its lifetime.
    #[error("your session expired — sign in again")]
    SessionExpired,
    #[error("bad credentials")]
    BadCredentials,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("integrity mismatch: expected {expected}, got {actual}")]
    IntegrityMismatch { expected: String, actual: String },
    #[error("validation error: {0}")]
    Validation(String),
    #[error("database error: {0}")]
    Db(#[source] sqlx::Error),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl PdmError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Conflict(_) => 409,
            Self::AuthFailed => 401,
            Self::SessionExpired => 401,
            Self::BadCredentials => 401,
            Self::Forbidden(_) => 403,
            Self::BadRequest(_) => 400,
            Self::IntegrityMismatch { .. } => 422,
            Self::Validation(_) => 422,
            Self::Db(_) => 500,
            Self::Storage(_) => 502,
            Self::Internal(_) => 500,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::AuthFailed => "auth_failed",
            Self::SessionExpired => "session_expired",
            Self::BadCredentials => "bad_credentials",
            Self::Forbidden(_) => "forbidden",
            Self::BadRequest(_) => "bad_request",
            Self::IntegrityMismatch { .. } => "integrity_mismatch",
            Self::Validation(_) => "validation",
            Self::Db(_) => "db_error",
            Self::Storage(_) => "storage_error",
            Self::Internal(_) => "internal",
        }
    }
}

// ── Entities ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Organization {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    pub id: Uuid,
    pub org_id: Uuid,
    pub email: String,
    pub display_name: String,
    /// 'viewer' | 'editor' | 'admin' (Phase-1 simplification of the roles model).
    pub role: String,
    pub active: bool,
    /// Argon2id PHC hash of the user's password, if any.
    /// Never serialized in JSON responses.
    #[serde(skip_serializing, default)]
    pub password_hash: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApiToken {
    pub id: Uuid,
    pub org_id: Uuid,
    pub user_id: Option<Uuid>,
    pub name: String,
    pub scopes: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Project {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    /// Soft-delete marker: `Some(_)` when the project is archived, `None` when live.
    pub archived_at: Option<DateTime<Utc>>,
    /// Optional cover image as a `data:image/...` URL (resized JPEG from the web UI).
    pub image_data: Option<String>,
    /// The main assembly of the project: the root item the bill of materials is
    /// built off. Its rendered thumbnail is also used as the project cover image.
    pub main_assembly_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Folder {
    pub id: Uuid,
    pub org_id: Uuid,
    pub project_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Item {
    pub id: Uuid,
    pub org_id: Uuid,
    pub project_id: Uuid,
    pub folder_id: Option<Uuid>,
    pub part_number: String,
    pub description: String,
    pub classification: String,
    pub doc_id: Option<Uuid>,
    pub lifecycle_state: String,
    pub owner_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Soft-delete marker: `Some(_)` when the item is archived, `None` when live.
    pub archived_at: Option<DateTime<Utc>>,
}

/// An [`Item`] enriched with denormalized summary fields, as returned by the
/// listing endpoints (`GET /projects/{id}/items`, `GET /search`) and
/// `GET /items/{id}`. All extra fields are nullable.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ItemSummary {
    pub id: Uuid,
    pub org_id: Uuid,
    pub project_id: Uuid,
    pub folder_id: Option<Uuid>,
    pub part_number: String,
    pub description: String,
    pub classification: String,
    pub doc_id: Option<Uuid>,
    pub lifecycle_state: String,
    pub owner_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Soft-delete marker: `Some(_)` when the item is archived, `None` when live.
    pub archived_at: Option<DateTime<Utc>>,
    /// Display name (falling back to email) of the user in `owner_id`.
    pub owner_name: Option<String>,
    /// `committed_at` of the head (max seq) file version.
    pub modified_at: Option<DateTime<Utc>>,
    /// Display name (falling back to email) of the head version's author.
    pub modified_by: Option<String>,
    /// Highest revision label for the item from `file_revisions`, if any.
    pub latest_revision: Option<String>,
    /// Display name (falling back to email) of the user holding a live lock.
    pub locked_by: Option<String>,
    /// Seq of the head (max seq) file version, if any.
    pub head_version_seq: Option<i32>,
    /// Major version of the head file version (bumped only on "save as new version").
    pub head_version: Option<i32>,
    /// Per-save revision of the head file version within its major version.
    pub head_revision: Option<i32>,
    /// Filename recorded on the item's file, if a file exists.
    pub filename: Option<String>,
    /// MIME type recorded at check-in, if a file exists.
    pub mime_type: Option<String>,
    /// True when the item has a `sheet_metal_specs` row (it is a sheet-metal part).
    pub sheet_metal: Option<bool>,
    /// Material of the item's sheet-metal spec, if any.
    pub sheet_metal_material: Option<String>,
    /// Thickness in mm of the item's sheet-metal spec, if any.
    pub sheet_metal_thickness_mm: Option<f64>,
    /// True when the item's sheet-metal spec has a generated flat-pattern DXF.
    pub sheet_metal_flat_pattern: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FileRecord {
    pub id: Uuid,
    pub item_id: Uuid,
    pub filename: String,
    pub mime_type: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Blob {
    pub id: Uuid,
    pub org_id: Uuid,
    pub sha256: String,
    pub size_bytes: i64,
    pub storage_key: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FileVersion {
    pub id: Uuid,
    pub file_id: Uuid,
    /// Global monotonic per-save counter (head = max seq; used for stale-base checks).
    pub seq: i32,
    /// Major version, bumped only when the user checks in "as a new version".
    pub version: i32,
    /// Per-save revision within the major version (V2 - R5 = fifth save in version 2).
    pub revision: i32,
    pub blob_id: Uuid,
    pub author_id: Uuid,
    pub committed_at: DateTime<Utc>,
    pub commit_message: String,
    pub base_version_id: Option<Uuid>,
    pub native_format: String,
    pub measurements: Option<serde_json::Value>,
    /// SHA-256 hex of the rendered thumbnail PNG, set after `preview.render` succeeds.
    pub thumbnail_sha256: Option<String>,
    /// Byte size of the rendered thumbnail PNG.
    pub thumbnail_size_bytes: Option<i64>,
    /// Clear reason no thumbnail was produced (the CLI could not open or convert the
    /// file), or `None` while the preview is pending / deliberately skipped.
    #[serde(default)]
    pub thumbnail_error: Option<String>,
}

/// A version with its blob's SHA-256 and byte size joined in, for listing and the portal.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct VersionDetail {
    pub id: Uuid,
    pub file_id: Uuid,
    pub seq: i32,
    pub version: i32,
    pub revision: i32,
    pub blob_id: Uuid,
    pub author_id: Uuid,
    pub committed_at: DateTime<Utc>,
    pub commit_message: String,
    pub base_version_id: Option<Uuid>,
    pub native_format: String,
    pub sha256: String,
    pub size_bytes: i64,
    /// SHA-256 hex of the rendered thumbnail PNG, if one has been produced.
    pub thumbnail_sha256: Option<String>,
    /// Clear reason no thumbnail was produced (the CLI could not open or convert the
    /// file), if one has been recorded.
    #[serde(default)]
    pub thumbnail_error: Option<String>,
    /// SHA-256 hex of the converted STEP, if the `cad.convert` job stored one
    /// (Inventor, FreeCAD, Rhino, STEP and IGES files reopen from it).
    #[serde(default)]
    pub derived_step_sha256: Option<String>,
    /// Size of that converted STEP in bytes.
    #[serde(default)]
    pub derived_step_size_bytes: Option<i64>,
    #[serde(default)]
    pub derived_solidcraft_sha256: Option<String>,
    #[serde(default)]
    pub derived_solidcraft_size_bytes: Option<i64>,
    #[serde(default)]
    pub derived_solidcraft_error: Option<String>,
    #[serde(default)]
    pub derived_solidcraft_job_id: Option<Uuid>,
    /// MIME type of the item's file as recorded at check-in.
    pub mime_type: String,
    /// Version-specific filename (older clients can still use the item's file name).
    #[serde(default)]
    pub filename: String,
    /// Display name (falling back to email) of the version's author.
    pub author_name: String,
}

/// File extensions `solidcraft-cli convert` reads (lower case): the B-rep CAD formats the PDM's
/// `cad.convert` job restates as STEP on check-in.
pub const CAD_CONVERT_EXTS: [&str; 8] =
    ["ipt", "iam", "fcstd", "3dm", "step", "stp", "igs", "iges"];

/// Does this file name (any case) end in one of [`CAD_CONVERT_EXTS`]?
pub fn is_cad_convert_path(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| CAD_CONVERT_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

// ── Preview planning ─────────────────────────────────────────────────────

/// File extensions the headless CLI opens directly (lower case): native designs,
/// STEP/IGES, FreeCAD/Rhino/Inventor (read through cadmpeg) and meshes.
/// `snapshot` (and `eval`) take these as-is.
pub const PREVIEW_DIRECT_EXTS: [&str; 17] = [
    "sc",
    "solvecraft",
    "solidcraft",
    "json",
    "step",
    "stp",
    "igs",
    "iges",
    "fcstd",
    "3dm",
    "ipt",
    "iam",
    "3mf",
    "stl",
    "obj",
    "glb",
    "gltf",
];

/// B-rep CAD extensions the CLI *may* restate as STEP through `convert` (lower case):
/// the commercial formats SolidCraft reads natively or is gaining (SolidWorks, CATIA,
/// NX, Pro/E, ACIS, …). A file with one of these gets a best-effort preview: the CLI
/// tries it directly, falls back to converting it to STEP, and if neither works the
/// version records why instead of showing an empty preview.
pub const PREVIEW_CONVERT_EXTS: [&str; 15] = [
    "sldprt", "sldasm", "slddrw", "cgr", "prt", "x_t", "f3d", "sat", "sab", "smt", "smb", "idw",
    "ipj", "jt", "wrl",
];

/// What the `preview.render` job should do with a checked-in file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewPlan {
    /// The CLI opens the file directly: `snapshot` (and `eval`) it as-is.
    Direct,
    /// The CLI may read the file as B-rep CAD: restate it as STEP through `convert`
    /// first, then `snapshot` the STEP.
    Convert,
    /// Not a CAD file the CLI reads (documents, drawings, images, no extension, …):
    /// leave a clean "no preview" state.
    Skip,
}

/// Choose a [`PreviewPlan`] from a file name (extension, any case).
pub fn preview_plan(filename: &str) -> PreviewPlan {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    if ext.is_empty() {
        return PreviewPlan::Skip;
    }
    if PREVIEW_DIRECT_EXTS.contains(&ext.as_str()) {
        return PreviewPlan::Direct;
    }
    if PREVIEW_CONVERT_EXTS.contains(&ext.as_str()) {
        return PreviewPlan::Convert;
    }
    PreviewPlan::Skip
}

/// Feature-timeline types the SolidCraft engine reports for sheet-metal
/// features (`solidcraft-cli eval` `timeline[].type`). A design whose
/// timeline carries any of these was built with sheet-metal tools, so the
/// PDM can flag its item as a sheet-metal part on its own.
pub const SHEET_METAL_FEATURE_TYPES: &[&str] = &[
    "BaseFlangeFeature",
    "ContourFlangeFeature",
    "EdgeFlangeFeature",
    "PathFlangeFeature",
    "HemFeature",
    "PerforateFeature",
    "FoldFeature",
    "UnfoldFeature",
    "RefoldFeature",
    "ConvertToSheetMetalFeature",
    "CoilFeature",
];

/// True when `solvecraft-cli eval` measurements JSON contains a sheet-metal
/// feature in the design's timeline. Imported B-rep files (STEP and the like)
/// carry only a `BaseFeature` timeline and are never flagged by this.
pub fn measurements_have_sheet_metal_features(measurements: &serde_json::Value) -> bool {
    measurements
        .get("timeline")
        .and_then(|t| t.as_array())
        .is_some_and(|timeline| {
            timeline.iter().any(|f| {
                f.get("type")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| SHEET_METAL_FEATURE_TYPES.contains(&t))
            })
        })
}

/// File extensions Dutchman (the dutchman-cad/dutchman release) decodes into a
/// SolidCraft `.sc` design, keeping editable feature history where it maps
/// (lower case): Fusion, Inventor, SolidWorks, FreeCAD, CATIA, Rhino,
/// Creo View, ACIS, STEP and Onshape bundles.
pub const DUTCHMAN_EXTS: [&str; 17] = [
    "f3d",
    "f3z",
    "ipt",
    "iam",
    "sldprt",
    "sldasm",
    "fcstd",
    "catpart",
    "pvz",
    "3dm",
    "sat",
    "sab",
    "smt",
    "smb",
    "step",
    "stp",
    "onshape-bundle",
];

/// Organization Dutchman settings: which release channel to convert with, and
/// which conversion options to pass to the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DutchmanSettings {
    /// Release channel: `latest` (newest stable), `nightly`, or a pinned tag
    /// such as `v0.2.9` / `onshape-preview-2`.
    pub channel: String,
    /// `fast` | `standard` | `full`.
    pub preset: String,
    /// `smart` | `last-shown` | `first-placed`.
    pub part_positions: String,
    /// Verify a rebuilt feature history in the SolidCraft CLI.
    pub verify: bool,
    /// Wall-clock limit for one conversion in seconds; `0` = the worker default.
    pub timeout_secs: u64,
    /// `dutchman onshape fetch --max-calls`; `0` = the default of 200.
    pub onshape_max_calls: u32,
}

impl Default for DutchmanSettings {
    fn default() -> Self {
        Self {
            channel: "latest".into(),
            preset: "standard".into(),
            part_positions: "smart".into(),
            verify: true,
            timeout_secs: 0,
            onshape_max_calls: 0,
        }
    }
}

/// The org's saved defaults for the laser-nest create panel. Every field is
/// optional so a partial set pre-fills only what was saved.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestDefaults {
    /// Unit of measure for the sheet/part dimensional fields ("in" | "mm").
    #[serde(default)]
    pub units: Option<String>,
    #[serde(default)]
    pub length: Option<f64>,
    #[serde(default)]
    pub width: Option<f64>,
    #[serde(default)]
    pub margin: Option<f64>,
    #[serde(default)]
    pub spacing_x: Option<f64>,
    #[serde(default)]
    pub spacing_y: Option<f64>,
    #[serde(default)]
    pub grain_direction: Option<String>,
    #[serde(default)]
    pub nesting_corner: Option<String>,
    #[serde(default)]
    pub guillotine: Option<bool>,
    #[serde(default)]
    pub add_part_labels: Option<bool>,
    #[serde(default)]
    pub part_unit: Option<String>,
}

impl DutchmanSettings {
    /// Reject values the CLI would refuse or the update logic cannot reason about.
    pub fn validate(&self) -> Result<(), PdmError> {
        let ok_channel = self.channel == "latest"
            || self.channel == "nightly"
            || (4..=40).contains(&self.channel.len())
                && self.channel.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-')
                })
                && self
                    .channel
                    .bytes()
                    .any(|b| b.is_ascii_digit() || b == b'n');
        if !ok_channel {
            return Err(PdmError::BadRequest(
                "dutchman channel must be 'latest', 'nightly' or a release tag (v0.2.9, onshape-preview-2)".into(),
            ));
        }
        match self.preset.as_str() {
            "fast" | "standard" | "full" => {}
            _ => {
                return Err(PdmError::BadRequest(
                    "dutchman preset must be fast, standard or full".into(),
                ));
            }
        }
        match self.part_positions.as_str() {
            "smart" | "last-shown" | "first-placed" => {}
            _ => {
                return Err(PdmError::BadRequest(
                    "dutchman part_positions must be smart, last-shown or first-placed".into(),
                ));
            }
        }
        if self.timeout_secs > 0 && !(60..=7200).contains(&self.timeout_secs) {
            return Err(PdmError::BadRequest(
                "dutchman timeout_secs must be 0 or between 60 and 7200".into(),
            ));
        }
        if self.onshape_max_calls != 0 && !(10..=10000).contains(&self.onshape_max_calls) {
            return Err(PdmError::BadRequest(
                "dutchman onshape_max_calls must be 0 or between 10 and 10000".into(),
            ));
        }
        Ok(())
    }

    /// `--max-calls` for `dutchman onshape fetch` (0 stores the default of 200).
    pub fn onshape_call_budget(&self) -> u32 {
        if self.onshape_max_calls == 0 {
            200
        } else {
            self.onshape_max_calls
        }
    }
}

/// Mesh extensions the SolidCraft CLI restates into a design (lower case); meshes
/// carry no history, so Dutchman does not handle them.
pub const MESH_EXTS: [&str; 5] = ["stl", "obj", "3mf", "glb", "gltf"];

/// Which converter turns a checked-in file into a native `.sc` artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScConverter {
    /// Dutchman: decodes native CAD into a SolidCraft design with feature history.
    Dutchman,
    /// The SolidCraft CLI: restates meshes and IGES into a design (geometry only).
    SolidcraftCli,
}

/// Pick the converter from a file name (extension, any case): Dutchman for the
/// native formats it decodes, the SolidCraft CLI for the rest it admits.
pub fn sc_converter(filename: &str) -> Option<ScConverter> {
    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if DUTCHMAN_EXTS.contains(&ext.as_str()) {
        return Some(ScConverter::Dutchman);
    }
    if CAD_CONVERT_EXTS.contains(&ext.as_str()) || MESH_EXTS.contains(&ext.as_str()) {
        return Some(ScConverter::SolidcraftCli);
    }
    None
}

/// Formats admitted for conversion into a native `.sc` artifact. Scripts and
/// existing native designs are deliberately not offered as CAD conversions.
pub fn can_convert_to_solidcraft(filename: &str) -> bool {
    sc_converter(filename).is_some()
}

/// A safe downloadable name, retaining the source version in the derived file.
pub fn solidcraft_artifact_filename(filename: &str, seq: i32) -> String {
    let base = filename.rsplit(['/', '\\']).next().unwrap_or("design");
    let stem = std::path::Path::new(base)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("design");
    let safe: String = stem
        .chars()
        .take(120)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_matches([' ', '.']);
    format!(
        "{}-v{seq}.sc",
        if safe.is_empty() { "design" } else { safe }
    )
}

pub fn version_filename(filename: &str, native_format: &str) -> String {
    if !native_format.is_empty()
        && native_format.len() <= 16
        && native_format.bytes().all(|b| b.is_ascii_alphanumeric())
    {
        std::path::Path::new(filename)
            .with_extension(native_format.to_ascii_lowercase())
            .to_string_lossy()
            .into_owned()
    } else {
        filename.to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FileRevision {
    pub id: Uuid,
    pub item_id: Uuid,
    pub label: String,
    pub version_id: Uuid,
    pub released_by: Option<Uuid>,
    pub released_at: DateTime<Utc>,
    pub release_note: String,
    pub scheme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FileLock {
    pub id: Uuid,
    pub item_id: Uuid,
    pub user_id: Uuid,
    pub workstation: String,
    pub base_version_id: Option<Uuid>,
    pub acquired_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AuditEvent {
    pub id: i64,
    pub org_id: Uuid,
    pub actor_id: Option<Uuid>,
    pub agent_id: String,
    pub action: String,
    pub object_type: String,
    pub object_id: Option<Uuid>,
    pub detail: Option<serde_json::Value>,
    pub at: DateTime<Utc>,
}

/// A comment on an item (web workspace discussion).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ItemComment {
    pub id: Uuid,
    pub item_id: Uuid,
    pub user_id: Uuid,
    /// Display name (falling back to email) of the commenting user.
    pub author_name: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

// ── Manufacturing registry (Machines & Tools) ────────────────────────────

/// A machine in the manufacturing registry (Machines & Tools).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Machine {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub kind: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub controller: Option<String>,
    pub capabilities: Option<serde_json::Value>,
    pub work_envelope: Option<serde_json::Value>,
    pub axes: Vec<String>,
    pub materials: Vec<String>,
    pub maintenance_status: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A post-processor (G-code / dialect generator) targeting a machine kind.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct PostProcessor {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub version: Option<String>,
    pub dialect: Option<String>,
    pub machine_kind: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A reusable tool in the tool library.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Tool {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub kind: String,
    /// General: free-text description.
    pub description: Option<String>,
    /// General: tool vendor.
    pub vendor: Option<String>,
    /// General: vendor part number.
    pub part_number: Option<String>,
    /// General: link (vendor page, datasheet, ...).
    pub link: Option<String>,
    /// Cutter: measurement system for the geometry values.
    pub units: String,
    /// Cutter: rotation direction.
    pub rotation: String,
    /// Cutter: number of flutes / cutting edges.
    pub flutes: Option<i32>,
    /// Cutter: the tool's own material (carbide, HSS, ...).
    pub tool_material: Option<String>,
    /// Post processor: the tool number assigned on the machine.
    pub tool_number: Option<i32>,
    /// Cutter: geometry specific to the tool kind.
    pub dimensions: Option<serde_json::Value>,
    pub cutting_params: Option<serde_json::Value>,
    pub material_compatibility: Vec<String>,
    pub inventory: i32,
    pub tool_life: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// A cutting-data set (speeds + feedrates) for a tool, scoped to a material and
/// a required finish. All values are in the tool's units.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ToolCuttingData {
    pub id: Uuid,
    pub tool_id: Uuid,
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
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A document attached to a tool (datasheet, drawing, vendor page file, ...).
/// The bytes live in a content-addressed blob.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ToolDocument {
    pub id: Uuid,
    pub tool_id: Uuid,
    pub filename: String,
    pub blob_id: Uuid,
    pub size_bytes: i64,
    pub content_type: Option<String>,
    pub created_at: DateTime<Utc>,
}

// ── Manufacturing execution chain (Part → Setup → CAM → NC → Job → Inspection) ──

/// A manufacturing setup: the link between a part revision and its stock/workholding/WCS.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MfgSetup {
    pub id: Uuid,
    pub org_id: Uuid,
    pub item_id: Uuid,
    pub file_version_id: Option<Uuid>,
    pub name: String,
    pub stock: Option<serde_json::Value>,
    pub workholding: Option<String>,
    pub wcs: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// A CAM project (toolpaths + tool selection) built from a setup.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CamProject {
    pub id: Uuid,
    pub org_id: Uuid,
    pub mfg_setup_id: Uuid,
    pub name: String,
    pub source: Option<String>,
    pub tool_ids: Vec<Uuid>,
    pub toolpaths: Option<serde_json::Value>,
    pub feeds_speeds: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// An NC program (G-code). Pins the exact source CAD revision, setup, machine and
/// post-processor version used to produce it (data integrity).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct NcProgram {
    pub id: Uuid,
    pub org_id: Uuid,
    pub item_id: Uuid,
    pub file_version_id: Option<Uuid>,
    pub mfg_setup_id: Option<Uuid>,
    pub cam_project_id: Option<Uuid>,
    pub machine_id: Option<Uuid>,
    pub post_processor_id: Option<Uuid>,
    pub post_processor_version: Option<String>,
    pub gcode_sha256: Option<String>,
    pub simulation_result: Option<serde_json::Value>,
    pub released: bool,
    pub released_at: Option<DateTime<Utc>>,
    /// TRUE when the source design changed after this NC program was created.
    pub outdated: bool,
    pub created_at: DateTime<Utc>,
}

/// A production job: machining a released NC program on a machine.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ProductionJob {
    pub id: Uuid,
    pub org_id: Uuid,
    pub nc_program_id: Uuid,
    pub item_id: Option<Uuid>,
    pub machine_id: Option<Uuid>,
    pub status: String,
    pub operator: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// An inspection result against a production job.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct InspectionRecord {
    pub id: Uuid,
    pub org_id: Uuid,
    pub production_job_id: Option<Uuid>,
    pub item_id: Option<Uuid>,
    pub passed: Option<bool>,
    pub notes: Option<String>,
    pub measured: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

/// A 3D print job (STL/3MF on a printer).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct PrintJob {
    pub id: Uuid,
    pub org_id: Uuid,
    pub item_id: Uuid,
    pub file_version_id: Option<Uuid>,
    pub printer_id: Option<Uuid>,
    pub material: Option<String>,
    pub slicer_profile: Option<String>,
    pub orientation: Option<String>,
    pub supports: bool,
    pub layer_height_mm: Option<f64>,
    pub estimated_minutes: Option<i32>,
    pub machine_file_sha: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// Sheet-metal manufacturing spec for a part.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SheetMetalSpec {
    pub id: Uuid,
    pub org_id: Uuid,
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
    pub created_at: DateTime<Utc>,
}

// ── Laser nesting (Sparrow integration) ──────────────────────────────────

/// An immutable, revision-bound flat-pattern DXF: the exact CAD version,
/// generator and units a DXF was produced from.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct FlatPatternArtifact {
    pub id: Uuid,
    pub org_id: Uuid,
    pub item_id: Uuid,
    pub file_version_id: Uuid,
    pub sha256: String,
    pub size_bytes: i64,
    /// 'generated' | 'uploaded' | 'version'
    pub source_kind: String,
    pub generator: String,
    /// 'mm' | 'inch' | 'unknown'
    pub unit: String,
    pub notes: String,
    pub created_at: DateTime<Utc>,
}

/// One sheet-size definition within a nesting run. The worker works through
/// definitions in order, filling each up to its quantity.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SheetDef {
    pub width_mm: f64,
    pub length_mm: f64,
    /// How many physical sheets of this size to use; `None` = as many as needed.
    pub quantity: Option<i32>,
}

/// A laser nesting run (settings snapshot + status).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LaserNest {
    pub id: Uuid,
    pub org_id: Uuid,
    pub project_id: Option<Uuid>,
    pub name: String,
    /// 'queued' | 'running' | 'completed' | 'failed' | 'cancelled'
    pub status: String,
    pub machine_id: Option<Uuid>,
    pub material: Option<String>,
    pub thickness_mm: Option<f64>,
    pub stock_width_mm: f64,
    pub stock_height_mm: f64,
    pub edge_margin_mm: f64,
    pub part_gap_mm: f64,
    /// Horizontal (x-axis) spacing between parts, mm.
    pub spacing_x_mm: f64,
    /// Vertical (y-axis) spacing between parts, mm.
    pub spacing_y_mm: f64,
    /// Sheet grain direction: "along_length" | "along_width" | "any".
    pub grain_direction: String,
    /// Which corner the fill starts from.
    pub nesting_corner: String,
    /// Nesting origin offset from the stock lower-left corner, mm.
    pub origin_x_mm: f64,
    pub origin_y_mm: f64,
    /// Reference gap between sheets (mm), for the manifest/package.
    pub sheet_sheet_distance_mm: f64,
    /// Request a guillotine-cuttable layout.
    pub guillotine: bool,
    /// Draw part labels on the sheet DXF + preview.
    pub add_part_labels: bool,
    pub fill_direction: String,
    pub auto_sheet_origins: bool,
    pub fill_all_sheets: bool,
    pub optimized_pairs: bool,
    /// Sheet-size definitions in order; empty = single legacy stock size.
    pub sheet_defs: sqlx::types::Json<Vec<SheetDef>>,
    pub chord_tol_mm: f64,
    pub join_tol_mm: f64,
    /// Default source unit for parts whose DXF declares no `$INSUNITS`
    /// ("mm" | "in" | "inch").
    pub default_part_unit: String,
    pub allowed_rotations: sqlx::types::Json<Vec<f64>>,
    pub time_budget_sec: i32,
    pub attempt_limit_sec: i32,
    pub max_sheets: i32,
    pub seed: Option<i64>,
    pub solver_version: Option<String>,
    pub engine_build: Option<String>,
    pub job_id: Option<Uuid>,
    pub error: Option<String>,
    pub manifest_sha: Option<String>,
    pub stale: bool,
    pub stale_reason: Option<String>,
    pub parent_nest_id: Option<Uuid>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// One part in a nest (exact source snapshot + quantity).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LaserNestPart {
    pub id: Uuid,
    pub nest_id: Uuid,
    pub org_id: Uuid,
    pub item_id: Uuid,
    pub file_version_id: Uuid,
    pub artifact_id: Option<Uuid>,
    pub dxf_sha: String,
    pub dxf_size: i64,
    pub part_number: String,
    pub revision_label: Option<String>,
    pub quantity: i32,
    pub layer_cut: String,
    pub layers_mark: sqlx::types::Json<Vec<String>>,
    pub unit: String,
    /// This line uses a mirror of the original geometry.
    pub mirror: bool,
    /// The solver may flip copies of this line.
    pub allow_mirror: bool,
    /// Part grain preference: "along_length" | "along_width" | "any".
    pub part_grain: String,
    /// Stable line order (the part_index used by placements).
    pub ordinal: i32,
    /// Rotation increment in degrees (90 => 0/90/180/270, 45 => 8, 180 => 0/180).
    pub step_angle: f64,
    /// Nesting priority (lower number placed first).
    pub priority: i32,
    /// Part label drawn when the nest has add_part_labels on.
    pub label: String,
    /// Nest by the part's bounding box instead of its outline.
    pub use_bounding_box: bool,
    /// Request common-edge alignment.
    pub common_edge: bool,
    pub min_qty: i32,
    pub filler_qty: i32,
}

/// A produced sheet and its output blobs.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LaserNestSheet {
    pub id: Uuid,
    pub nest_id: Uuid,
    pub org_id: Uuid,
    pub sheet_index: i32,
    pub dxf_sha: Option<String>,
    pub dxf_size: Option<i64>,
    pub preview_sha: Option<String>,
    pub strip_width_mm: Option<f64>,
    pub utilization: Option<f64>,
    pub solver_input_sha: Option<String>,
    pub solver_output_sha: Option<String>,
    pub placed_count: i32,
}

/// One placed copy and its rigid transform (stock coordinates).
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LaserNestPlacement {
    pub id: Uuid,
    pub sheet_id: Uuid,
    pub nest_id: Uuid,
    pub org_id: Uuid,
    pub part_index: i32,
    pub copy_index: i32,
    pub item_id: Uuid,
    pub file_version_id: Uuid,
    pub reflected: bool,
    pub rotation_deg: f64,
    pub tx: f64,
    pub ty: f64,
}

/// A nest with its parts, sheets and placements (the detail endpoint).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaserNestDetail {
    pub nest: LaserNest,
    pub parts: Vec<LaserNestPart>,
    pub sheets: Vec<LaserNestSheet>,
    pub placements: Vec<LaserNestPlacement>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Job {
    pub id: Uuid,
    pub org_id: Uuid,
    pub kind: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Property {
    pub id: Uuid,
    pub item_id: Uuid,
    pub prop_key: String,
    pub value_type: String,
    pub value: serde_json::Value,
    pub computed: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Material {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
    pub density: Option<f64>,
}

/// A sheet-metal standard: one row of the organization's reusable library of
/// material x thickness -> bend properties (90-degree air bend). Per-item
/// `SheetMetalSpec`s reference these by material name and thickness.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct SheetMaterialStandard {
    pub id: Uuid,
    pub org_id: Uuid,
    pub material_group: String,
    pub material: String,
    pub thickness_mm: f64,
    pub thickness_gauge: Option<String>,
    pub density_g_cm3: Option<f64>,
    pub min_bend_radius_mm: Option<f64>,
    pub k_factor: Option<f64>,
    pub bend_deduction_mm: Option<f64>,
    pub bend_allowance_mm: Option<f64>,
    pub notes: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct PartNumberPrefix {
    pub id: Uuid,
    pub org_id: Uuid,
    pub category: String,
    pub prefix: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// ── Lifecycle state machine entities ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LifecycleDefinition {
    pub id: Uuid,
    pub org_id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LifecycleState {
    pub id: Uuid,
    pub definition_id: Uuid,
    pub name: String,
    pub is_released: bool,
    pub is_terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct LifecycleTransition {
    pub id: Uuid,
    pub definition_id: Uuid,
    pub from_state: Option<Uuid>,
    pub to_state: Uuid,
}

// ── Approval entities ────────────────────────────────────────────────────

/// An approval request routes one item to one or more assignees. `assignees`
/// is decoded from the `assignees` JSONB column by the repository layer, so
/// this struct is hand-mapped rather than derived from a raw row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub org_id: Uuid,
    pub item_id: Option<Uuid>,
    pub requested_by: Option<Uuid>,
    pub status: String,
    pub assignees: Vec<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ApprovalDecision {
    pub id: Uuid,
    pub request_id: Uuid,
    pub user_id: Option<Uuid>,
    pub decision: String,
    pub comment: String,
    pub decided_at: DateTime<Utc>,
}

/// Summary of an approval request with the denormalized fields the inbox UI
/// needs (part number, requester name, decoded assignees).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequestSummary {
    pub id: Uuid,
    pub item_id: Option<Uuid>,
    pub item_part_number: Option<String>,
    pub requested_by: Option<Uuid>,
    pub requested_by_name: Option<String>,
    pub assignees: Vec<Uuid>,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

// ── Event outbox ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct IntegrationOutbox {
    pub id: Uuid,
    pub org_id: Uuid,
    pub kind: String,
    pub payload: serde_json::Value,
    pub status: String,
    pub attempts: i32,
    pub sent_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

// ── Part numbering ───────────────────────────────────────────────────────

/// Generate a part number like "DT-000001" from a prefix and sequence.
pub fn generate_part_number(prefix: &str, seq: i64) -> String {
    format!("{prefix}-{seq:06}")
}

/// Resolve the part-number prefix for a category: the configured prefix if one
/// exists, otherwise the category itself (so an unconfigured category still
/// yields a stable prefix).
pub fn prefix_for_category(prefixes: &[PartNumberPrefix], category: &str) -> String {
    prefixes
        .iter()
        .find(|p| p.category.eq_ignore_ascii_case(category))
        .map(|p| p.prefix.clone())
        .unwrap_or_else(|| category.to_string())
}

// ── Versioning rules ─────────────────────────────────────────────────────

/// Validate that a check-in's base version is consistent with the current head.
///
/// - `None, None` → first version ever, OK.
/// - `Some(b), Some(h)` → must be equal, otherwise stale.
/// - `None, Some(_)`  → client claims no base but head exists → stale.
/// - `Some(_), None`  → client has a base but no head yet (shouldn't happen) → OK.
pub fn validate_checkin(base_seq: Option<i32>, head_seq: Option<i32>) -> Result<(), PdmError> {
    match (base_seq, head_seq) {
        (None, None) => Ok(()),
        (Some(b), Some(h)) => {
            if b == h {
                Ok(())
            } else {
                Err(PdmError::Conflict(format!(
                    "stale base version {b}, current head is {h}"
                )))
            }
        }
        (None, Some(h)) => Err(PdmError::Conflict(format!(
            "no base version supplied but head is {h}"
        ))),
        (Some(_), None) => Ok(()),
    }
}

/// Default lock lease duration.
pub fn default_lease() -> chrono::Duration {
    chrono::Duration::hours(24)
}

/// Check whether a lock has expired.
pub fn lock_expired(lock: &FileLock, now: &DateTime<Utc>) -> bool {
    &lock.lease_expires_at < now
}

// ── Lifecycle states ─────────────────────────────────────────────────────

pub const LIFECYCLE_STATES: &[&str] = &["wip", "in_review", "approved", "released", "obsolete"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_number_format() {
        assert_eq!(generate_part_number("BE", 1), "BE-000001");
        assert_eq!(generate_part_number("ASM", 42), "ASM-000042");
        assert_eq!(generate_part_number("MFG", 1000000), "MFG-1000000");
    }

    #[test]
    fn prefix_resolves_to_configured_or_falls_back_to_category() {
        fn mk(cat: &str, pre: &str) -> PartNumberPrefix {
            PartNumberPrefix {
                id: Uuid::new_v4(),
                org_id: Uuid::new_v4(),
                category: cat.into(),
                prefix: pre.into(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }
        }
        let prefixes = vec![mk("Drain Tray", "DT"), mk("Glycol Unit", "GL")];
        assert_eq!(prefix_for_category(&prefixes, "Drain Tray"), "DT");
        assert_eq!(prefix_for_category(&prefixes, "glycol unit"), "GL"); // case-insensitive
        assert_eq!(
            prefix_for_category(&prefixes, "Shaft Accessory"),
            "Shaft Accessory"
        ); // unconfigured → itself
    }

    #[test]
    fn sheet_metal_detection_reads_the_timeline() {
        let sheet = serde_json::json!({
            "body_count": 1,
            "timeline": [
                {"type": "Sketch"},
                {"type": "BaseFlangeFeature"},
                {"type": "EdgeFlangeFeature"}
            ]
        });
        assert!(measurements_have_sheet_metal_features(&sheet));
        let solid = serde_json::json!({
            "body_count": 1,
            "timeline": [
                {"type": "Sketch"},
                {"type": "ExtrudeFeature"},
                {"type": "FilletFeature"}
            ]
        });
        assert!(!measurements_have_sheet_metal_features(&solid));
        let imported = serde_json::json!({ "timeline": [{"type": "BaseFeature"}] });
        assert!(!measurements_have_sheet_metal_features(&imported));
        let skipped =
            serde_json::json!({ "skipped": true, "reason": "solvecraft-cli not configured" });
        assert!(!measurements_have_sheet_metal_features(&skipped));
    }

    #[test]
    fn preview_plan_classifies_by_extension() {
        for name in [
            "native.SC",
            "part.solvecraft",
            "Part.SOLIDCRAFT",
            "script.json",
            "gear.step",
            "gear.stp",
            "surface.igs",
            "surface.IGES",
            "part.FCStd",
            "model.3dm",
            "bolt.ipt",
            "assy.IAM",
            "thing.3mf",
            "mesh.stl",
            "mesh.OBJ",
            "scene.glb",
            "scene.gltf",
        ] {
            assert_eq!(preview_plan(name), PreviewPlan::Direct, "{name}");
        }
        for name in [
            "part.sldprt",
            "assembly.SLDASM",
            "drawing.slddrw",
            "catia.cgr",
            "nx.prt",
            "parasolid.x_t",
            "f3dpart.f3d",
            "acis.sat",
            "inventor.idw",
            "view.ipj",
            "tess.jt",
        ] {
            assert_eq!(preview_plan(name), PreviewPlan::Convert, "{name}");
        }
        for name in [
            "sheet.pdf",
            "flat.dxf",
            "photo.png",
            "notes.txt",
            "noext",
            "",
            "a/b",
        ] {
            assert_eq!(preview_plan(name), PreviewPlan::Skip, "{name}");
        }
    }

    #[test]
    fn solidcraft_conversion_admission_and_names() {
        assert!(can_convert_to_solidcraft("plate.STEP"));
        assert!(can_convert_to_solidcraft("part.ipt"));
        assert!(can_convert_to_solidcraft("mesh.3mf"));
        // Dutchman's native formats: Fusion, SolidWorks, CATIA, Creo View, ACIS.
        for name in [
            "printer.f3d",
            "archive.F3Z",
            "bracket.sldprt",
            "machine.SLDASM",
            "housings.CATPart",
            "view.pvz",
            "acis.sat",
            "acis.sab",
            "acis.smt",
            "acis.smb",
        ] {
            assert!(can_convert_to_solidcraft(name), "{name}");
        }
        for name in [
            "already.sc",
            "script.json",
            "drawing.pdf",
            "image.png",
            "bad.step.exe",
        ] {
            assert!(!can_convert_to_solidcraft(name), "{name}");
        }
        assert_eq!(solidcraft_artifact_filename("plate.step", 3), "plate-v3.sc");
        assert_eq!(
            solidcraft_artifact_filename("C:\\parts\\plate.step", 1),
            "plate-v1.sc"
        );
        assert_eq!(version_filename("plate.step", "sc"), "plate.sc");
        assert_eq!(version_filename("plate.sc", "step"), "plate.step");
        let name = solidcraft_artifact_filename("../evil\r\n\".step", 2);
        assert!(!name.contains(['\r', '\n', '"', '/', '\\']));
    }

    #[test]
    fn converter_selection_prefers_dutchman() {
        for name in [
            "printer.f3d",
            "part.ipt",
            "assy.IAM",
            "block.FCStd",
            "model.3DM",
            "plate.STEP",
            "plate.stp",
            "bracket.sldprt",
            "casing.CATPart",
            "scan.pvz",
            "solid.sat",
            "intake.onshape-bundle",
        ] {
            assert_eq!(sc_converter(name), Some(ScConverter::Dutchman), "{name}");
        }
        // Geometry-only formats stay on the SolidCraft CLI.
        for name in [
            "mesh.stl",
            "mesh.OBJ",
            "thing.3mf",
            "scene.glb",
            "scene.gltf",
            "wire.igs",
            "wire.IGES",
        ] {
            assert_eq!(
                sc_converter(name),
                Some(ScConverter::SolidcraftCli),
                "{name}"
            );
        }
        for name in ["already.sc", "script.json", "drawing.pdf", "noext"] {
            assert_eq!(sc_converter(name), None, "{name}");
        }
    }

    #[test]
    fn dutchman_settings_defaults_and_validation() {
        let s = DutchmanSettings::default();
        assert!(s.validate().is_ok());
        assert_eq!(s.channel, "latest");
        assert_eq!(s.onshape_call_budget(), 200);

        let s = DutchmanSettings {
            channel: "onshape-preview-2".into(),
            preset: "full".into(),
            part_positions: "last-shown".into(),
            verify: false,
            timeout_secs: 900,
            onshape_max_calls: 500,
        };
        assert!(s.validate().is_ok());
        assert_eq!(s.onshape_call_budget(), 500);

        let long = "x".repeat(60);
        for channel in ["", "Latest", "v 0.2.9", "tags/nightly", long.as_str()] {
            let s = DutchmanSettings {
                channel: channel.into(),
                ..Default::default()
            };
            assert!(s.validate().is_err(), "{channel}");
        }
        let s = DutchmanSettings {
            preset: "turbo".into(),
            ..Default::default()
        };
        assert!(s.validate().is_err());
        let s = DutchmanSettings {
            timeout_secs: 30,
            ..Default::default()
        };
        assert!(s.validate().is_err());
        let s = DutchmanSettings {
            onshape_max_calls: 3,
            ..Default::default()
        };
        assert!(s.validate().is_err());
    }

    #[test]
    fn cad_convert_extensions() {
        for name in [
            "part.ipt",
            "assy.IAM",
            "block.FCStd",
            "block.3DM",
            "part.step",
            "part.stp",
            "part.igs",
            "part.IGES",
        ] {
            assert!(is_cad_convert_path(name), "{name}");
        }
        for name in [
            "part.stl",
            "part.3mf",
            "part.sldprt",
            "part.f3d",
            "part.dxf",
            "sheet.pdf",
            "nodot",
        ] {
            assert!(!is_cad_convert_path(name), "{name}");
        }
    }

    #[test]
    fn checkin_first_version() {
        assert!(validate_checkin(None, None).is_ok());
    }

    #[test]
    fn checkin_matching_base() {
        assert!(validate_checkin(Some(1), Some(1)).is_ok());
        assert!(validate_checkin(Some(5), Some(5)).is_ok());
    }

    #[test]
    fn checkin_stale_base() {
        assert!(matches!(
            validate_checkin(Some(1), Some(2)),
            Err(PdmError::Conflict(ref msg)) if msg.contains("stale")
        ));
    }

    #[test]
    fn checkin_no_base_with_head() {
        assert!(matches!(
            validate_checkin(None, Some(3)),
            Err(PdmError::Conflict(_))
        ));
    }

    #[test]
    fn lock_expiry() {
        let now = Utc::now();
        let active = FileLock {
            id: Uuid::new_v4(),
            item_id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            workstation: String::new(),
            base_version_id: None,
            acquired_at: now,
            lease_expires_at: now + chrono::Duration::hours(1),
        };
        assert!(!lock_expired(&active, &now));

        let expired = FileLock {
            lease_expires_at: now - chrono::Duration::seconds(1),
            ..active.clone()
        };
        assert!(lock_expired(&expired, &now));
    }

    #[test]
    fn error_status_codes() {
        assert_eq!(PdmError::NotFound("x".into()).status_code(), 404);
        assert_eq!(PdmError::Conflict("x".into()).status_code(), 409);
        assert_eq!(PdmError::AuthFailed.status_code(), 401);
        assert_eq!(PdmError::SessionExpired.status_code(), 401);
        assert_eq!(PdmError::SessionExpired.code(), "session_expired");
        assert_eq!(PdmError::BadCredentials.status_code(), 401);
        assert_eq!(PdmError::Forbidden("x".into()).status_code(), 403);
        assert_eq!(PdmError::BadRequest("x".into()).status_code(), 400);
        assert_eq!(
            PdmError::IntegrityMismatch {
                expected: "a".into(),
                actual: "b".into()
            }
            .status_code(),
            422
        );
        assert_eq!(PdmError::Validation("x".into()).status_code(), 422);
        assert_eq!(PdmError::Db(sqlx::Error::RowNotFound).status_code(), 500);
        assert_eq!(PdmError::Storage("x".into()).status_code(), 502);
        assert_eq!(PdmError::Internal("x".into()).status_code(), 500);
    }
}
