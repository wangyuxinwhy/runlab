use serde::{Deserialize, Serialize};

pub(crate) const TRANSPORT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct GuestHandshake {
    pub(crate) schema_version: u32,
    pub(crate) transport_version: u32,
    pub(crate) runlab_version: String,
    pub(crate) os: String,
    pub(crate) architecture: String,
    pub(crate) capabilities: Vec<String>,
}

#[cfg(target_os = "linux")]
pub(crate) fn guest_handshake() -> GuestHandshake {
    GuestHandshake {
        schema_version: 1,
        transport_version: TRANSPORT_VERSION,
        runlab_version: env!("CARGO_PKG_VERSION").to_owned(),
        os: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
        capabilities: vec!["native-engine".to_owned()],
    }
}
