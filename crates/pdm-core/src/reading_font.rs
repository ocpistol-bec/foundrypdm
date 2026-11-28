//! Organization default reading font.
//!
//! The web portal bundles open-license faces under `static/fonts/`. An admin can make one of
//! them the organization's default; a member's own choice in the portal still wins. The setting
//! names a face for reading text (`ui`) and one for identifiers and codes (`mono`); `system`
//! keeps the device's own font. Only faces the portal bundles are accepted.

use serde_json::{Value, json};

use crate::PdmError;

/// Reading-text faces by id. Keep in step with `static/style.css` (`html[data-font-ui="…"]`)
/// and `fontSettings()` in `static/app.js`.
pub const UI_FACES: &[&str] = &["system", "atkinson"];
/// Faces for identifiers and codes. Keep in step with `html[data-font-mono="…"]`.
pub const MONO_FACES: &[&str] = &["system", "atkinson"];

/// The organization's default faces: `system` for both until an admin picks one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadingFont {
    pub ui: String,
    pub mono: String,
}

impl Default for ReadingFont {
    fn default() -> Self {
        Self {
            ui: "system".into(),
            mono: "system".into(),
        }
    }
}

impl ReadingFont {
    /// Parse a request body: an object with exactly `ui` and `mono`, each a bundled face id.
    pub fn parse(value: &Value) -> Result<Self, PdmError> {
        let obj = value
            .as_object()
            .ok_or_else(|| bad("expected an object with ui and mono"))?;
        if let Some(key) = obj.keys().find(|k| *k != "ui" && *k != "mono") {
            return Err(bad(&format!("unknown field {key}; expected ui and mono")));
        }
        let face = |key: &str, allowed: &[&str]| {
            obj.get(key)
                .and_then(Value::as_str)
                .filter(|id| allowed.contains(id))
                .map(str::to_string)
                .ok_or_else(|| bad(&format!("{key} must be one of {}", allowed.join(", "))))
        };
        Ok(Self {
            ui: face("ui", UI_FACES)?,
            mono: face("mono", MONO_FACES)?,
        })
    }

    /// A stored value as read back. Each face that is missing or no longer bundled reads as
    /// `system`, so one stale entry does not discard the other.
    pub fn from_stored(value: Option<&Value>) -> Self {
        let face = |key: &str, allowed: &[&str]| {
            value
                .and_then(|v| v.get(key))
                .and_then(Value::as_str)
                .filter(|id| allowed.contains(id))
                .unwrap_or("system")
                .to_string()
        };
        Self {
            ui: face("ui", UI_FACES),
            mono: face("mono", MONO_FACES),
        }
    }

    /// The JSON the API answers with and stores: `{ "ui": …, "mono": … }`.
    pub fn to_json(&self) -> Value {
        json!({ "ui": self.ui, "mono": self.mono })
    }
}

fn bad(msg: &str) -> PdmError {
    PdmError::BadRequest(format!("reading font: {msg}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn font(ui: &str, mono: &str) -> ReadingFont {
        ReadingFont {
            ui: ui.into(),
            mono: mono.into(),
        }
    }

    #[test]
    fn accepts_every_bundled_face_and_round_trips() {
        for ui in UI_FACES {
            for mono in MONO_FACES {
                let body = json!({ "ui": ui, "mono": mono });
                let parsed = ReadingFont::parse(&body).ok();
                assert_eq!(parsed, Some(font(ui, mono)));
                assert_eq!(parsed.map(|f| f.to_json()), Some(body.clone()));
                assert_eq!(ReadingFont::from_stored(Some(&body)), font(ui, mono));
            }
        }
        assert_eq!(UI_FACES.first(), Some(&"system"));
        assert_eq!(MONO_FACES.first(), Some(&"system"));
    }

    #[test]
    fn rejects_faces_that_are_not_bundled_and_malformed_bodies() {
        for body in [
            json!({ "ui": "comic", "mono": "system" }),
            json!({ "ui": "system", "mono": "Atkinson" }),
            json!({ "ui": "system" }),
            json!({ "mono": "system" }),
            json!({ "ui": 1, "mono": "system" }),
            json!({ "ui": "system", "mono": "system", "size": 125 }),
            json!(["system", "system"]),
            json!("atkinson"),
            Value::Null,
        ] {
            let err = ReadingFont::parse(&body).err();
            assert!(
                matches!(err, Some(PdmError::BadRequest(_))),
                "{body} gave {err:?}"
            );
        }
        let msg = ReadingFont::parse(&json!({ "ui": "comic", "mono": "system" }))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(msg.contains("ui must be one of system, atkinson"), "{msg}");
    }

    #[test]
    fn stored_values_fall_back_to_system_face_by_face() {
        assert_eq!(ReadingFont::from_stored(None), ReadingFont::default());
        assert_eq!(
            ReadingFont::from_stored(Some(&json!("atkinson"))),
            ReadingFont::default()
        );
        assert_eq!(
            ReadingFont::from_stored(Some(&json!({ "ui": "atkinson", "mono": "retired-face" }))),
            font("atkinson", "system")
        );
        assert_eq!(ReadingFont::default(), font("system", "system"));
    }
}
