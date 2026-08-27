mod guest;
#[cfg(target_os = "macos")]
mod host;
#[cfg(target_os = "macos")]
mod transport;

#[cfg(target_os = "linux")]
pub(crate) use guest::guest_handshake;
#[cfg(target_os = "macos")]
pub(crate) use guest::{GuestHandshake, TRANSPORT_VERSION};
#[cfg(target_os = "macos")]
pub(crate) use host::ManagedVm;
#[cfg(target_os = "macos")]
pub(crate) use transport::{ForwardRunStart, ForwardedOutput};
