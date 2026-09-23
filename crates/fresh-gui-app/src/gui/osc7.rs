//! OSC 7 cwd extraction (shell integration), ported from the Vite host.

/// Parse OSC 7 payload (`file://host/path` or raw path) → absolute path.
pub fn parse_osc7(data: &str) -> Option<String> {
    if let Some(rest) = data.strip_prefix("file://") {
        let path = rest.find('/').map(|i| &rest[i..]).unwrap_or(rest);
        return decode_abs(path);
    }
    let s = data.trim();
    decode_abs(s)
}

fn decode_abs(s: &str) -> Option<String> {
    let mut decoded = percent_decode(s);
    // `file://host/C:/Users` arrives as `/C:/Users`. Keep the drive path.
    if let Some(rest) = decoded.strip_prefix('/')
        && rest.len() >= 3
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest.as_bytes()[1] == b':'
        && (rest.as_bytes()[2] == b'/' || rest.as_bytes()[2] == b'\\')
    {
        decoded = rest.to_string();
    }
    if decoded.starts_with('/') || decoded.starts_with('\\') {
        return Some(decoded);
    }
    let bytes = decoded.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        return Some(decoded);
    }
    None
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v as char);
            i += 3;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Scan a PTY chunk for OSC 7, keeping a short carry buffer for sequences
/// split across WebSocket frames.
pub fn feed_osc7_chunk(carry: &mut String, chunk: &str) -> Option<String> {
    let s = format!("{carry}{chunk}");
    let mut last = None;
    let mut last_end = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b']' && bytes[i + 2] == b'7' && bytes[i + 3] == b';'
        {
            let start = i + 4;
            let mut end = start;
            let mut term = None;
            while end < bytes.len() {
                if bytes[end] == 0x07 {
                    term = Some(end + 1);
                    break;
                }
                if bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == b'\\' {
                    term = Some(end + 2);
                    break;
                }
                end += 1;
            }
            if let Some(term_end) = term {
                let payload = &s[start..end];
                if let Some(cwd) = parse_osc7(payload) {
                    last = Some(cwd);
                }
                last_end = term_end;
                i = term_end;
                continue;
            }
            break;
        }
        i += 1;
    }
    let rest = &s[last_end..];
    let keep = 512.min(rest.len());
    let tail = &rest[rest.len() - keep..];
    if let Some(idx) = tail.rfind("\x1b]") {
        *carry = tail[idx..].to_string();
    } else {
        carry.clear();
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_url() {
        assert_eq!(
            parse_osc7("file://host/home/me/proj"),
            Some("/home/me/proj".into())
        );
        assert_eq!(
            parse_osc7("file://host/C:/Users/me/proj"),
            Some("C:/Users/me/proj".into())
        );
    }

    #[test]
    fn feeds_split_sequence() {
        let mut carry = String::new();
        assert!(feed_osc7_chunk(&mut carry, "\x1b]7;file://x/tmp/a").is_none());
        assert_eq!(
            feed_osc7_chunk(&mut carry, "bc\x07more"),
            Some("/tmp/abc".into())
        );
        assert!(carry.is_empty() || !carry.contains('\x07'));
    }
}
