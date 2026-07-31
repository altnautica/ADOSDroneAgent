//! Generating operator-facing secrets, unambiguously and fail-closed.
//!
//! The pairing code established the contract this follows: draw from a charset
//! with no visually ambiguous characters, reject the tail of the random byte
//! range so the modulo is unbiased, and propagate an entropy failure rather
//! than degrading — a guessable secret is worse than no secret, and the caller
//! surfaces the error.
//!
//! It lives here rather than beside the pairing code because the access-point
//! passphrase needs exactly the same contract, and those two live in crates
//! that do not depend on one another.

/// Characters an operator can read off a screen and type without guessing.
///
/// `0/O` and `1/I/L` are excluded: these values get read aloud, copied off a
/// small display, and typed into a phone, and the difference between a
/// mistyped passphrase and a broken radio is not visible to the person doing
/// the typing.
pub const UNAMBIGUOUS_CHARSET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Length of a generated access-point passphrase.
///
/// WPA2-PSK accepts 8 to 63 printable ASCII characters, so the 6 of a pairing
/// code is not even legal here. Twelve from a 31-character alphabet is about
/// 59 bits — far past anything worth attacking a field radio over — while
/// still being short enough to read off an on-board display and type on a
/// phone at the bench.
///
/// Note for anyone tempted by hex: 32 bytes of hex is 64 characters, one past
/// the WPA2 maximum, and hostapd rejects the whole configuration rather than
/// truncating.
pub const AP_PASSPHRASE_LEN: usize = 12;

// Enforced at compile time rather than by a test: a length outside the WPA2
// bounds makes hostapd refuse its whole configuration, which takes the access
// point down entirely. That should not be able to build.
const _: () = assert!(
    AP_PASSPHRASE_LEN >= 8 && AP_PASSPHRASE_LEN <= 63,
    "the AP passphrase length must be legal for WPA2-PSK"
);

/// Draw `len` characters from [`UNAMBIGUOUS_CHARSET`].
///
/// Rejection-sampled: bytes at or above the largest multiple of the charset
/// length are discarded rather than folded in, so every character is equally
/// likely. Fail-closed on an entropy error.
pub fn generate(len: usize) -> Result<String, getrandom::Error> {
    let n = UNAMBIGUOUS_CHARSET.len() as u8;
    let limit = (256u16 - (256u16 % n as u16)) as u8;
    let mut out = String::with_capacity(len);
    while out.len() < len {
        let mut b = [0u8; 1];
        getrandom::getrandom(&mut b)?;
        if b[0] < limit {
            out.push(UNAMBIGUOUS_CHARSET[(b[0] % n) as usize] as char);
        }
    }
    Ok(out)
}

/// A fresh access-point passphrase, legal for WPA2-PSK.
pub fn generate_ap_passphrase() -> Result<String, getrandom::Error> {
    generate(AP_PASSPHRASE_LEN)
}

/// Whether a string is usable as a WPA2-PSK passphrase.
///
/// hostapd refuses the whole configuration on a bad passphrase rather than
/// correcting it, which takes the access point down entirely — so anything
/// destined for that file is checked before it is written.
pub fn is_valid_wpa2_passphrase(s: &str) -> bool {
    let len = s.chars().count();
    (8..=63).contains(&len) && s.chars().all(|c| c.is_ascii_graphic() || c == ' ')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_generated_passphrase_is_legal_for_wpa2() {
        let p = generate_ap_passphrase().expect("entropy");
        assert!(
            is_valid_wpa2_passphrase(&p),
            "hostapd refuses the whole config on a bad passphrase: {p:?}"
        );
        assert_eq!(p.chars().count(), AP_PASSPHRASE_LEN);
    }

    #[test]
    fn hex_of_32_bytes_would_have_been_illegal() {
        // Guards the obvious alternative: 32 bytes of hex is 64 characters,
        // one past the maximum, and hostapd rejects rather than truncates.
        assert!(!is_valid_wpa2_passphrase(&"a".repeat(64)));
        assert!(is_valid_wpa2_passphrase(&"a".repeat(63)));
    }

    #[test]
    fn only_unambiguous_characters_are_used() {
        // These are read off a small display and typed into a phone.
        for _ in 0..200 {
            let p = generate(16).expect("entropy");
            for c in p.chars() {
                assert!(
                    !"0O1IL".contains(c),
                    "ambiguous character {c:?} in generated secret {p:?}"
                );
                assert!(UNAMBIGUOUS_CHARSET.contains(&(c as u8)));
            }
        }
    }

    #[test]
    fn successive_draws_differ() {
        let mut seen = HashSet::new();
        for _ in 0..50 {
            assert!(
                seen.insert(generate_ap_passphrase().expect("entropy")),
                "a repeated passphrase means the draw is not random"
            );
        }
    }

    #[test]
    fn every_charset_position_is_reachable() {
        // A biased modulo would leave the tail of the alphabet unreachable.
        let mut seen = HashSet::new();
        for _ in 0..400 {
            for c in generate(32).expect("entropy").chars() {
                seen.insert(c);
            }
        }
        assert_eq!(
            seen.len(),
            UNAMBIGUOUS_CHARSET.len(),
            "some characters are never drawn, so the sampling is biased"
        );
    }

    #[test]
    fn a_zero_length_draw_is_empty_rather_than_looping() {
        assert_eq!(generate(0).expect("entropy"), "");
    }

    #[test]
    fn validation_rejects_the_obvious_bad_shapes() {
        assert!(!is_valid_wpa2_passphrase(""), "empty");
        assert!(!is_valid_wpa2_passphrase("short"), "under 8");
        assert!(
            !is_valid_wpa2_passphrase("with\ttab\tchars"),
            "control chars"
        );
        assert!(!is_valid_wpa2_passphrase("café-passphrase"), "non-ascii");
        assert!(is_valid_wpa2_passphrase("a valid one"), "spaces are legal");
    }
}
