use std::path::Path;
use std::time::{Duration, Instant};

use super::budget::OperationBudget;
use super::prepare::{PreparedInvocation, PreparedProgram};
use super::program::RootfsStability;
use super::runc::{helper_message, runc_command};
use super::subprocess::{InvocationSupervisor, run_helper_until};

pub(super) struct RuntimeCleanup<'a> {
    pub(super) runc: &'a Path,
    pub(super) runtime_root: &'a Path,
    pub(super) program: &'a PreparedProgram,
    pub(super) supervisor: &'a InvocationSupervisor,
    pub(super) runtime_attempted: bool,
    pub(super) removal_timeout: Duration,
    pub(super) supervisor_deadline: Instant,
    pub(super) egress: &'a mut Option<super::network::EgressNetwork>,
}

#[derive(Default)]
pub(super) struct RuntimeCleanupReport {
    pub(super) runtime_deleted: bool,
    pub(super) rootfs_stability: RootfsStability,
    pub(super) supervisor_unreaped: bool,
    pub(super) issues: Vec<CleanupIssue>,
}

pub(super) struct CleanupIssue {
    message: String,
    code: Option<i64>,
}

impl CleanupIssue {
    fn new(message: impl Into<String>, code: Option<i64>) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }

    pub(super) fn into_parts(self) -> (String, Option<i64>) {
        (self.message, self.code)
    }
}

pub(super) fn cleanup_runtime(cleanup: RuntimeCleanup<'_>) -> RuntimeCleanupReport {
    cleanup.run()
}

impl RuntimeCleanup<'_> {
    fn run(self) -> RuntimeCleanupReport {
        let mut report = RuntimeCleanupReport {
            runtime_deleted: !self.runtime_attempted,
            ..RuntimeCleanupReport::default()
        };
        let deadline = Instant::now()
            .checked_add(self.removal_timeout)
            .expect("validated OperationTimeouts fit Instant")
            .min(self.supervisor_deadline);
        if let Some(network) = self.egress {
            for issue in network.cleanup(self.supervisor, deadline) {
                report.supervisor_unreaped |= !issue.supervisor_reaped;
                report.issues.push(CleanupIssue::new(issue.message, None));
            }
        }
        if self.runtime_attempted {
            self.delete_runtime(&mut report, deadline);
        }
        if !report.runtime_deleted || report.supervisor_unreaped {
            return report;
        }
        if let Err(error) = self.program.rootfs.ensure_no_mounts() {
            report.issues.push(CleanupIssue::new(
                format!("mounts remain after runc deletion: {error:#}"),
                None,
            ));
            return report;
        }
        self.remove_mount_artifacts(&mut report, deadline);
        report
    }

    fn delete_runtime(&self, report: &mut RuntimeCleanupReport, deadline: Instant) {
        if Instant::now() >= deadline {
            report.issues.push(CleanupIssue::new(
                "runtime filesystem removal deadline exceeded before runc deletion",
                None,
            ));
            return;
        }
        match run_helper_until(
            self.supervisor,
            runc_command(self.runc, self.runtime_root)
                .arg("delete")
                .arg("--force")
                .arg(&self.program.runtime_id),
            deadline,
            None,
        ) {
            Ok(output) if output.status.success() => report.runtime_deleted = true,
            Ok(output) => report.issues.push(CleanupIssue::new(
                helper_message("runc delete --force", &output),
                output.status.code().map(i64::from),
            )),
            Err(error) => {
                report.supervisor_unreaped = !error.supervisor_reaped;
                report.issues.push(CleanupIssue::new(
                    format!("runc delete --force: {error:#}"),
                    None,
                ));
            }
        }
    }

    fn remove_mount_artifacts(&self, report: &mut RuntimeCleanupReport, deadline: Instant) {
        let mut stable = true;
        for relative in self.program.artifacts.iter().rev() {
            if Instant::now() >= deadline {
                report.issues.push(CleanupIssue::new(
                    "runtime filesystem removal deadline exceeded while removing mount artifacts",
                    None,
                ));
                return;
            }
            if let Err(error) = self.program.rootfs.remove_mount_artifact(relative) {
                stable = false;
                report.issues.push(CleanupIssue::new(
                    format!(
                        "runtime-created mount artifact {} left the rootfs unstable: {error:#}",
                        relative.display()
                    ),
                    error
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        .map(i64::from),
                ));
            }
        }
        if stable {
            report.rootfs_stability = RootfsStability::Stable;
        }
    }
}

pub(super) fn cleanup_invocation(
    prepared: &mut PreparedInvocation,
    all_writers_stopped: bool,
    budget: OperationBudget,
) -> Option<CleanupIssue> {
    if let Err(error) = budget.check() {
        let preserved = preserve_workspace(prepared);
        return Some(CleanupIssue::new(
            preserved.map_or_else(
                || format!("cleanup deadline exceeded before workspace removal: {error:#}"),
                |path| {
                    format!(
                        "cleanup deadline exceeded before workspace removal; preserved {}: {error:#}",
                        path.display()
                    )
                },
            ),
            None,
        ));
    }
    if !all_writers_stopped
        || prepared
            .programs
            .values()
            .any(|program| program.rootfs.ensure_no_mounts().is_err())
    {
        let path = preserve_workspace(prepared);
        return path.map(|path| {
            CleanupIssue::new(
                format!(
                    "preserved workspace {} because a writer may still be active or a residual mount could not be excluded",
                    path.display()
                ),
                None,
            )
        });
    }
    prepared.workspace.take().and_then(|workspace| {
        let result = std::fs::remove_dir_all(&workspace);
        match (result, budget.check()) {
            (Err(error), _) => Some(CleanupIssue::new(
                format!("failed to remove invocation workspace: {error}"),
                error.raw_os_error().map(i64::from),
            )),
            (Ok(()), Err(error)) => Some(CleanupIssue::new(
                format!("cleanup deadline exceeded while removing workspace: {error:#}"),
                None,
            )),
            (Ok(()), Ok(())) => None,
        }
    })
}

fn preserve_workspace(prepared: &mut PreparedInvocation) -> Option<std::path::PathBuf> {
    prepared.workspace.take()
}
