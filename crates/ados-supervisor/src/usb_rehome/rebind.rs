//! The sysfs unbind/rebind an authorized rehome plan executes.

#[cfg(target_os = "linux")]
use std::time::Duration;

use super::RehomePlan;

/// Settle between the unbind and the bind so the device node fully drops.
#[cfg(target_os = "linux")]
const REHOME_SETTLE_UNBIND: Duration = Duration::from_millis(1500);
/// Bounded wait for the interface to re-enumerate after the bind.
#[cfg(target_os = "linux")]
const REHOME_REENUM_CEILING: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const REHOME_REENUM_STEP: Duration = Duration::from_millis(200);
/// The USB core driver's bind/unbind sysfs attributes.
#[cfg(target_os = "linux")]
const USB_UNBIND_PATH: &str = "/sys/bus/usb/drivers/usb/unbind";
#[cfg(target_os = "linux")]
const USB_BIND_PATH: &str = "/sys/bus/usb/drivers/usb/bind";

/// Execute the sysfs unbind/rebind for an authorized plan. Called by the
/// supervisor BETWEEN stopping and starting the radio unit, so no live injector
/// races the rebind. Best-effort; a sysfs write failure is logged and the radio
/// restart still re-probes the adapter.
#[cfg(target_os = "linux")]
pub async fn execute_rebind(plan: &RehomePlan) {
    tracing::warn!(iface = %plan.iface, bind_id = %plan.bind_id, attempt = plan.attempt, "usb_rehome_unbind_rebind");
    if let Err(e) = sysfs_write(USB_UNBIND_PATH, &plan.bind_id).await {
        tracing::warn!(error = %e, "usb_rehome unbind failed");
    }
    tokio::time::sleep(REHOME_SETTLE_UNBIND).await;
    if let Err(e) = sysfs_write(USB_BIND_PATH, &plan.bind_id).await {
        tracing::warn!(error = %e, "usb_rehome bind failed");
    }
    // Wait (bounded) for the interface's device link to resolve again.
    let deadline = tokio::time::Instant::now() + REHOME_REENUM_CEILING;
    let link = format!("/sys/class/net/{}/device", plan.iface);
    loop {
        if tokio::fs::canonicalize(&link).await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(REHOME_REENUM_STEP).await;
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn execute_rebind(_plan: &RehomePlan) {}

#[cfg(target_os = "linux")]
async fn sysfs_write(path: &str, val: &str) -> std::io::Result<()> {
    tokio::fs::write(path, val).await
}
