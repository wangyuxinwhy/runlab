use anyhow::{Result, bail};

use super::Xattrs;

#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "non-Linux filesystem tests only exercise native ownership"
    )
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum FilesystemOwnership {
    #[default]
    Native,
    SingleId {
        host_uid: u32,
        host_gid: u32,
    },
}

#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "rootless ownership mapping is Linux-only")
)]
impl FilesystemOwnership {
    pub(crate) fn materialized_ids(self, uid: u32, gid: u32) -> Result<(u32, u32)> {
        match self {
            Self::Native => Ok((uid, gid)),
            Self::SingleId { host_uid, host_gid } if uid == 0 && gid == 0 => {
                Ok((host_uid, host_gid))
            }
            Self::SingleId { .. } => {
                bail!("rootless native execution only supports Image filesystem uid=0 and gid=0")
            }
        }
    }

    pub(crate) fn logical_ids(self, uid: u32, gid: u32) -> Result<(u32, u32)> {
        match self {
            Self::Native => Ok((uid, gid)),
            Self::SingleId { host_uid, host_gid } if uid == host_uid && gid == host_gid => {
                Ok((0, 0))
            }
            Self::SingleId { .. } => bail!(
                "rootless filesystem entry ownership is outside the accepted single-ID mapping: host uid={uid}, gid={gid}"
            ),
        }
    }

    pub(crate) fn validate_xattrs(self, xattrs: &Xattrs) -> Result<()> {
        if matches!(self, Self::SingleId { .. })
            && let Some(name) = xattrs.keys().find(|name| !name.starts_with(b"user."))
        {
            bail!(
                "rootless native execution does not support privileged xattr {}",
                String::from_utf8_lossy(name)
            );
        }
        Ok(())
    }

    #[must_use]
    pub(crate) const fn is_single_id(self) -> bool {
        matches!(self, Self::SingleId { .. })
    }
}
