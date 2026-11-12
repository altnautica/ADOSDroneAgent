//! The navigation source the EKF is using and how well, resolved as the
//! shared `web-shared/src/hud/nav-source.ts` `resolveNavSource` resolves it:
//! from the FC's EKF status flags, its GPS, and the companion's nav report.
//! The terminal loads no `EK3_SRC*` parameters, so it takes the shared rule's
//! parameters-unknown branch, as the cockpit does before they load.

use super::autonomy::NavSource;
use super::vehicle::Telemetry;

/// EKF_STATUS_REPORT flag bits the rule reads.
const VEL_H: u32 = 2;
const POS_H_REL: u32 = 8;
const POS_H_ABS: u32 = 16;
const CONST_POS: u32 = 128;
const GPS_GLITCH: u32 = 1024;

/// An EKF report older than this is no report (ms).
pub const EKF_STALE_MS: u64 = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKind {
    Dr,
    Ext,
    Gps,
    Flow,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTone {
    Good,
    Warn,
    Crit,
    Muted,
}

/// The resolved source: its kind and tone (all the pill reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavPill {
    pub kind: NavKind,
    pub tone: NavTone,
}

impl NavKind {
    /// The source's spec word key (`nav_source.*`); none for a stale report.
    pub fn key(self) -> Option<&'static str> {
        match self {
            NavKind::Gps => Some("nav_source.gps"),
            NavKind::Ext => Some("nav_source.ext_nav"),
            NavKind::Flow => Some("nav_source.flow"),
            NavKind::Dr => Some("nav_source.dr"),
            NavKind::Stale => None,
        }
    }

    /// A shared case file's kind name.
    #[cfg(test)]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "dr" => NavKind::Dr,
            "ext" => NavKind::Ext,
            "gps" => NavKind::Gps,
            "flow" => NavKind::Flow,
            "stale" => NavKind::Stale,
            _ => return None,
        })
    }
}

impl NavTone {
    /// A shared case file's tone name.
    #[cfg(test)]
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "good" => NavTone::Good,
            "warn" => NavTone::Warn,
            "crit" => NavTone::Crit,
            "muted" => NavTone::Muted,
            _ => return None,
        })
    }
}

fn pill(kind: NavKind, tone: NavTone) -> NavPill {
    NavPill { kind, tone }
}

fn gps_row(flags: u32, tel: &Telemetry) -> NavPill {
    if tel.fix_type.unwrap_or(0) < 3 {
        return pill(NavKind::Gps, NavTone::Crit);
    }
    if flags & GPS_GLITCH != 0 {
        return pill(NavKind::Gps, NavTone::Warn);
    }
    match tel.eph {
        Some(h) if h < 1.5 => pill(NavKind::Gps, NavTone::Good),
        _ => pill(NavKind::Gps, NavTone::Warn),
    }
}

fn ext_row(flags: u32, nav: Option<&NavSource>) -> NavPill {
    match nav.and_then(|n| n.extnav_health.as_deref()) {
        Some("good") => pill(NavKind::Ext, NavTone::Good),
        Some("degraded") => pill(NavKind::Ext, NavTone::Warn),
        Some("lost") => pill(NavKind::Ext, NavTone::Crit),
        _ if flags & (POS_H_ABS | POS_H_REL) != 0 => pill(NavKind::Ext, NavTone::Good),
        _ => pill(NavKind::Ext, NavTone::Crit),
    }
}

/// The source from live telemetry and the fresh nav report (`None` when
/// absent or stale). Telemetry that is not live carries no EKF report.
pub fn resolve(tel: Option<&Telemetry>, nav: Option<&NavSource>) -> NavPill {
    let nav = nav.filter(|n| !n.stale);
    let Some((tel, flags)) = tel.and_then(|t| {
        t.ekf
            .filter(|(_, age)| *age <= EKF_STALE_MS)
            .map(|(f, _)| (t, f))
    }) else {
        return pill(NavKind::Stale, NavTone::Muted);
    };
    if flags & CONST_POS != 0 || flags & (VEL_H | POS_H_REL | POS_H_ABS) == 0 {
        return pill(NavKind::Dr, NavTone::Crit);
    }
    if flags & POS_H_ABS != 0 && tel.fix_type.unwrap_or(0) >= 3 {
        return gps_row(flags, tel);
    }
    if nav.is_some_and(|n| n.mode.as_deref() == Some("external_nav") && n.extnav_health.is_some()) {
        return ext_row(flags, nav);
    }
    if flags & POS_H_REL != 0 {
        return ext_row(flags, nav);
    }
    if flags & VEL_H != 0 {
        pill(NavKind::Flow, NavTone::Good)
    } else {
        pill(NavKind::Flow, NavTone::Crit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tel(flags: u32, age: u64, fix: i64, hdop: f64) -> Telemetry {
        Telemetry {
            vehicle: true,
            ekf: Some((flags, age)),
            fix_type: Some(fix),
            eph: Some(hdop),
            ..Telemetry::default()
        }
    }

    #[test]
    fn no_or_old_ekf_report_is_stale() {
        assert_eq!(resolve(None, None).kind, NavKind::Stale);
        let old = tel(POS_H_ABS | VEL_H, EKF_STALE_MS + 1, 3, 0.8);
        assert_eq!(resolve(Some(&old), None).tone, NavTone::Muted);
    }

    #[test]
    fn gps_reads_its_fix_and_hdop() {
        let good = tel(POS_H_ABS | VEL_H, 100, 3, 0.8);
        assert_eq!(
            resolve(Some(&good), None),
            pill(NavKind::Gps, NavTone::Good)
        );
        let poor = tel(POS_H_ABS | VEL_H, 100, 3, 3.0);
        assert_eq!(resolve(Some(&poor), None).tone, NavTone::Warn);
        let glitch = tel(POS_H_ABS | VEL_H | GPS_GLITCH, 100, 3, 0.8);
        assert_eq!(resolve(Some(&glitch), None).tone, NavTone::Warn);
    }

    #[test]
    fn constant_position_is_dead_reckoning() {
        let t = tel(CONST_POS | VEL_H, 100, 3, 0.8);
        assert_eq!(resolve(Some(&t), None), pill(NavKind::Dr, NavTone::Crit));
    }

    #[test]
    fn external_nav_follows_the_companion_health() {
        let t = tel(POS_H_REL | VEL_H, 100, 0, 0.0);
        let nav = |h: &str| NavSource {
            mode: Some("external_nav".into()),
            extnav_health: Some(h.into()),
            ..NavSource::default()
        };
        assert_eq!(
            resolve(Some(&t), Some(&nav("degraded"))).tone,
            NavTone::Warn
        );
        assert_eq!(resolve(Some(&t), Some(&nav("lost"))).tone, NavTone::Crit);
        // A stale report is no report: the flags decide.
        let stale = NavSource {
            stale: true,
            ..nav("lost")
        };
        assert_eq!(
            resolve(Some(&t), Some(&stale)),
            pill(NavKind::Ext, NavTone::Good)
        );
    }
}
