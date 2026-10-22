//! Country (ISO 3166-1 alpha-2) to plan-row mapping for the
//! country-code fallback.

/// EU member states and Iceland (EEA) follow the EU plan, which Commission
/// Implementing Decision (EU) 2019/1345 binds them to. Liechtenstein follows
/// the Swiss row (BAKOM RIR1008 covers both).
const EU_EEA: &[&str] = &[
    "AT", "BE", "BG", "HR", "CY", "CZ", "DK", "EE", "FI", "FR", "DE", "GR", "HU", "IE", "IT", "LV",
    "LT", "LU", "MT", "NL", "PL", "PT", "RO", "SK", "SI", "ES", "SE", "IS",
];

/// Countries with their own row (transmitting or research-only).
const OWN_ROW: &[&str] = &[
    "IN", "GB", "CH", "NO", "RU", "TR", "AE", "US", "CA", "BR", "CL", "AU", "JP", "KR", "TW", "MY",
    "TH", "ID", "VN", "CN", "NZ", "SG", "HK", "PH", "MX", "AR", "IL", "ZA", "SA",
];

/// The row code a country maps to, or `None` for a country without a plan.
pub fn country_plan(country: &str) -> Option<&'static str> {
    let c = country.trim().to_ascii_uppercase();
    if c == "LI" {
        return Some("CH");
    }
    if EU_EEA.contains(&c.as_str()) {
        return Some("EU");
    }
    OWN_ROW.iter().copied().find(|code| *code == c)
}
