//! The tab registry: one state struct per tab, looked up by [`TabId`].

use super::{Tab, TabId};
use crate::ui::extensions::ExtensionsTab;
use crate::ui::ground::GroundTab;
use crate::ui::link::LinkTab;
use crate::ui::logs::LogsTab;
use crate::ui::overview::OverviewTab;
use crate::ui::params::ParamsTab;
use crate::ui::settings::SettingsTab;
use crate::ui::system::SystemTab;
use crate::ui::vehicle::VehicleTab;
use crate::ui::video::VideoTab;

/// Every tab's state. Slot 2 is the Vehicle tab, or the Ground tab on a
/// ground station.
#[derive(Default)]
pub struct Tabs {
    pub overview: OverviewTab,
    pub vehicle: VehicleTab,
    pub ground: GroundTab,
    pub link: LinkTab,
    pub video: VideoTab,
    pub params: ParamsTab,
    pub extensions: ExtensionsTab,
    pub logs: LogsTab,
    pub system: SystemTab,
    pub settings: SettingsTab,
}

impl Tabs {
    pub fn get(&self, id: TabId, profile: &str) -> &dyn Tab {
        match id {
            TabId::Overview => &self.overview,
            TabId::Vehicle if profile == "ground_station" => &self.ground,
            TabId::Vehicle => &self.vehicle,
            TabId::Link => &self.link,
            TabId::Video => &self.video,
            TabId::Params => &self.params,
            TabId::Extensions => &self.extensions,
            TabId::Logs => &self.logs,
            TabId::System => &self.system,
            TabId::Settings => &self.settings,
        }
    }

    pub fn get_mut(&mut self, id: TabId, profile: &str) -> &mut dyn Tab {
        match id {
            TabId::Overview => &mut self.overview,
            TabId::Vehicle if profile == "ground_station" => &mut self.ground,
            TabId::Vehicle => &mut self.vehicle,
            TabId::Link => &mut self.link,
            TabId::Video => &mut self.video,
            TabId::Params => &mut self.params,
            TabId::Extensions => &mut self.extensions,
            TabId::Logs => &mut self.logs,
            TabId::System => &mut self.system,
            TabId::Settings => &mut self.settings,
        }
    }

    /// Every tab, for the per-poll `on_global` hook.
    pub fn all_mut(&mut self) -> [&mut dyn Tab; 10] {
        [
            &mut self.overview,
            &mut self.vehicle,
            &mut self.ground,
            &mut self.link,
            &mut self.video,
            &mut self.params,
            &mut self.extensions,
            &mut self.logs,
            &mut self.system,
            &mut self.settings,
        ]
    }
}
