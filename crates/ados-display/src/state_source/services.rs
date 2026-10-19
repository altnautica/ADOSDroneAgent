//! Profile-gated, rate-limited service reads: the sensing routes a visor panel
//! shows and the GNSS / radio-adapter / LoRa routes a ground-station panel
//! shows. A profile fetches only the routes its panel shows, and at most once
//! per [`SERVICE_READ_PERIOD`]; the render ticks in between reuse the last
//! reads.

use std::time::{Duration, Instant};

use super::*;
use crate::pages::gs_ctx::{GnssCtx, LoraCtx, RadioAdaptersCtx};
use crate::pages::reading::Reading;
use crate::pages::sensing_ctx::SensingCtx;

/// The shortest interval between two fetches of the service routes.
pub(super) const SERVICE_READ_PERIOD: Duration = Duration::from_secs(5);

/// Which service routes a profile's panel shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ServiceRoutes {
    pub sensing: bool,
    pub gnss: bool,
    pub radio: bool,
    pub lora: bool,
}

impl ServiceRoutes {
    /// The routes for a profile in the underscore spelling the panel tables
    /// key on.
    pub(super) fn for_profile(profile: &str) -> Self {
        match profile {
            "visor" => Self {
                sensing: true,
                ..Self::default()
            },
            "ground_station" => Self {
                gnss: true,
                radio: true,
                lora: true,
                ..Self::default()
            },
            // A drone panel lists LoRa in its More menu when a module is fitted.
            "drone" => Self {
                lora: true,
                ..Self::default()
            },
            _ => Self::default(),
        }
    }
}

/// The last service reads, copied into every context until the next fetch.
#[derive(Debug, Clone, Default)]
pub(super) struct ServiceReads {
    sensing: SensingCtx,
    gnss: Reading<GnssCtx>,
    radio_adapters: Reading<RadioAdaptersCtx>,
    lora: Reading<LoraCtx>,
}

impl ServiceReads {
    fn apply(&self, ctx: &mut PageContext) {
        ctx.sensing = self.sensing.clone();
        ctx.gnss = self.gnss.clone();
        ctx.radio_adapters = self.radio_adapters.clone();
        ctx.lora = self.lora.clone();
    }
}

/// The cached reads and when they were fetched.
#[derive(Debug, Default)]
pub(super) struct ServiceCache {
    reads: ServiceReads,
    fetched_at: Option<Instant>,
}

impl ServiceCache {
    /// Whether the reads are due for a fetch at `now`.
    fn due(&self, now: Instant) -> bool {
        match self.fetched_at {
            None => true,
            Some(at) => now.saturating_duration_since(at) >= SERVICE_READ_PERIOD,
        }
    }
}

impl StateSource {
    /// The node profile: the fixed one a test or caller set, otherwise
    /// resolved the way the panel page set resolves it.
    pub(super) fn service_profile(&self) -> String {
        match &self.profile {
            Some(p) => p.clone(),
            None => crate::panel_conf::PanelConf::load().profile,
        }
    }

    /// Copy the service reads into `ctx`, fetching them first when the last
    /// fetch is older than [`SERVICE_READ_PERIOD`].
    pub(super) fn apply_services(&mut self, ctx: &mut PageContext) {
        let now = Instant::now();
        if self.services.due(now) {
            let routes = ServiceRoutes::for_profile(&self.service_profile());
            self.services.reads = self.fetch_services(routes);
            self.services.fetched_at = Some(now);
        }
        self.services.reads.apply(ctx);
    }

    fn fetch_services(&self, routes: ServiceRoutes) -> ServiceReads {
        ServiceReads {
            sensing: if routes.sensing {
                self.fetch_sensing()
            } else {
                SensingCtx::default()
            },
            gnss: if routes.gnss {
                self.fetch_gnss()
            } else {
                Reading::Absent
            },
            radio_adapters: if routes.radio {
                self.fetch_radio_adapters()
            } else {
                Reading::Absent
            },
            lora: if routes.lora {
                self.fetch_lora()
            } else {
                Reading::Absent
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offline(profile: &str) -> StateSource {
        StateSource::with_paths(
            "http://127.0.0.1:1",
            Path::new("/nonexistent/pairing.json"),
            PathBuf::from("/nonexistent/hop.json"),
        )
        .with_profile(profile)
    }

    #[test]
    fn each_profile_reads_only_what_its_panel_shows() {
        let v = ServiceRoutes::for_profile("visor");
        assert!(v.sensing && !v.gnss && !v.radio && !v.lora);
        let g = ServiceRoutes::for_profile("ground_station");
        assert!(!g.sensing && g.gnss && g.radio && g.lora);
        let d = ServiceRoutes::for_profile("drone");
        assert!(!d.sensing && !d.gnss && !d.radio && d.lora);
        for p in ["workstation", "compute", ""] {
            assert_eq!(ServiceRoutes::for_profile(p), ServiceRoutes::default());
        }
    }

    #[test]
    fn a_failed_read_is_unreachable_and_an_unread_route_absent() {
        let mut ctx = PageContext::default();
        offline("drone").apply_services(&mut ctx);
        assert_eq!(ctx.lora, Reading::Unreachable);
        assert_eq!(ctx.gnss, Reading::Absent);
        assert_eq!(ctx.sensing, SensingCtx::default());

        let mut ctx = PageContext::default();
        offline("visor").apply_services(&mut ctx);
        assert_eq!(ctx.sensing.sensors, Reading::Unreachable);
        assert_eq!(ctx.sensing.capture, Reading::Unreachable);
        assert_eq!(ctx.radio_adapters, Reading::Absent);

        let mut ctx = PageContext::default();
        offline("workstation").apply_services(&mut ctx);
        assert!(ctx.gnss.is_absent() && ctx.lora.is_absent());
    }

    #[test]
    fn reads_are_reused_until_the_period_passes() {
        let mut src = offline("ground_station");
        let mut ctx = PageContext::default();
        src.apply_services(&mut ctx);
        let at = src.services.fetched_at.expect("fetched");
        // A tick inside the period keeps the same fetch.
        src.apply_services(&mut ctx);
        assert_eq!(src.services.fetched_at, Some(at));
        assert!(!src.services.due(at + SERVICE_READ_PERIOD / 2));
        assert!(src.services.due(at + SERVICE_READ_PERIOD));
        assert_eq!(ctx.gnss, Reading::Unreachable);
    }
}
