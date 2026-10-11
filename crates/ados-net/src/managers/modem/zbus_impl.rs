//! zbus-backed ModemManager1 client. Pure-Rust D-Bus (no dbus-sys / libdbus,
//! no ring); reuses the tokio runtime via zbus's `tokio` feature.

use super::*;
use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as ZValue};
use zbus::{Connection, Proxy};

/// `MM_BEARER_IP_METHOD_STATIC`: the host must apply the address itself.
const IP_METHOD_STATIC: u32 = 2;

/// The static IPv4 settings out of a bearer's `Ip4Config` dict, or `None`
/// when the method is not static or the address is missing.
fn parse_ip4_static(cfg: &HashMap<String, OwnedValue>) -> Option<Ip4Static> {
    let method = cfg.get("method").and_then(|v| u32::try_from(v).ok())?;
    if method != IP_METHOD_STATIC {
        return None;
    }
    let address = cfg
        .get("address")
        .and_then(|v| <&str>::try_from(v).ok())
        .filter(|a| !a.is_empty())?
        .to_string();
    let prefix = cfg
        .get("prefix")
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(32);
    let gateway = cfg
        .get("gateway")
        .and_then(|v| <&str>::try_from(v).ok())
        .filter(|g| !g.is_empty())
        .map(str::to_string);
    Some(Ip4Static {
        address,
        prefix,
        gateway,
    })
}

/// The ObjectManager `GetManagedObjects` reply shape (`a{oa{sa{sv}}}`); we
/// only need the object-path keys.
type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

pub struct ZbusModem;

impl ZbusModem {
    pub fn new() -> Self {
        Self
    }

    async fn connect(&self) -> Result<Connection, String> {
        tokio::time::timeout(DBUS_TIMEOUT, Connection::system())
            .await
            .map_err(|_| "dbus_connect_timeout".to_string())?
            .map_err(|e| e.to_string())
    }

    async fn bearer_proxy<'a>(
        &self,
        conn: &'a Connection,
        bearer: &OwnedObjectPath,
    ) -> Result<Proxy<'a>, String> {
        Proxy::new(
            conn,
            MM_SERVICE,
            bearer.as_str().to_string(),
            "org.freedesktop.ModemManager1.Bearer",
        )
        .await
        .map_err(|e| e.to_string())
    }

    /// First `/Modem/N` object path (skipping `/Bearer/` children).
    async fn first_modem_path(&self, conn: &Connection) -> Result<String, String> {
        let om = Proxy::new(
            conn,
            MM_SERVICE,
            MM_ROOT_PATH,
            "org.freedesktop.DBus.ObjectManager",
        )
        .await
        .map_err(|e| e.to_string())?;

        let managed: ManagedObjects =
            tokio::time::timeout(DBUS_TIMEOUT, om.call("GetManagedObjects", &()))
                .await
                .map_err(|_| "dbus_list_timeout".to_string())?
                .map_err(|e| e.to_string())?;

        let mut paths: Vec<String> = managed
            .keys()
            .map(|p| p.as_str().to_string())
            .filter(|p| p.contains("/ModemManager1/Modem/") && !p.contains("/Bearer/"))
            .collect();
        paths.sort();
        paths
            .into_iter()
            .next()
            .ok_or_else(|| "no_modem".to_string())
    }
}

#[async_trait]
impl ModemDbus for ZbusModem {
    async fn bring_up(&self, apn: &str) -> Result<DbusConnectResult, String> {
        let conn = self.connect().await?;
        let path = self.first_modem_path(&conn).await?;

        // Enable is best-effort (some modems auto-enable); a failure here is
        // logged but not fatal, matching the Python `enable_skipped` path.
        if let Ok(modem) = Proxy::new(
            &conn,
            MM_SERVICE,
            path.clone(),
            "org.freedesktop.ModemManager1.Modem",
        )
        .await
        {
            let _ =
                tokio::time::timeout(DBUS_TIMEOUT, modem.call::<_, _, ()>("Enable", &true)).await;
        }

        let simple = Proxy::new(
            &conn,
            MM_SERVICE,
            path,
            "org.freedesktop.ModemManager1.Modem.Simple",
        )
        .await
        .map_err(|e| e.to_string())?;

        let mut props: HashMap<&str, ZValue> = HashMap::new();
        props.insert("apn", ZValue::from(apn));
        // Connect returns the new bearer's object path: kept so the
        // manager can read the bearer's interface, IPv4 settings and
        // connection state.
        let bearer: OwnedObjectPath =
            tokio::time::timeout(CONNECT_TIMEOUT, simple.call("Connect", &props))
                .await
                .map_err(|_| "dbus_connect_timeout".to_string())?
                .map_err(|e| e.to_string())?;

        let proxy = self.bearer_proxy(&conn, &bearer).await?;
        let iface = tokio::time::timeout(DBUS_TIMEOUT, proxy.get_property::<String>("Interface"))
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let ip4 = tokio::time::timeout(
            DBUS_TIMEOUT,
            proxy.get_property::<HashMap<String, OwnedValue>>("Ip4Config"),
        )
        .await
        .ok()
        .and_then(Result::ok);
        let ip4_static = ip4.as_ref().and_then(parse_ip4_static);
        Ok(DbusConnectResult {
            iface,
            ip: ip4_static
                .as_ref()
                .map(|c| c.address.clone())
                .unwrap_or_default(),
            apn: apn.to_string(),
            bearer: bearer.as_str().to_string(),
            ip4_static,
        })
    }

    async fn bearer_connected(&self, bearer: &str) -> Result<bool, String> {
        let conn = self.connect().await?;
        let path = OwnedObjectPath::try_from(bearer).map_err(|e| e.to_string())?;
        let proxy = self.bearer_proxy(&conn, &path).await?;
        tokio::time::timeout(DBUS_TIMEOUT, proxy.get_property::<bool>("Connected"))
            .await
            .map_err(|_| "dbus_bearer_timeout".to_string())?
            .map_err(|e| e.to_string())
    }

    async fn bring_down(&self) -> Result<(), String> {
        let conn = self.connect().await?;
        let path = self.first_modem_path(&conn).await?;
        let simple = Proxy::new(
            &conn,
            MM_SERVICE,
            path,
            "org.freedesktop.ModemManager1.Modem.Simple",
        )
        .await
        .map_err(|e| e.to_string())?;
        // Disconnect("/") tears down all bearers.
        let bearer = OwnedObjectPath::try_from("/").map_err(|e| e.to_string())?;
        tokio::time::timeout(
            DBUS_TIMEOUT * 2,
            simple.call::<_, _, ()>("Disconnect", &bearer),
        )
        .await
        .map_err(|_| "dbus_disconnect_timeout".to_string())?
        .map_err(|e| e.to_string())
    }

    async fn modem_present(&self) -> Result<bool, String> {
        let conn = self.connect().await?;
        match self.first_modem_path(&conn).await {
            Ok(_) => Ok(true),
            Err(e) if e == "no_modem" => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn imsi(&self) -> Result<Option<String>, String> {
        let conn = self.connect().await?;
        let path = match self.first_modem_path(&conn).await {
            Ok(p) => p,
            Err(e) if e == "no_modem" => return Ok(None),
            Err(e) => return Err(e),
        };
        // The SIM IMSI lives on the 3GPP interface's `Imsi` property.
        let modem3gpp = Proxy::new(
            &conn,
            MM_SERVICE,
            path,
            "org.freedesktop.ModemManager1.Modem.Modem3gpp",
        )
        .await
        .map_err(|e| e.to_string())?;
        match tokio::time::timeout(DBUS_TIMEOUT, modem3gpp.get_property::<String>("Imsi")).await {
            Ok(Ok(imsi)) if !imsi.is_empty() => Ok(Some(imsi)),
            // Property absent / empty (no SIM) → no IMSI, not an error.
            Ok(_) => Ok(None),
            Err(_) => Err("dbus_imsi_timeout".to_string()),
        }
    }
}
