//! Bespoke settings pages, one file each.

use super::page::{Bespoke, BespokeId};

pub mod targets;
pub mod profile;
pub mod wifi;
pub mod hotspot;
pub mod cellular;
pub mod mac_pin;
pub mod discovery;
pub mod radio;
pub mod video_link;
pub mod sensors;
pub mod capture;
pub mod cloud;
pub mod remote_access;
pub mod pairing;
pub mod security;
pub mod mcp;
pub mod region;
pub mod display;
pub mod advanced;
pub mod kit;
pub mod rowkit;
pub mod wifi_model;

/// Every bespoke page's state.
#[derive(Default)]
pub struct Pages {
    pub targets: targets::Page,
    pub profile: profile::Page,
    pub wifi: wifi::Page,
    pub hotspot: hotspot::Page,
    pub cellular: cellular::Page,
    pub mac_pin: mac_pin::Page,
    pub discovery: discovery::Page,
    pub radio: radio::Page,
    pub video_link: video_link::Page,
    pub sensors: sensors::Page,
    pub capture: capture::Page,
    pub cloud: cloud::Page,
    pub remote_access: remote_access::Page,
    pub pairing: pairing::Page,
    pub security: security::Page,
    pub mcp: mcp::Page,
    pub region: region::Page,
    pub display: display::Page,
    pub advanced: advanced::Page,
}

impl Pages {
    pub fn get_mut(&mut self, id: BespokeId) -> &mut dyn Bespoke {
        match id {
            BespokeId::Targets => &mut self.targets,
            BespokeId::Profile => &mut self.profile,
            BespokeId::Wifi => &mut self.wifi,
            BespokeId::Hotspot => &mut self.hotspot,
            BespokeId::Cellular => &mut self.cellular,
            BespokeId::MacPin => &mut self.mac_pin,
            BespokeId::Discovery => &mut self.discovery,
            BespokeId::Radio => &mut self.radio,
            BespokeId::VideoLink => &mut self.video_link,
            BespokeId::Sensors => &mut self.sensors,
            BespokeId::Capture => &mut self.capture,
            BespokeId::Cloud => &mut self.cloud,
            BespokeId::RemoteAccess => &mut self.remote_access,
            BespokeId::Pairing => &mut self.pairing,
            BespokeId::Security => &mut self.security,
            BespokeId::Mcp => &mut self.mcp,
            BespokeId::Region => &mut self.region,
            BespokeId::Display => &mut self.display,
            BespokeId::Advanced => &mut self.advanced,
        }
    }

    pub fn get(&self, id: BespokeId) -> &dyn Bespoke {
        match id {
            BespokeId::Targets => &self.targets,
            BespokeId::Profile => &self.profile,
            BespokeId::Wifi => &self.wifi,
            BespokeId::Hotspot => &self.hotspot,
            BespokeId::Cellular => &self.cellular,
            BespokeId::MacPin => &self.mac_pin,
            BespokeId::Discovery => &self.discovery,
            BespokeId::Radio => &self.radio,
            BespokeId::VideoLink => &self.video_link,
            BespokeId::Sensors => &self.sensors,
            BespokeId::Capture => &self.capture,
            BespokeId::Cloud => &self.cloud,
            BespokeId::RemoteAccess => &self.remote_access,
            BespokeId::Pairing => &self.pairing,
            BespokeId::Security => &self.security,
            BespokeId::Mcp => &self.mcp,
            BespokeId::Region => &self.region,
            BespokeId::Display => &self.display,
            BespokeId::Advanced => &self.advanced,
        }
    }
}
