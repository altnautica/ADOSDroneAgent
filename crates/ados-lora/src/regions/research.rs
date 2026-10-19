//! Research-only rows: regions whose enforced limits could not all be cited
//! from the regulator text. They are listed with their bands and no rules;
//! resolving to one blocks transmit with `region_unset` while receive stays on.
//! A row moves to the transmitting table only when a later research pass cites
//! every limit it would enforce.

use super::{Plan, SubBand};

const fn band(lo_mhz: f64, hi_mhz: f64) -> SubBand {
    SubBand {
        lo_mhz,
        hi_mhz,
        duty: None,
        power: None,
    }
}

const fn row(
    code: &'static str,
    name: &'static str,
    plan_id: &'static str,
    citation: &'static str,
    band_mhz: (f64, f64),
    sub_bands: &'static [SubBand],
) -> Plan {
    Plan {
        code,
        name,
        plan_id,
        band_mhz,
        citation,
        sub_bands,
        dwell: None,
        lbt: None,
        grid: None,
        hop: false,
        hop_channels_min: 0,
        default_mhz: None,
        bw_max_khz: None,
        apc: false,
        meshtastic: None,
        tx: false,
    }
}

const CN: &[SubBand] = &[band(470.0, 510.0)];
const NZ: &[SubBand] = &[band(915.0, 928.0)];
const SG: &[SubBand] = &[band(917.0, 925.0)];
const HK: &[SubBand] = &[band(920.0, 925.0)];
const PH: &[SubBand] = &[band(868.0, 869.4)];
const MX: &[SubBand] = &[band(902.0, 928.0)];
const AR: &[SubBand] = &[band(915.0, 928.0)];
const IL: &[SubBand] = &[band(917.0, 920.0)];
const ZA: &[SubBand] = &[band(865.0, 870.0)];
const SA: &[SubBand] = &[band(863.0, 875.8)];

pub(super) static RESEARCH: &[Plan] = &[
    row(
        "CN",
        "China",
        "CN470",
        "MIIT Announcement 2019 No. 52 (search-before-transmit threshold not specified; 779-787 MHz closed to new devices)",
        (470.0, 510.0),
        CN,
    ),
    row("NZ", "New Zealand", "AU915", "RSM GURL short range devices (special conditions not read)", (915.0, 928.0), NZ),
    row("SG", "Singapore", "AS923-1", "IMDA TS SRD (row-to-limit mapping not confirmed)", (917.0, 925.0), SG),
    row("HK", "Hong Kong", "AS923-1", "OFCA short range devices (licence status for private use not confirmed)", (920.0, 925.0), HK),
    row("PH", "Philippines", "AS923-3", "NTC MC 03-06-2017 (duty not stated)", (868.0, 869.4), PH),
    row("MX", "Mexico", "US915", "IFT-008-2015 (text not machine-readable)", (902.0, 928.0), MX),
    row("AR", "Argentina", "AU915", "Res. 581/2018 (annex not retrievable)", (915.0, 928.0), AR),
    row("IL", "Israel", "AS923-4", "LoRa Alliance RP002-1.0.3 (regulator page not retrievable)", (917.0, 920.0), IL),
    row("ZA", "South Africa", "EU868", "ICASA RF Spectrum Regulations 2015 (gazette table is a scanned image)", (865.0, 870.0), ZA),
    row("SA", "Saudi Arabia", "EU868", "LoRa Alliance RP002-1.0.3 (CST limits not confirmed)", (863.0, 875.8), SA),
];
