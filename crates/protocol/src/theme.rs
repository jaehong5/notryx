use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const MAX_THEME_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum ThemeBase {
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum ThemeSelection {
    #[default]
    Light,
    Dark,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(rename_all = "kebab-case")]
pub enum ThemeFont {
    SystemMonospace,
    SystemUi,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ThemeTokens {
    #[serde(rename = "surface.background")]
    #[ts(rename = "surface.background")]
    pub surface_background: String,
    #[serde(rename = "text.primary")]
    #[ts(rename = "text.primary")]
    pub text_primary: String,
    #[serde(rename = "border.default")]
    #[ts(rename = "border.default")]
    pub border_default: String,
    #[serde(rename = "status.conflict")]
    #[ts(rename = "status.conflict")]
    pub status_conflict: String,
    #[serde(
        rename = "surface.raised",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "surface.raised", optional)]
    pub surface_raised: Option<String>,
    #[serde(
        rename = "text.secondary",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "text.secondary", optional)]
    pub text_secondary: Option<String>,
    #[serde(
        rename = "accent.background",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "accent.background", optional)]
    pub accent_background: Option<String>,
    #[serde(
        rename = "accent.text",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "accent.text", optional)]
    pub accent_text: Option<String>,
    #[serde(
        rename = "status.error",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "status.error", optional)]
    pub status_error: Option<String>,
    #[serde(
        rename = "status.success",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "status.success", optional)]
    pub status_success: Option<String>,
    #[serde(
        rename = "editor.fontFamily",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "editor.fontFamily", optional)]
    pub editor_font_family: Option<ThemeFont>,
    #[serde(
        rename = "editor.fontSize",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "editor.fontSize", optional)]
    pub editor_font_size: Option<u8>,
    #[serde(
        rename = "spacing.scale",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    #[ts(rename = "spacing.scale", optional)]
    pub spacing_scale: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ThemePackage {
    pub id: String,
    #[serde(rename = "schemaVersion")]
    #[ts(rename = "schemaVersion", type = "1")]
    #[schemars(range(min = 1, max = 1))]
    pub schema_version: u8,
    pub name: String,
    pub base: ThemeBase,
    pub tokens: ThemeTokens,
}

impl ThemePackage {
    pub fn parse_json(json: &str) -> Result<Self, &'static str> {
        if json.len() > MAX_THEME_BYTES {
            return Err("theme_too_large");
        }
        let value: Self = serde_json::from_str(json).map_err(|_| "theme_invalid")?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1
            || self.id.is_empty()
            || self.id.len() > 64
            || !self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            || self.name.trim().is_empty()
            || self.name.chars().count() > 80
            || self.name.chars().any(char::is_control)
        {
            return Err("theme_invalid");
        }
        let tokens = &self.tokens;
        let colors = [
            Some(tokens.surface_background.as_str()),
            Some(tokens.text_primary.as_str()),
            Some(tokens.border_default.as_str()),
            Some(tokens.status_conflict.as_str()),
            tokens.surface_raised.as_deref(),
            tokens.text_secondary.as_deref(),
            tokens.accent_background.as_deref(),
            tokens.accent_text.as_deref(),
            tokens.status_error.as_deref(),
            tokens.status_success.as_deref(),
        ];
        if colors.into_iter().flatten().any(|color| {
            color.len() != 7
                || !color.starts_with('#')
                || !color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
        }) || tokens
            .editor_font_size
            .is_some_and(|size| !(12..=24).contains(&size))
            || tokens
                .spacing_scale
                .is_some_and(|scale| !scale.is_finite() || !(0.8..=1.5).contains(&scale))
        {
            return Err("theme_invalid");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ThemePreferences {
    pub selection: ThemeSelection,
    pub custom: Option<ThemePackage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ThemeStatus {
    pub selection: ThemeSelection,
    pub custom: Option<ThemePackage>,
    pub safe_mode: bool,
    pub warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package() -> serde_json::Value {
        json!({"id":"theme.synthetic", "schemaVersion":1,"name":"합성 테마","base":"dark","tokens":{
            "surface.background":"#16191f","text.primary":"#e7eaf0","border.default":"#343b48",
            "status.conflict":"#e6a44b","editor.fontFamily":"system-monospace","editor.fontSize":15
        }})
    }

    #[test]
    fn accepts_the_design_example_and_boundary_values() {
        let mut value = package();
        value["tokens"]["editor.fontSize"] = json!(12);
        value["tokens"]["spacing.scale"] = json!(0.8);
        assert!(ThemePackage::parse_json(&value.to_string()).is_ok());
        value["tokens"]["editor.fontSize"] = json!(24);
        value["tokens"]["spacing.scale"] = json!(1.5);
        assert!(ThemePackage::parse_json(&value.to_string()).is_ok());
    }

    #[test]
    fn rejects_code_css_urls_unknown_fields_and_invalid_tokens() {
        for (key, value) in [
            ("css", json!("body {display:none}")),
            ("script", json!("alert(1)")),
            ("@import", json!("https://example.invalid/theme")),
            (
                "surface.background",
                json!("url(https://example.invalid/image)"),
            ),
            ("text.primary", json!("red;display:none")),
            ("status.conflict", json!("#123")),
            ("editor.fontFamily", json!("https://example.invalid/font")),
            ("editor.fontSize", json!(25)),
            ("editor.fontSize", json!(11)),
            ("spacing.scale", json!(0.5)),
            ("spacing.scale", json!(2)),
        ] {
            let mut input = package();
            input["tokens"][key] = value;
            assert!(
                ThemePackage::parse_json(&input.to_string()).is_err(),
                "{key}"
            );
        }
        let mut input = package();
        input["script"] = json!("alert(1)");
        assert!(ThemePackage::parse_json(&input.to_string()).is_err());
    }

    #[test]
    fn rejects_invalid_identity_version_and_missing_required_state_color() {
        for (key, value) in [
            ("schemaVersion", json!(2)),
            ("id", json!("../theme")),
            ("name", json!(" \n ")),
        ] {
            let mut input = package();
            input[key] = value;
            assert!(
                ThemePackage::parse_json(&input.to_string()).is_err(),
                "{key}"
            );
        }
        let mut input = package();
        input["tokens"]
            .as_object_mut()
            .unwrap()
            .remove("status.conflict");
        assert!(ThemePackage::parse_json(&input.to_string()).is_err());
    }

    #[test]
    fn enforces_utf8_byte_limit_before_deserializing() {
        let value = package().to_string();
        let exact = format!("{value}{}", " ".repeat(MAX_THEME_BYTES - value.len()));
        assert!(ThemePackage::parse_json(&exact).is_ok());
        assert_eq!(
            ThemePackage::parse_json(&(exact + " ")),
            Err("theme_too_large")
        );
    }
}
