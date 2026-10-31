use notryx_protocol::{
    ThemePackage, ThemePreferences, ThemeSelection, ThemeStatus, theme::MAX_THEME_BYTES,
};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};
use tauri::Webview;

pub(crate) struct ThemeStore {
    directory: PathBuf,
    safe_mode: bool,
    state: Mutex<ThemeState>,
}

struct ThemeState {
    preferences: ThemePreferences,
    warning: Option<String>,
}

impl ThemeStore {
    pub(crate) fn new(directory: PathBuf, safe_mode: bool) -> Self {
        let (preferences, warning) = if safe_mode {
            (ThemePreferences::default(), Some("theme_safe_mode".into()))
        } else {
            match read_preferences(&directory.join("theme.json")) {
                Ok(preferences) => (preferences, None),
                Err(warning) => (ThemePreferences::default(), Some(warning.into())),
            }
        };
        Self {
            directory,
            safe_mode,
            state: Mutex::new(ThemeState {
                preferences,
                warning,
            }),
        }
    }
    fn status(&self) -> Result<ThemeStatus, String> {
        let state = self.state.lock().map_err(|_| "theme_unavailable")?;
        Ok(self.snapshot(&state))
    }
    fn snapshot(&self, state: &ThemeState) -> ThemeStatus {
        ThemeStatus {
            selection: state.preferences.selection,
            custom: state.preferences.custom.clone(),
            safe_mode: self.safe_mode,
            warning: state.warning.clone(),
        }
    }
    fn commit(
        &self,
        state: &mut ThemeState,
        preferences: ThemePreferences,
    ) -> Result<ThemeStatus, String> {
        write_preferences(&self.directory, &preferences).map_err(|_| "theme_save_failed")?;
        state.preferences = preferences;
        state.warning = self.safe_mode.then(|| "theme_safe_mode".into());
        Ok(self.snapshot(state))
    }
    fn import(&self, json: &str) -> Result<ThemeStatus, String> {
        if self.safe_mode {
            return Err("theme_safe_mode".into());
        }
        let custom = ThemePackage::parse_json(json).map_err(str::to_owned)?;
        let mut state = self.state.lock().map_err(|_| "theme_unavailable")?;
        self.commit(
            &mut state,
            ThemePreferences {
                selection: ThemeSelection::Custom,
                custom: Some(custom),
            },
        )
    }
    fn select(&self, selection: ThemeSelection) -> Result<ThemeStatus, String> {
        if self.safe_mode {
            return Err("theme_safe_mode".into());
        }
        let mut state = self.state.lock().map_err(|_| "theme_unavailable")?;
        if selection == ThemeSelection::Custom && state.preferences.custom.is_none() {
            return Err("theme_custom_missing".into());
        }
        let preferences = ThemePreferences {
            selection,
            custom: state.preferences.custom.clone(),
        };
        self.commit(&mut state, preferences)
    }
    fn reset(&self) -> Result<ThemeStatus, String> {
        let mut state = self.state.lock().map_err(|_| "theme_unavailable")?;
        self.commit(&mut state, ThemePreferences::default())
    }
}

fn read_preferences(path: &Path) -> Result<ThemePreferences, &'static str> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ThemePreferences::default());
        }
        Err(_) => return Err("theme_preferences_unavailable"),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_THEME_BYTES as u64 {
        return Err("theme_preferences_invalid");
    }
    let file = std::fs::File::open(path).map_err(|_| "theme_preferences_unavailable")?;
    let mut bytes = Vec::new();
    file.take((MAX_THEME_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "theme_preferences_unavailable")?;
    if bytes.len() > MAX_THEME_BYTES {
        return Err("theme_preferences_invalid");
    }
    let preferences: ThemePreferences =
        serde_json::from_slice(&bytes).map_err(|_| "theme_preferences_invalid")?;
    if let Some(custom) = &preferences.custom {
        custom.validate().map_err(|_| "theme_preferences_invalid")?;
    }
    if preferences.selection == ThemeSelection::Custom && preferences.custom.is_none() {
        return Err("theme_preferences_invalid");
    }
    Ok(preferences)
}

#[cfg(target_os = "linux")]
fn write_preferences(directory: &Path, preferences: &ThemePreferences) -> std::io::Result<()> {
    use std::io::Write;
    let path = directory.join("theme.json");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(std::io::Error::other("theme path is not a regular file"));
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let bytes = serde_json::to_vec(preferences)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(&path).map_err(|error| error.error)?;
    std::fs::File::open(directory)?.sync_all()
}

#[cfg(not(target_os = "linux"))]
fn write_preferences(_directory: &Path, _preferences: &ThemePreferences) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "theme persistence is not supported on this platform",
    ))
}

#[tauri::command]
pub(crate) async fn theme_status(
    webview: Webview,
    store: tauri::State<'_, ThemeStore>,
) -> Result<ThemeStatus, String> {
    crate::check_caller(&webview)?;
    store.status()
}

#[tauri::command]
pub(crate) async fn theme_select(
    webview: Webview,
    store: tauri::State<'_, ThemeStore>,
    selection: ThemeSelection,
) -> Result<ThemeStatus, String> {
    crate::check_caller(&webview)?;
    store.select(selection)
}

#[tauri::command]
pub(crate) async fn theme_import(
    webview: Webview,
    store: tauri::State<'_, ThemeStore>,
    json: String,
) -> Result<ThemeStatus, String> {
    crate::check_caller(&webview)?;
    store.import(&json)
}

#[tauri::command]
pub(crate) async fn theme_reset(
    webview: Webview,
    store: tauri::State<'_, ThemeStore>,
) -> Result<ThemeStatus, String> {
    crate::check_caller(&webview)?;
    store.reset()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic() -> String {
        serde_json::json!({"id":"theme.synthetic","schemaVersion":1,"name":"합성 테마","base":"dark","tokens":{
            "surface.background":"#16191f","text.primary":"#e7eaf0","border.default":"#343b48","status.conflict":"#e6a44b"
        }}).to_string()
    }

    #[test]
    fn saves_custom_and_builtin_selection_across_restarts() {
        let directory = tempfile::tempdir().unwrap();
        let store = ThemeStore::new(directory.path().into(), false);
        assert_eq!(
            store.import(&synthetic()).unwrap().selection,
            ThemeSelection::Custom
        );
        let restarted = ThemeStore::new(directory.path().into(), false);
        assert_eq!(
            restarted.status().unwrap().custom.unwrap().name,
            "합성 테마"
        );
        restarted.select(ThemeSelection::Dark).unwrap();
        let again = ThemeStore::new(directory.path().into(), false);
        let status = again.status().unwrap();
        assert_eq!(status.selection, ThemeSelection::Dark);
        assert!(status.custom.is_some());
        assert_eq!(
            again.select(ThemeSelection::Custom).unwrap().selection,
            ThemeSelection::Custom
        );
    }

    #[test]
    fn rejects_invalid_import_without_changing_the_selected_theme_or_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = ThemeStore::new(directory.path().into(), false);
        store.import(&synthetic()).unwrap();
        let before = std::fs::read(directory.path().join("theme.json")).unwrap();
        assert!(store.import("{\"script\":\"alert(1)\"}").is_err());
        assert_eq!(
            std::fs::read(directory.path().join("theme.json")).unwrap(),
            before
        );
        assert_eq!(store.status().unwrap().selection, ThemeSelection::Custom);
    }

    #[test]
    fn corrupt_or_oversized_preferences_fall_back_without_rewriting_the_file() {
        let directory = tempfile::tempdir().unwrap();
        for bytes in [
            b"{broken".to_vec(),
            vec![b' '; notryx_protocol::theme::MAX_THEME_BYTES + 1],
        ] {
            let path = directory.path().join("theme.json");
            std::fs::write(&path, &bytes).unwrap();
            let status = ThemeStore::new(directory.path().into(), false)
                .status()
                .unwrap();
            assert_eq!(status.selection, ThemeSelection::Light);
            assert!(status.warning.is_some());
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn safe_mode_preserves_settings_until_explicit_reset() {
        let directory = tempfile::tempdir().unwrap();
        ThemeStore::new(directory.path().into(), false)
            .import(&synthetic())
            .unwrap();
        let path = directory.path().join("theme.json");
        let before = std::fs::read(&path).unwrap();
        let safe = ThemeStore::new(directory.path().into(), true);
        assert_eq!(safe.status().unwrap().selection, ThemeSelection::Light);
        assert!(safe.status().unwrap().safe_mode);
        assert!(safe.select(ThemeSelection::Dark).is_err());
        assert!(safe.import(&synthetic()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(safe.reset().unwrap().safe_mode);
        let normal = ThemeStore::new(directory.path().into(), false)
            .status()
            .unwrap();
        assert_eq!(normal.selection, ThemeSelection::Light);
        assert!(normal.custom.is_none());
    }

    #[test]
    fn missing_custom_selection_does_not_create_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = ThemeStore::new(directory.path().into(), false);
        assert!(store.select(ThemeSelection::Custom).is_err());
        assert!(!directory.path().join("theme.json").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refuses_symlink_settings_without_touching_the_target() {
        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("untouched.json");
        std::fs::write(&outside, b"synthetic outside content").unwrap();
        std::os::unix::fs::symlink(&outside, directory.path().join("theme.json")).unwrap();
        let store = ThemeStore::new(directory.path().into(), false);
        assert!(store.status().unwrap().warning.is_some());
        assert!(store.reset().is_err());
        assert_eq!(
            std::fs::read(&outside).unwrap(),
            b"synthetic outside content"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_failure_preserves_the_current_selection_and_file() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let store = ThemeStore::new(directory.path().into(), false);
        store.import(&synthetic()).unwrap();
        let path = directory.path().join("theme.json");
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = store.select(ThemeSelection::Dark);
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result.unwrap_err(), "theme_save_failed");
        assert_eq!(store.status().unwrap().selection, ThemeSelection::Custom);
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}
