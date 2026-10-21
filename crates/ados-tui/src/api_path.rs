//! Building API paths from data (SSIDs, plugin ids, interface names).

/// Percent-encode one path segment: everything outside the RFC 3986
/// unreserved set (`A-Z a-z 0-9 - . _ ~`) becomes `%XX` per UTF-8 byte, so a
/// `/`, space or `?` in a value can never change the route it lands on.
pub fn seg(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_bytes_are_encoded() {
        assert_eq!(seg("Home WiFi/5G?"), "Home%20WiFi%2F5G%3F");
        assert_eq!(
            seg("com.altnautica.world-engine"),
            "com.altnautica.world-engine"
        );
        assert_eq!(seg("café"), "caf%C3%A9");
    }
}
