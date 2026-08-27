use std::collections::BTreeMap;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result as AnyResult, bail};
use rustix::process::{Pid, Signal, kill_process_group};

pub(in crate::native) const SUPERVISOR_REAP_LIMIT: Duration = Duration::from_millis(250);
const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone)]
pub(in crate::native) struct InvocationSupervisor {
    state: Arc<Mutex<SupervisorState>>,
}

struct SupervisorState {
    next_id: u64,
    children: BTreeMap<u64, SupervisedChild>,
}

struct SupervisedChild {
    child: Child,
    pid: Pid,
    reaped: bool,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::native) struct SupervisorToken(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::native) enum SupervisorLifecycle {
    Reaped,
    Active { children: usize },
}

impl InvocationSupervisor {
    pub(in crate::native) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(SupervisorState {
                next_id: 0,
                children: BTreeMap::new(),
            })),
        }
    }

    pub(in crate::native) fn spawn(&self, command: &mut Command) -> AnyResult<SupervisorToken> {
        command.process_group(0);
        let child = command.spawn()?;
        let pid = Pid::from_raw(child.id().cast_signed()).expect("std::process::Child has a PID");
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1);
        state.children.insert(
            id,
            SupervisedChild {
                child,
                pid,
                reaped: false,
            },
        );
        Ok(SupervisorToken(id))
    }

    pub(in crate::native) fn with_child<T>(
        &self,
        token: SupervisorToken,
        operation: impl FnOnce(&mut Child) -> std::io::Result<T>,
    ) -> std::io::Result<T> {
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state.children.get_mut(&token.0).ok_or_else(unknown_token)?;
        operation(&mut child.child)
    }

    pub(in crate::native) fn try_wait(
        &self,
        token: SupervisorToken,
    ) -> std::io::Result<Option<ExitStatus>> {
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state.children.get_mut(&token.0).ok_or_else(unknown_token)?;
        let status = child.child.try_wait()?;
        child.reaped |= status.is_some();
        Ok(status)
    }

    pub(in crate::native) fn kill(&self, token: SupervisorToken) -> AnyResult<()> {
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        let child = state
            .children
            .get_mut(&token.0)
            .context("unknown supervisor token")?;
        if child.reaped {
            return Ok(());
        }
        match kill_process_group(child.pid, Signal::KILL) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::SRCH) => {
                child.reaped |= child.child.try_wait()?.is_some();
                if child.reaped {
                    Ok(())
                } else {
                    bail!("helper process group disappeared before its leader was reaped")
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(in crate::native) fn release_reaped(&self, token: SupervisorToken) -> AnyResult<()> {
        let mut state = self.state.lock().expect("supervisor mutex poisoned");
        if !state
            .children
            .get(&token.0)
            .context("unknown supervisor token")?
            .reaped
        {
            bail!("cannot release an unreaped helper")
        }
        state.children.remove(&token.0);
        Ok(())
    }

    pub(in crate::native) fn finalize(&self, deadline: Instant) -> AnyResult<()> {
        loop {
            let tokens = self.tokens();
            if tokens.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "helper cleanup deadline exceeded with {} child processes",
                    tokens.len()
                );
            }
            for token in tokens {
                if self.try_wait(token)?.is_some() {
                    self.release_reaped(token)?;
                } else {
                    self.kill(token)?;
                }
            }
            thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    pub(in crate::native) fn lifecycle(&self) -> SupervisorLifecycle {
        let children = self
            .state
            .lock()
            .expect("supervisor mutex poisoned")
            .children
            .len();
        if children == 0 {
            SupervisorLifecycle::Reaped
        } else {
            SupervisorLifecycle::Active { children }
        }
    }

    fn tokens(&self) -> Vec<SupervisorToken> {
        self.state
            .lock()
            .expect("supervisor mutex poisoned")
            .children
            .keys()
            .copied()
            .map(SupervisorToken)
            .collect()
    }
}

impl Drop for InvocationSupervisor {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) == 1 {
            for token in self.tokens() {
                let _ = self.kill(token);
            }
        }
    }
}

fn unknown_token() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, "unknown supervisor token")
}
