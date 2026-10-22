//! The agent's MAC-pin file layout for the `ados-macpin` engine.

/// Where the per-adapter verdicts + learner memory are persisted.
pub const MAC_PINS_STATE: &str = "/etc/ados/mac-pins.state";

/// The file-name prefix every MAC-pin `.link` drop-in carries. The `10-` sorts
/// before the stock board files (`50-...`) so the pin wins.
pub const MAC_PIN_LINK_PREFIX: &str = "10-ados-mac-";

/// The agent's [`ados_macpin::engine::PinLayout`].
pub fn ados_pin_layout() -> ados_macpin::engine::PinLayout {
    ados_macpin::engine::PinLayout {
        state_path: MAC_PINS_STATE.into(),
        networkd_dir: ados_macpin::engine::NETWORKD_DIR.into(),
        link_prefix: MAC_PIN_LINK_PREFIX.into(),
        owner_label: "ADOS agent".into(),
    }
}
