//! The transmitting plans. Each limit is the most restrictive cited value for
//! the sub-bands the plan uses; where a cited cell admits two readings the
//! stricter one is enforced. A regulator that states a duty percentage without
//! an observation period gets the 3600 s rolling window of EN 300 220-1
//! clause 5.4.1, the default of the standard those rules cite.

use super::{DutyRule, DwellRule, Grid, LbtRule, Plan, PowerRef, PowerRule, SubBand};

const fn duty(limit_pct: f64) -> Option<DutyRule> {
    Some(DutyRule {
        limit_pct,
        window_s: 3600,
    })
}

const fn power(limit_mw: f64, reference: PowerRef) -> Option<PowerRule> {
    Some(PowerRule {
        limit_mw,
        reference,
        psd_dbm_per_3khz: None,
        antenna_max_dbi: None,
    })
}

const fn sb(lo_mhz: f64, hi_mhz: f64, duty: Option<DutyRule>, power: Option<PowerRule>) -> SubBand {
    SubBand {
        lo_mhz,
        hi_mhz,
        duty,
        power,
    }
}

const ERP25: Option<PowerRule> = power(25.0, PowerRef::Erp);

// ERC Recommendation 70-03 Annex 1 bands h1.3 .. h1.7 as used by the EU plan.
const EU_K: SubBand = sb(863.0, 865.0, duty(0.1), ERP25);
const EU_L: SubBand = sb(865.0, 868.0, duty(1.0), ERP25);
const EU_M: SubBand = sb(868.0, 868.6, duty(1.0), ERP25);
const EU_N: SubBand = sb(868.7, 869.2, duty(0.1), ERP25);
const EU_P: SubBand = sb(869.4, 869.65, duty(10.0), power(500.0, PowerRef::Erp));
const EU_Q: SubBand = sb(869.7, 870.0, duty(1.0), ERP25);

const fn eu_like(
    code: &'static str,
    name: &'static str,
    plan_id: &'static str,
    citation: &'static str,
    sub_bands: &'static [SubBand],
) -> Plan {
    Plan {
        code,
        name,
        plan_id,
        band_mhz: (863.0, 870.0),
        citation,
        sub_bands,
        dwell: None,
        lbt: None,
        grid: None,
        hop: false,
        hop_channels_min: 0,
        default_mhz: Some(868.125),
        bw_max_khz: None,
        apc: false,
        meshtastic: Some("EU_868"),
        tx: true,
    }
}

/// A plan with one sub-band and no dwell, LBT, grid or hopping.
#[allow(clippy::too_many_arguments)]
const fn simple(
    code: &'static str,
    name: &'static str,
    plan_id: &'static str,
    citation: &'static str,
    sub_bands: &'static [SubBand],
    band_mhz: (f64, f64),
    default_mhz: f64,
    meshtastic: Option<&'static str>,
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
        default_mhz: Some(default_mhz),
        bw_max_khz: None,
        apc: false,
        meshtastic,
        tx: true,
    }
}

const US_POWER: Option<PowerRule> = Some(PowerRule {
    limit_mw: 1000.0,
    reference: PowerRef::Conducted,
    psd_dbm_per_3khz: Some(8.0),
    antenna_max_dbi: Some(6.0),
});

const US_SUB_BANDS: &[SubBand] = &[sb(902.0, 928.0, None, US_POWER)];

/// 47 CFR 15.247 hybrid system: 400 ms per channel per 20 s.
const US_DWELL: Option<DwellRule> = Some(DwellRule {
    limit_ms: Some(400),
    per_s: Some(20),
    single_tx_max_ms: 400,
    pause_ms: 0,
});

const fn us_like(
    code: &'static str,
    name: &'static str,
    plan_id: &'static str,
    citation: &'static str,
) -> Plan {
    Plan {
        code,
        name,
        plan_id,
        band_mhz: (902.0, 928.0),
        citation,
        sub_bands: US_SUB_BANDS,
        dwell: US_DWELL,
        lbt: None,
        grid: None,
        hop: true,
        hop_channels_min: 0,
        default_mhz: None,
        bw_max_khz: None,
        apc: false,
        meshtastic: Some("US"),
        tx: true,
    }
}

/// ARIB STD-T108 / Korean LBT channels: 4 s per transmission then a 50 ms pause.
const ARIB_DWELL: Option<DwellRule> = Some(DwellRule {
    limit_ms: None,
    per_s: None,
    single_tx_max_ms: 4000,
    pause_ms: 50,
});

pub(super) static PLANS: &[Plan] = &[
    Plan {
        apc: true,
        bw_max_khz: Some(200),
        ..simple(
            "IN",
            "India",
            "IN865",
            "G.S.R. 853(E), 2021, Table II (clamped to 25 mW ERP and 1 %)",
            &[sb(865.0, 867.0, duty(1.0), ERP25)],
            (865.0, 867.0),
            866.125,
            Some("IN"),
        )
    },
    eu_like(
        "EU",
        "European Union (ETSI)",
        "EU868",
        "ERC Recommendation 70-03 Annex 1; Commission Implementing Decision (EU) 2019/1345",
        &[EU_K, EU_L, EU_M, EU_N, EU_P, EU_Q],
    ),
    simple(
        "EU433",
        "European Union 433 MHz",
        "EU433",
        "ERC Recommendation 70-03 Annex 1",
        &[sb(433.05, 434.79, duty(10.0), power(10.0, PowerRef::Erp))],
        (433.05, 434.79),
        433.125,
        Some("EU_433"),
    ),
    eu_like(
        "GB",
        "United Kingdom",
        "EU868-GB",
        "Ofcom IR 2030",
        &[EU_M, EU_N, EU_P, EU_Q],
    ),
    eu_like(
        "CH",
        "Switzerland",
        "EU868",
        "BAKOM RIR1008",
        &[EU_K, EU_L, EU_M, EU_N, EU_P, EU_Q],
    ),
    eu_like(
        "NO",
        "Norway",
        "EU868-NO",
        "Norway fribruksforskriften section 8",
        &[EU_K, EU_M, EU_N, EU_P, EU_Q],
    ),
    Plan {
        meshtastic: None,
        band_mhz: (864.0, 868.0),
        ..simple(
            "RU",
            "Russian Federation",
            "RU864",
            "GKRCh decision 07-20-03-001 (amended 2023)",
            &[
                sb(864.0, 865.0, duty(0.1), ERP25),
                sb(866.0, 868.0, duty(1.0), ERP25),
            ],
            (864.0, 868.0),
            866.125,
            None,
        )
    },
    eu_like(
        "TR",
        "Türkiye",
        "EU868-TR",
        "BTK class-1 technical criteria",
        &[EU_M, EU_N, EU_P],
    ),
    Plan {
        band_mhz: (863.0, 920.8),
        ..eu_like(
            "AE",
            "United Arab Emirates",
            "EU868-AE",
            "TDRA UWB and SRD Regulations v5.0",
            &[
                EU_K,
                EU_L,
                EU_M,
                EU_N,
                EU_P,
                EU_Q,
                sb(915.2, 920.8, duty(1.0), ERP25),
            ],
        )
    },
    us_like("US", "United States", "US915", "47 CFR 15.247"),
    us_like("CA", "Canada", "US915-CA", "ISED RSS-247 Issue 4"),
    Plan {
        code: "BR",
        name: "Brazil",
        plan_id: "AU915-BR",
        band_mhz: (902.0, 928.0),
        citation: "Anatel Ato 14448 (fewer than 35 hop channels: 250 mW)",
        sub_bands: &[
            sb(902.0, 907.5, None, power(250.0, PowerRef::Conducted)),
            sb(915.0, 928.0, None, power(250.0, PowerRef::Conducted)),
        ],
        dwell: Some(DwellRule {
            limit_ms: Some(400),
            per_s: Some(14),
            single_tx_max_ms: 400,
            pause_ms: 0,
        }),
        lbt: None,
        grid: None,
        hop: true,
        hop_channels_min: 0,
        default_mhz: None,
        bw_max_khz: Some(500),
        apc: false,
        meshtastic: Some("BR_902"),
        tx: true,
    },
    simple(
        "CL",
        "Chile",
        "AU915-CL",
        "SUBTEL Res. 1985 (updated 2021); radiated power enforced as EIRP",
        &[sb(915.0, 928.0, None, power(500.0, PowerRef::Eirp))],
        (915.0, 928.0),
        915.125,
        Some("ANZ"),
    ),
    simple(
        "AU",
        "Australia",
        "AU915",
        "ACMA LIPD Class Licence 2025",
        &[sb(
            915.0,
            928.0,
            None,
            Some(PowerRule {
                limit_mw: 1000.0,
                reference: PowerRef::Eirp,
                // 25 mW per 3 kHz.
                psd_dbm_per_3khz: Some(13.979_400_086_720_377),
                antenna_max_dbi: None,
            }),
        )],
        (915.0, 928.0),
        915.125,
        Some("ANZ"),
    ),
    simple(
        "AU433",
        "Australia 433 MHz",
        "AU433",
        "ACMA LIPD Class Licence 2025; reference enforced as EIRP",
        &[sb(433.05, 434.79, None, power(25.0, PowerRef::Eirp))],
        (433.05, 434.79),
        433.125,
        Some("ANZ_433"),
    ),
    Plan {
        code: "JP",
        name: "Japan",
        plan_id: "AS923-1-JP",
        band_mhz: (920.5, 923.5),
        citation: "ARIB STD-T108 v1.5",
        sub_bands: &[sb(
            920.5,
            923.5,
            None,
            Some(PowerRule {
                limit_mw: 20.0,
                reference: PowerRef::Conducted,
                psd_dbm_per_3khz: None,
                antenna_max_dbi: Some(3.0),
            }),
        )],
        dwell: ARIB_DWELL,
        lbt: Some(LbtRule {
            threshold_dbm: -80,
            listen_us: 5000,
            gap_max_us: 5000,
        }),
        grid: Some(Grid {
            start_mhz: 920.6,
            step_mhz: 0.2,
        }),
        hop: false,
        hop_channels_min: 0,
        default_mhz: None,
        bw_max_khz: Some(200),
        apc: false,
        meshtastic: Some("JP"),
        tx: true,
    },
    Plan {
        code: "KR",
        name: "Republic of Korea",
        plan_id: "KR920",
        band_mhz: (920.9, 923.3),
        citation: "MSIT Notice 2024-22; LoRa Alliance RP002-1.0.0",
        sub_bands: &[
            sb(920.9, 921.9, None, power(10.0, PowerRef::Eirp)),
            sb(922.1, 923.3, None, power(25.0, PowerRef::Eirp)),
        ],
        dwell: ARIB_DWELL,
        lbt: Some(LbtRule {
            threshold_dbm: -65,
            listen_us: 5000,
            gap_max_us: 5000,
        }),
        grid: Some(Grid {
            start_mhz: 920.9,
            step_mhz: 0.2,
        }),
        hop: false,
        hop_channels_min: 0,
        default_mhz: None,
        bw_max_khz: None,
        apc: false,
        meshtastic: Some("KR"),
        tx: true,
    },
    Plan {
        code: "TW",
        name: "Taiwan",
        plan_id: "AS923-1-TW",
        band_mhz: (920.0, 925.0),
        citation: "NCC LP0002 (2024)",
        sub_bands: &[sb(
            920.0,
            925.0,
            None,
            Some(PowerRule {
                limit_mw: 500.0,
                reference: PowerRef::Conducted,
                psd_dbm_per_3khz: None,
                antenna_max_dbi: Some(6.0),
            }),
        )],
        dwell: Some(DwellRule {
            limit_ms: Some(400),
            per_s: Some(4),
            single_tx_max_ms: 400,
            pause_ms: 0,
        }),
        lbt: None,
        grid: None,
        hop: true,
        hop_channels_min: 10,
        default_mhz: None,
        bw_max_khz: Some(500),
        apc: false,
        meshtastic: Some("TW"),
        tx: true,
    },
    simple(
        "MY",
        "Malaysia",
        "AS923-1-MY",
        "MCMC MTSFB TC T007:2020",
        &[sb(923.0, 924.0, duty(1.0), power(500.0, PowerRef::Eirp))],
        (923.0, 924.0),
        923.125,
        Some("MY_919"),
    ),
    simple(
        "TH",
        "Thailand",
        "AS923-1-TH",
        "NBTC TS 1033-2560 (duty class enforced at 1 %)",
        &[sb(920.0, 925.0, duty(1.0), power(50.0, PowerRef::Eirp))],
        (920.0, 925.0),
        922.125,
        Some("TH"),
    ),
    Plan {
        bw_max_khz: Some(250),
        ..simple(
            "ID",
            "Indonesia",
            "AS923-2-ID",
            "Kepmen Kominfo 05/2024",
            &[sb(920.0, 923.0, duty(1.0), power(100.0, PowerRef::Eirp))],
            (920.0, 923.0),
            921.125,
            None,
        )
    },
    simple(
        "VN",
        "Viet Nam",
        "AS923-2-VN",
        "Circular 08/2021/TT-BTTTT",
        &[sb(920.0, 923.0, duty(1.0), ERP25)],
        (920.0, 923.0),
        921.125,
        None,
    ),
];
