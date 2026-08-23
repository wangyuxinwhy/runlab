//! A termination flag shared between the signal handler and the threads
//! supervising a Run.
//!
//! Registering returns a guard; the flag stays armed until the guard is dropped,
//! which is how a Run keeps responding to interruption right through its
//! terminal write.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result};

#[derive(Debug)]
pub(crate) struct TerminationFlag {
    flag: Arc<AtomicBool>,
    #[cfg(unix)]
    registrations: [signal_hook::SigId; 2],
}

impl TerminationFlag {
    pub(crate) fn register() -> Result<Self> {
        let flag = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        {
            let sigint =
                signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag))
                    .context("failed to register SIGINT handler")?;
            let sigterm = match signal_hook::flag::register(
                signal_hook::consts::SIGTERM,
                Arc::clone(&flag),
            ) {
                Ok(registration) => registration,
                Err(error) => {
                    signal_hook::low_level::unregister(sigint);
                    return Err(error).context("failed to register SIGTERM handler");
                }
            };
            Ok(Self {
                flag,
                registrations: [sigint, sigterm],
            })
        }
        #[cfg(not(unix))]
        Ok(Self { flag })
    }

    pub(crate) fn flag(&self) -> &AtomicBool {
        &self.flag
    }
}

#[cfg(unix)]
impl Drop for TerminationFlag {
    fn drop(&mut self) {
        for registration in self.registrations {
            signal_hook::low_level::unregister(registration);
        }
    }
}
