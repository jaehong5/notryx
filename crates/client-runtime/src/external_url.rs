#[derive(Clone, PartialEq, Eq)]
pub struct ExternalUrl(reqwest::Url);

impl ExternalUrl {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for ExternalUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ExternalUrl([redacted])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalUrlError {
    Invalid,
    UnsupportedPlatform,
    LaunchFailed,
}

pub fn validate_external_url(input: &str) -> Result<ExternalUrl, ExternalUrlError> {
    if input.is_empty()
        || input.len() > 8192
        || input
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
    {
        return Err(ExternalUrlError::Invalid);
    }
    let (scheme, remaining) = input.split_once("://").ok_or(ExternalUrlError::Invalid)?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err(ExternalUrlError::Invalid);
    }
    let authority = remaining
        .split(['/', '?', '#'])
        .next()
        .ok_or(ExternalUrlError::Invalid)?;
    if authority.is_empty() || authority.contains('@') {
        return Err(ExternalUrlError::Invalid);
    }
    let raw = input.as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let byte = if raw[index] == b'%' {
            if index + 2 >= raw.len() {
                return Err(ExternalUrlError::Invalid);
            }
            let hex = |byte: u8| -> Option<u8> {
                match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'a'..=b'f' => Some(byte - b'a' + 10),
                    b'A'..=b'F' => Some(byte - b'A' + 10),
                    _ => None,
                }
            };
            let high = hex(raw[index + 1]).ok_or(ExternalUrlError::Invalid)?;
            let low = hex(raw[index + 2]).ok_or(ExternalUrlError::Invalid)?;
            index += 3;
            (high << 4) | low
        } else {
            let byte = raw[index];
            index += 1;
            byte
        };
        if byte < 0x20 || byte == 0x7f || byte == b'\\' {
            return Err(ExternalUrlError::Invalid);
        }
        decoded.push(byte);
    }
    if String::from_utf8_lossy(&decoded)
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        return Err(ExternalUrlError::Invalid);
    }
    let url = reqwest::Url::parse(input).map_err(|_| ExternalUrlError::Invalid)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ExternalUrlError::Invalid);
    }
    Ok(ExternalUrl(url))
}

pub fn open_external_url(url: &ExternalUrl) -> Result<(), ExternalUrlError> {
    #[cfg(target_os = "linux")]
    {
        open::that_detached(url.as_str()).map_err(|_| ExternalUrlError::LaunchFailed)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = url;
        Err(ExternalUrlError::UnsupportedPlatform)
    }
}
