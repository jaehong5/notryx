use std::{collections::HashMap, sync::Mutex};
use tauri::{Manager, Webview};
use tokio::sync::oneshot;

const MAX_SOURCE_BYTES: usize = 64 * 1024;
const MAX_SVG_BYTES: usize = 2 * 1024 * 1024;
const MAX_RENDERS: usize = 2;
const RENDERER_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'none'; connect-src ipc: http://ipc.localhost; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";

type RenderResult = Result<String, String>;

#[derive(Default)]
pub(crate) struct RenderJobs(Mutex<HashMap<String, Option<oneshot::Sender<RenderResult>>>>);

impl RenderJobs {
    fn insert(&self, label: &str) -> Result<oneshot::Receiver<RenderResult>, String> {
        let mut jobs = self.0.lock().map_err(|_| "mermaid_unavailable")?;
        if jobs.len() >= MAX_RENDERS || jobs.contains_key(label) {
            return Err("mermaid_busy".into());
        }
        let (sender, receiver) = oneshot::channel();
        jobs.insert(label.to_owned(), Some(sender));
        Ok(receiver)
    }

    fn complete(&self, label: &str, result: RenderResult) -> Result<(), String> {
        let sender = self
            .0
            .lock()
            .map_err(|_| "mermaid_unavailable")?
            .get_mut(label)
            .and_then(Option::take)
            .ok_or("invalid_caller")?;
        sender.send(result).map_err(|_| "mermaid_cancelled".into())
    }

    fn remove(&self, label: &str) {
        if let Ok(mut jobs) = self.0.lock() {
            jobs.remove(label);
        }
    }
}

fn validate_input(source: &str, theme: &str) -> Result<(), String> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err("mermaid_source_too_large".into());
    }
    if !matches!(theme, "light" | "dark") {
        return Err("mermaid_invalid_theme".into());
    }
    Ok(())
}

fn renderer_location(url: &tauri::Url) -> bool {
    let local = (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (url.scheme() == "http" && url.host_str() == Some("tauri.localhost"))
        || (cfg!(debug_assertions)
            && url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(1420));
    local
        && url.path() == "/mermaid.html"
        && url.query().is_none()
        && url.fragment().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn checked_result(svg: Option<String>, error: Option<String>) -> RenderResult {
    match (svg, error) {
        (Some(svg), None) if !svg.is_empty() && svg.len() <= MAX_SVG_BYTES => Ok(svg),
        (None, Some(error)) if !error.is_empty() && error.len() <= 256 => Err(error),
        _ => Err("mermaid_invalid_result".into()),
    }
}

#[cfg(not(target_os = "android"))]
struct RenderLease {
    app: tauri::AppHandle,
    label: String,
}

#[cfg(not(target_os = "android"))]
impl Drop for RenderLease {
    fn drop(&mut self) {
        if let Some(window) = self.app.get_webview_window(&self.label) {
            let _ = window.destroy();
        }
        self.app.state::<RenderJobs>().remove(&self.label);
    }
}

pub(crate) fn cancel_all(app: &tauri::AppHandle) {
    if let Some(jobs) = app.try_state::<RenderJobs>()
        && let Ok(mut jobs) = jobs.0.lock()
    {
        for (_, sender) in jobs.drain() {
            if let Some(sender) = sender {
                let _ = sender.send(Err("mermaid_cancelled".into()));
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    for (label, window) in app.webview_windows() {
        if label.starts_with("mermaid-") {
            let _ = window.destroy();
        }
    }
}

#[cfg(not(target_os = "android"))]
fn build_renderer(
    app: &tauri::AppHandle,
    label: &str,
    input: Option<serde_json::Value>,
) -> tauri::Result<tauri::WebviewWindow> {
    use tauri::{WebviewUrl, WebviewWindowBuilder, http::HeaderValue};

    let mut builder = WebviewWindowBuilder::new(app, label, WebviewUrl::App("mermaid.html".into()))
        .title("Notryx diagram renderer")
        .visible(false)
        .focused(false)
        .skip_taskbar(true)
        .inner_size(1024.0, 768.0)
        .disable_drag_drop_handler()
        .on_navigation(renderer_location)
        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
        .on_web_resource_request(|request, response| {
            if request.uri().path() == "/mermaid.html" {
                response.headers_mut().insert(
                    "Content-Security-Policy",
                    HeaderValue::from_static(RENDERER_CSP),
                );
            }
        });
    if let Some(input) = input {
        builder = builder.initialization_script(format!("window.__NOTRYX_MERMAID__ = {input};"));
    }
    builder.build()
}

#[cfg(not(target_os = "android"))]
pub(crate) fn open_probe(app: &tauri::AppHandle) -> Result<(), String> {
    if app.get_webview_window("mermaid-probe").is_none() {
        build_renderer(app, "mermaid-probe", None).map_err(|_| "isolation_window_failed")?;
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn mermaid_render(
    webview: Webview,
    app: tauri::AppHandle,
    source: String,
    theme: String,
) -> RenderResult {
    crate::check_caller(&webview)?;
    validate_input(&source, &theme)?;
    #[cfg(target_os = "android")]
    {
        let _ = app;
        Err("mermaid_unavailable".into())
    }
    #[cfg(not(target_os = "android"))]
    {
        let label = format!("mermaid-{}", uuid::Uuid::new_v4());
        let receiver = app.state::<RenderJobs>().insert(&label)?;
        let _lease = RenderLease {
            app: app.clone(),
            label: label.clone(),
        };
        let input = serde_json::json!({ "source": source, "theme": theme });
        build_renderer(&app, &label, Some(input)).map_err(|_| "mermaid_unavailable")?;
        // Disposing the surface bounds its lifetime; this is not a separate CPU budget.
        tokio::time::timeout(std::time::Duration::from_secs(15), receiver)
            .await
            .map_err(|_| "mermaid_render_timeout")?
            .map_err(|_| "mermaid_render_failed")?
    }
}

#[tauri::command]
pub(crate) fn mermaid_complete(
    webview: Webview,
    jobs: tauri::State<'_, RenderJobs>,
    svg: Option<String>,
    error: Option<String>,
) -> Result<(), String> {
    let url = webview.url().map_err(|_| "invalid_caller")?;
    if !webview.label().starts_with("mermaid-") || !renderer_location(&url) {
        return Err("invalid_caller".into());
    }
    jobs.complete(webview.label(), checked_result(svg, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_source_byte_boundary_and_known_themes() {
        assert!(validate_input(&"x".repeat(MAX_SOURCE_BYTES), "light").is_ok());
        assert!(validate_input("graph TD; A-->B", "dark").is_ok());
        assert_eq!(
            validate_input(&"가".repeat(MAX_SOURCE_BYTES / 3 + 1), "light"),
            Err("mermaid_source_too_large".into())
        );
        assert_eq!(
            validate_input("graph TD", "external"),
            Err("mermaid_invalid_theme".into())
        );
    }

    #[test]
    fn permits_only_the_bundled_renderer_document() {
        assert!(renderer_location(
            &"tauri://localhost/mermaid.html".parse().unwrap()
        ));
        assert!(renderer_location(
            &"http://tauri.localhost/mermaid.html".parse().unwrap()
        ));
        for url in [
            "tauri://localhost/index.html",
            "tauri://localhost/mermaid.html?source=secret",
            "tauri://localhost/mermaid.html#other",
            "https://example.invalid/mermaid.html",
            "http://127.0.0.1:1421/mermaid.html",
            "file:///mermaid.html",
        ] {
            assert!(!renderer_location(&url.parse().unwrap()), "{url}");
        }
    }

    #[tokio::test]
    async fn binds_a_result_to_its_native_caller_and_holds_the_slot_until_disposal() {
        let jobs = RenderJobs::default();
        let first = jobs.insert("mermaid-first").unwrap();
        let second = jobs.insert("mermaid-second").unwrap();
        assert!(jobs.insert("mermaid-third").is_err());
        assert_eq!(
            jobs.complete("main", Ok("wrong".into())),
            Err("invalid_caller".into())
        );
        jobs.complete("mermaid-first", Ok("<svg/>".into())).unwrap();
        assert_eq!(first.await.unwrap(), Ok("<svg/>".into()));
        assert!(
            jobs.complete("mermaid-first", Ok("duplicate".into()))
                .is_err()
        );
        assert!(jobs.insert("mermaid-third").is_err());
        jobs.remove("mermaid-first");
        assert!(jobs.insert("mermaid-third").is_ok());
        jobs.remove("mermaid-second");
        assert!(second.await.is_err());
    }

    #[test]
    fn bounds_the_untrusted_result_and_rejects_ambiguous_messages() {
        assert!(checked_result(Some("x".repeat(MAX_SVG_BYTES)), None).is_ok());
        assert_eq!(
            checked_result(Some("x".repeat(MAX_SVG_BYTES + 1)), None),
            Err("mermaid_invalid_result".into())
        );
        assert_eq!(
            checked_result(Some("<svg/>".into()), Some("error".into())),
            Err("mermaid_invalid_result".into())
        );
        assert_eq!(
            checked_result(None, None),
            Err("mermaid_invalid_result".into())
        );
    }

    #[test]
    fn renderer_has_only_result_delivery_permission() {
        let renderer: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/mermaid.json")).unwrap();
        assert_eq!(
            renderer["permissions"],
            serde_json::json!(["allow-mermaid-complete"])
        );
        assert_eq!(renderer["windows"], serde_json::json!(["mermaid-*"]));
        let main: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/main.json")).unwrap();
        let permissions = main["permissions"].as_array().unwrap();
        assert!(permissions.contains(&serde_json::json!("allow-mermaid-render")));
        assert!(!permissions.contains(&serde_json::json!("allow-mermaid-complete")));
    }
}
