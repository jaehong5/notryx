use super::{MAX_SVG_BYTES, RenderResult, checked_result};

fn decode_result(reply: &str) -> Option<RenderResult> {
    if reply.is_empty() {
        return None;
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Reply {
        svg: Option<String>,
        error: Option<String>,
    }
    // evaluateJavascript can escape each UTF-16 code unit as six JSON bytes.
    let result = if reply.len() > MAX_SVG_BYTES * 6 + 1024 {
        Err("mermaid_invalid_result".into())
    } else {
        serde_json::from_str::<Reply>(reply)
            .map_err(|_| "mermaid_invalid_result".to_owned())
            .and_then(|reply| checked_result(reply.svg, reply.error))
    };
    Some(result)
}

#[cfg(target_os = "android")]
mod native {
    use super::*;
    use jni::objects::{JString, JValue};
    use tauri::Webview;

    async fn call(
        webview: &Webview,
        method: &'static str,
        id: &str,
        input: Option<&str>,
    ) -> RenderResult {
        let id = id.to_owned();
        let input = input.map(str::to_owned);
        let (send, receive) = tokio::sync::oneshot::channel();
        webview
            .with_webview(move |native| {
                native.jni_handle().exec(move |env, activity, _| {
                    if send.is_closed() {
                        return;
                    }
                    let result = env.with_local_frame(8, |env| -> jni::errors::Result<String> {
                        let id = env.new_string(id)?;
                        let reply = if let Some(input) = input {
                            let input = env.new_string(input)?;
                            env.call_method(
                                activity,
                                method,
                                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                                &[JValue::Object(&id), JValue::Object(&input)],
                            )?
                        } else {
                            env.call_method(
                                activity,
                                method,
                                "(Ljava/lang/String;)Ljava/lang/String;",
                                &[JValue::Object(&id)],
                            )?
                        };
                        let reply = JString::from(reply.l()?);
                        Ok(env.get_string(&reply)?.into())
                    });
                    let result = result.map_err(|_| {
                        let _ = env.exception_clear();
                        "mermaid_unavailable".to_owned()
                    });
                    let _ = send.send(result);
                });
            })
            .map_err(|_| "mermaid_unavailable")?;
        receive.await.map_err(|_| "mermaid_cancelled")?
    }

    fn cancel(webview: &Webview, id: Option<String>) {
        let _ = webview.with_webview(move |native| {
            native.jni_handle().exec(move |env, activity, _| {
                let result = if let Some(id) = id {
                    env.new_string(id).and_then(|id| {
                        env.call_method(
                            activity,
                            "cancelMermaidRender",
                            "(Ljava/lang/String;)V",
                            &[JValue::Object(&id)],
                        )
                    })
                } else {
                    env.call_method(activity, "cancelAllMermaidRenders", "()V", &[])
                };
                if result.is_err() {
                    let _ = env.exception_clear();
                }
            });
        });
    }

    struct Lease {
        webview: Webview,
        id: String,
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            cancel(&self.webview, Some(self.id.clone()));
        }
    }

    pub(crate) async fn render(webview: Webview, source: String, theme: String) -> RenderResult {
        let lease = Lease {
            webview,
            id: uuid::Uuid::new_v4().to_string(),
        };
        let input = serde_json::json!({ "source": source, "theme": theme }).to_string();
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let error = call(
                &lease.webview,
                "startMermaidRender",
                &lease.id,
                Some(&input),
            )
            .await?;
            if !error.is_empty() {
                return Err(error);
            }
            loop {
                let reply = call(&lease.webview, "pollMermaidRender", &lease.id, None).await?;
                if let Some(result) = decode_result(&reply) {
                    return result;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| "mermaid_render_timeout")?
    }

    pub(crate) fn cancel_all(webview: &Webview) {
        cancel(webview, None);
    }
}

#[cfg(target_os = "android")]
pub(super) use native::{cancel_all, render};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polls_pending_and_validates_bounded_untrusted_results() {
        assert_eq!(decode_result(""), None);
        assert_eq!(
            decode_result(r#"{"svg":"<svg/>","error":null}"#),
            Some(Ok("<svg/>".into()))
        );
        assert_eq!(
            decode_result(r#"{"svg":null,"error":"mermaid_parse_error"}"#),
            Some(Err("mermaid_parse_error".into()))
        );
        for invalid in [
            "null".to_owned(),
            "{}".into(),
            r#"{"svg":"<svg/>","error":"ambiguous"}"#.into(),
            serde_json::json!({"svg": "가".repeat(MAX_SVG_BYTES / 3 + 1), "error": null})
                .to_string(),
        ] {
            assert_eq!(
                decode_result(&invalid),
                Some(Err("mermaid_invalid_result".into()))
            );
        }
    }
}
