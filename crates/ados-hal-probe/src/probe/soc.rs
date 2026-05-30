//! SoC device-tree compatible probe.
//!
//! Reads `/proc/device-tree/compatible` (a NUL-separated list, most-specific
//! first) and returns it as a [`SocCompatible`]. This is the authoritative SoC
//! identity the kernel exposes, used to key host quirks and board behavior.

#[cfg(target_os = "linux")]
use ados_protocol::hwcaps::{AbsenceReason, Evidence};
use ados_protocol::hwcaps::{Probed, SocCompatible};

/// The device-tree node the kernel exposes the SoC compatible list on.
#[cfg(target_os = "linux")]
const COMPATIBLE_NODE: &str = "/proc/device-tree/compatible";

/// Probe the SoC's device-tree compatible strings.
///
/// On a Linux host this reads `/proc/device-tree/compatible` and returns the
/// NUL-separated list most-specific first. Off Linux there is no device tree to
/// read, so the honest answer is [`Probed::NotProbed`] rather than a guessed
/// absence.
pub fn probe_soc() -> Probed<SocCompatible> {
    #[cfg(not(target_os = "linux"))]
    {
        Probed::NotProbed
    }

    #[cfg(target_os = "linux")]
    {
        let Ok(raw) = std::fs::read(COMPATIBLE_NODE) else {
            return Probed::absent(AbsenceReason::NodeMissing);
        };
        let compatibles = parse_compatible(&raw);
        match compatibles.first() {
            Some(first) => {
                let evidence = Evidence::DeviceTreeCompatible(first.clone());
                Probed::present(SocCompatible(compatibles), evidence)
            }
            // The node existed but carried no usable string (empty / all-NUL).
            None => Probed::absent(AbsenceReason::NodeMissing),
        }
    }
}

/// Split the raw `/proc/device-tree/compatible` bytes into the list of
/// compatible strings.
///
/// The node is a sequence of NUL-terminated strings (each entry, including the
/// last, is followed by a NUL), so a naive `split` on NUL yields a trailing
/// empty element that must be dropped. Empty / non-UTF-8-lossy entries are also
/// skipped so a stray NUL run never produces blank strings.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_compatible(raw: &[u8]) -> Vec<String> {
    raw.split(|&b| b == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nul_separated_list_most_specific_first() {
        let raw = b"radxa,cubie-a7z\0allwinner,sun60i-a733\0";
        let parsed = parse_compatible(raw);
        assert_eq!(
            parsed,
            vec![
                "radxa,cubie-a7z".to_string(),
                "allwinner,sun60i-a733".to_string(),
            ]
        );
        // The most-specific (board) string is first; the SoC follows.
        assert_eq!(parsed.first().unwrap(), "radxa,cubie-a7z");
    }

    #[test]
    fn handles_missing_trailing_nul() {
        // Some nodes are read without the final NUL; the last entry must survive.
        let raw = b"vendor,board\0vendor,soc";
        assert_eq!(
            parse_compatible(raw),
            vec!["vendor,board".to_string(), "vendor,soc".to_string()]
        );
    }

    #[test]
    fn single_entry() {
        let raw = b"brcm,bcm2711\0";
        assert_eq!(parse_compatible(raw), vec!["brcm,bcm2711".to_string()]);
    }

    #[test]
    fn drops_empty_runs_and_trailing_nul() {
        // A doubled NUL must not yield a blank string between the two entries.
        let raw = b"a,one\0\0a,two\0";
        assert_eq!(
            parse_compatible(raw),
            vec!["a,one".to_string(), "a,two".to_string()]
        );
    }

    #[test]
    fn empty_input_yields_no_compatibles() {
        assert!(parse_compatible(b"").is_empty());
        assert!(parse_compatible(b"\0\0").is_empty());
    }
}
