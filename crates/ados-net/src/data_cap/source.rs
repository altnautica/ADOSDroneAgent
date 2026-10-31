//! The sysfs-backed modem usage source.

use super::*;

impl SysfsUsageSource {
    /// Default sysfs net dir + the canonical provisioned-gadget marker.
    pub fn new() -> Self {
        Self {
            net_dir: PathBuf::from("/sys/class/net"),
            gadget_dir: PathBuf::from(USB_GADGET_DIR),
        }
    }

    /// Explicit roots (tests point these at a tempdir).
    pub fn with_roots(net_dir: PathBuf, gadget_dir: PathBuf) -> Self {
        Self {
            net_dir,
            gadget_dir,
        }
    }

    /// The modem's current interface. `None` means "count nothing" (no modem
    /// iface, or `usb0` is the tether).
    fn modem_iface(&self) -> Option<String> {
        crate::managers::modem::resolve_cellular_iface(&self.net_dir, &self.gadget_dir)
    }

    fn read_counter(&self, iface: &str, counter: &str) -> Option<u64> {
        let path = self.net_dir.join(iface).join("statistics").join(counter);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
    }
}

impl Default for SysfsUsageSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl UsageSource for SysfsUsageSource {
    async fn data_usage(&self) -> Option<UsageSample> {
        let iface = self.modem_iface()?;
        let rx_bytes = self.read_counter(&iface, "rx_bytes")?;
        let tx_bytes = self.read_counter(&iface, "tx_bytes")?;
        Some(UsageSample {
            iface,
            bytes: UsageBytes { rx_bytes, tx_bytes },
        })
    }
}
