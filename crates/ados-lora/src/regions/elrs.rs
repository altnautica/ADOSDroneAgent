//! The ExpressLRS regulatory domain expected per region, for the ground
//! station's "ELRS domain vs region" warning (never a block: the ELRS lane's
//! regulatory authority is the RC lane's own). Names are the firmware's domain
//! names (FHSS.cpp): `ISM2G4` and `CE_LBT` at 2.4 GHz, `FCC915`, `AU915`,
//! `EU868`, `IN866`, `AU433`, `EU433` below 1 GHz. A region whose expected
//! domain was not confirmed, or that has none, maps to `None`.

/// The expected `(2.4 GHz, sub-GHz)` ELRS domains for a region row code.
pub fn elrs_domains(code: &str) -> (Option<&'static str>, Option<&'static str>) {
    match code {
        "EU" | "GB" | "CH" | "NO" => (Some("CE_LBT"), Some("EU868")),
        "EU433" => (Some("CE_LBT"), Some("EU433")),
        "IN" => (Some("ISM2G4"), Some("IN866")),
        "US" | "CA" | "MX" => (Some("ISM2G4"), Some("FCC915")),
        "BR" | "AR" | "CL" | "AU" | "NZ" => (Some("ISM2G4"), Some("AU915")),
        "AU433" => (Some("ISM2G4"), Some("AU433")),
        "TR" | "AE" | "ZA" | "SA" => (None, Some("EU868")),
        "JP" | "KR" | "CN" | "TW" | "HK" | "SG" | "MY" | "TH" | "ID" | "PH" | "VN" => {
            (Some("ISM2G4"), None)
        }
        _ => (None, None),
    }
}
