use notryx_client_runtime::{ExternalUrlError, validate_external_url};

#[test]
fn http_and_https_links_keep_path_query_and_fragment() {
    let url = validate_external_url("https://example.org/folder/file%20name?q=one&b=two#section")
        .unwrap();
    assert_eq!(
        url.as_str(),
        "https://example.org/folder/file%20name?q=one&b=two#section"
    );
    assert!(validate_external_url("http://127.0.0.1:8080/").is_ok());
}

#[test]
fn rejects_other_schemes_credentials_controls_and_shell_text() {
    for invalid in [
        "",
        "//example.org",
        "https:example.org",
        "javascript:alert(1)",
        "file:///etc/passwd",
        "data:text/html,a",
        "mailto:user@example.org",
        "https://user:pass@example.org/",
        "https://user@example.org/",
        "https://@example.org/",
        "https://example.org/\nnext",
        "https://example.org/%0aHeader",
        "https://example.org/%00",
        "https://example.org/%7f",
        "https://example.org/%C2%85",
        "https://example.org/%FF%C2%85",
        "https://example.org/%E2%80%A8",
        "https://example.org/%FF%E2%80%A9",
        "https://example.org/%5Cevil",
        "https://example.org/\\evil",
        "https://example.org/a%0d%0a",
        "https://example.org/%xx",
        "sh -c 'touch /tmp/x'",
        "$(touch /tmp/x)",
        "; touch /tmp/x",
        " https://example.org/",
    ] {
        assert_eq!(
            validate_external_url(invalid),
            Err(ExternalUrlError::Invalid),
            "unexpectedly accepted invalid input"
        );
    }
    assert_eq!(
        validate_external_url(&format!("https://example.org/{}", "a".repeat(8192))),
        Err(ExternalUrlError::Invalid)
    );
}

#[test]
fn debug_output_redacts_query_and_entire_url() {
    let value = validate_external_url("https://example.org/?token=synthetic-secret").unwrap();
    assert_eq!(format!("{value:?}"), "ExternalUrl([redacted])");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_launcher_keeps_a_url_with_query_as_one_argument_without_a_shell() {
    let url = validate_external_url("https://example.org/path;test?q=one&other=$(whoami)#fragment")
        .unwrap();
    let commands = open::commands(url.as_str());
    let gio = commands
        .iter()
        .find(|command| command.get_program() == "gio")
        .unwrap();
    let args: Vec<_> = gio.get_args().collect();
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], "open");
    assert_eq!(args[1], url.as_str());
}
