//! A minimal HTTP GET over WinHTTP.
//!
//! Online lyric lookup is opt-in and off by default, so pulling in a full HTTP
//! stack with its own TLS backend would be a lot of build time and binary size
//! for something most users never enable. WinHTTP ships with Windows, uses the
//! system proxy and certificate store, and needs about a hundred lines.

use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPEN_REQUEST_FLAGS,
};

/// Largest response we will buffer. A lyric file is a few kilobytes; anything
/// larger is a misconfigured endpoint.
const MAX_BODY: usize = 1024 * 1024;

/// A parsed absolute URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub secure: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    pub fn parse(input: &str) -> Option<Url> {
        // `https` is tried first: `http://` is not a prefix of it, but keeping
        // the order explicit documents the preference.
        let (secure, rest) = input
            .strip_prefix("https://")
            .map(|rest| (true, rest))
            .or_else(|| input.strip_prefix("http://").map(|rest| (false, rest)))?;

        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return None;
        }

        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, if secure { 443 } else { 80 }),
        };
        if host.is_empty() {
            return None;
        }

        Some(Url {
            secure,
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }
}

/// RAII wrapper so a WinHTTP handle can never leak on an early return.
struct Handle(*mut core::ffi::c_void);

impl Handle {
    fn get(&self) -> *mut core::ffi::c_void {
        self.0
    }

    fn is_null(&self) -> bool {
        self.0.is_null()
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = WinHttpCloseHandle(self.0);
            }
        }
    }
}

/// Wide, NUL-terminated copy of `s` that lives as long as the returned `Vec`.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Blocking GET with no extra headers. Returns the body as UTF-8.
pub fn get(url: &str, timeout_ms: i32) -> Result<String, String> {
    get_with_headers(url, timeout_ms, &[])
}

/// Blocking GET. Returns the response body as UTF-8 (lossily decoded).
///
/// `timeout_ms` is applied to each phase, so a hung server cannot wedge the
/// caller — this runs on the media thread.
///
/// Some lyric providers require a `Referer` or a browser `User-Agent`; WinHTTP
/// takes the whole header block as one CRLF-terminated string.
pub fn get_with_headers(
    url: &str,
    timeout_ms: i32,
    headers: &[(&str, &str)],
) -> Result<String, String> {
    let parsed = Url::parse(url).ok_or_else(|| format!("unsupported url: {url}"))?;
    // WinHTTP takes the whole header block as one CRLF-terminated string.
    let mut header_text = String::new();
    for (name, value) in headers {
        header_text.push_str(name);
        header_text.push_str(": ");
        header_text.push_str(value);
        header_text.push_str("\r\n");
    }
    let header_block: Vec<u16> = header_text.encode_utf16().collect();

    let host = wide(&parsed.host);
    let path = wide(&parsed.path);
    let agent = wide("WinBeautify/0.1");

    unsafe {
        let session = Handle(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ));
        if session.is_null() {
            return Err("WinHttpOpen failed".into());
        }
        let _ = WinHttpSetTimeouts(session.get(), timeout_ms, timeout_ms, timeout_ms, timeout_ms);

        let connection = Handle(WinHttpConnect(
            session.get(),
            PCWSTR(host.as_ptr()),
            parsed.port,
            0,
        ));
        if connection.is_null() {
            return Err("WinHttpConnect failed".into());
        }

        let flags: WINHTTP_OPEN_REQUEST_FLAGS = if parsed.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = Handle(WinHttpOpenRequest(
            connection.get(),
            w!("GET"),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            // A null accept-types list is fine; an empty list would send an
            // empty Accept header, which some servers reject.
            std::ptr::null(),
            flags,
        ));
        if request.is_null() {
            return Err("WinHttpOpenRequest failed".into());
        }

        let header_slice = (!header_block.is_empty()).then_some(header_block.as_slice());
        WinHttpSendRequest(request.get(), header_slice, None, 0, 0, 0)
            .map_err(|e| format!("WinHttpSendRequest: {e}"))?;
        WinHttpReceiveResponse(request.get(), std::ptr::null_mut())
            .map_err(|e| format!("WinHttpReceiveResponse: {e}"))?;

        let mut body: Vec<u8> = Vec::new();
        loop {
            let mut available: u32 = 0;
            WinHttpQueryDataAvailable(request.get(), &mut available)
                .map_err(|e| format!("WinHttpQueryDataAvailable: {e}"))?;
            if available == 0 {
                break;
            }
            let take = (available as usize).min(MAX_BODY - body.len());
            if take == 0 {
                break;
            }
            let mut buf = vec![0u8; take];
            let mut read: u32 = 0;
            WinHttpReadData(
                request.get(),
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                take as u32,
                &mut read,
            )
            .map_err(|e| format!("WinHttpReadData: {e}"))?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
        }

        // Charset detection is not worth implementing: LRC files are UTF-8 in
        // practice and `from_utf8_lossy` degrades gracefully on GBK.
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// Percent-encode a query parameter value.
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The URL to fetch for a track, expanding the configured template.
pub fn build_lyric_url(template: &str, title: &str, artist: &str, album: &str) -> Option<String> {
    if template.trim().is_empty() {
        return None;
    }
    Some(
        template
            .replace("{title}", &encode_component(title))
            .replace("{artist}", &encode_component(artist))
            .replace("{album}", &encode_component(album)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_https_urls() {
        let u = Url::parse("https://example.com/api/lrc?a=1").unwrap();
        assert!(u.secure);
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/api/lrc?a=1");
    }

    #[test]
    fn parses_http_with_explicit_port_and_missing_path() {
        let u = Url::parse("http://localhost:8080").unwrap();
        assert!(!u.secure);
        assert_eq!(u.host, "localhost");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/");
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(Url::parse("file:///c:/x").is_none());
        assert!(Url::parse("example.com/x").is_none());
        assert!(Url::parse("https://").is_none());
    }

    #[test]
    fn encodes_query_components() {
        assert_eq!(encode_component("hello world"), "hello%20world");
        assert_eq!(encode_component("周杰伦"), "%E5%91%A8%E6%9D%B0%E4%BC%A6");
        assert_eq!(encode_component("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(encode_component("x&y=z"), "x%26y%3Dz");
    }

    #[test]
    fn expands_templates() {
        let url = build_lyric_url(
            "https://api.example/lrc?t={title}&a={artist}&al={album}",
            "My Song",
            "A&B",
            "",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://api.example/lrc?t=My%20Song&a=A%26B&al="
        );
        assert!(build_lyric_url("  ", "t", "a", "").is_none());
    }
}
