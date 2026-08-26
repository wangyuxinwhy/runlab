use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use super::budget::OperationBudget;
use super::prepare::{PreparedInvocation, PreparedProgram};
use super::program::RootfsStability;
use super::runc::{helper_message, runc_command};
use super::subprocess::{InvocationSupervisor, run_helper_until};

#[derive(Clone, Copy)]
pub(super) struct RuntimeCleanup<'a> {
    pub(super) runc: &'a Path,
    pub(super) runtime_root: &'a Path,
    pub(super) program: &'a PreparedProgram,
    pub(super) supervisor: &'a InvocationSupervisor,
    pub(super) runtime_attempted: bool,
    pub(super) observed_cgroup: Option<&'a Path>,
    pub(super) removal_timeout: Duration,
    pub(super) supervisor_deadline: Instant,
}

#[derive(Default)]
pub(super) struct RuntimeCleanupReport {
    pub(super) runtime_deleted: bool,
    pub(super) cgroup_processes: CgroupProcessProof,
    pub(super) rootfs_stability: RootfsStability,
    pub(super) supervisor_unreaped: bool,
    pub(super) issues: Vec<CleanupIssue>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum CgroupProcessProof {
    #[default]
    Unproved,
    Absent,
}

struct CgroupCleanupReport {
    processes_absent: bool,
    issue: Option<CleanupIssue>,
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
        let mut report = RuntimeCleanupReport::default();
        let deadline = Instant::now()
            .checked_add(self.removal_timeout)
            .expect("validated OperationTimeouts fit Instant")
            .min(self.supervisor_deadline);
        if deadline_expired(&mut report, deadline, "before runc deletion") {
            return report;
        }
        self.delete_runtime(&mut report, deadline);
        if deadline_expired(&mut report, deadline, "after runc deletion") {
            return report;
        }
        self.remove_cgroups(&mut report, deadline);
        if deadline_expired(&mut report, deadline, "after cgroup removal") {
            return report;
        }
        if !self.prove_mounts_removed(&mut report) {
            return report;
        }
        if deadline_expired(&mut report, deadline, "after mount verification") {
            return report;
        }
        self.remove_mount_artifacts(&mut report, deadline);
        report
    }

    fn delete_runtime(&self, report: &mut RuntimeCleanupReport, deadline: Instant) {
        if !self.runtime_attempted {
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
                report.supervisor_unreaped |= !error.supervisor_reaped;
                report.issues.push(CleanupIssue::new(
                    format!("runc delete --force: {error:#}"),
                    None,
                ));
            }
        }
    }

    fn remove_cgroups(&self, report: &mut RuntimeCleanupReport, deadline: Instant) {
        let mut cgroups = BTreeSet::from([self.program.expected_cgroup_path.clone()]);
        if let Some(path) = self.observed_cgroup {
            cgroups.insert(path.to_path_buf());
        }
        report.cgroup_processes = CgroupProcessProof::Absent;
        for cgroup in cgroups {
            let cgroup_report = remove_owned_cgroup(&cgroup, &self.program.runtime_id, deadline);
            if !cgroup_report.processes_absent {
                report.cgroup_processes = CgroupProcessProof::Unproved;
            }
            if let Some(issue) = cgroup_report.issue {
                report.issues.push(issue);
            }
        }
    }

    fn prove_mounts_removed(&self, report: &mut RuntimeCleanupReport) -> bool {
        if let Err(error) = self.program.rootfs.ensure_no_mounts() {
            report.issues.push(CleanupIssue::new(
                format!("mounts remain after runc deletion: {error:#}"),
                None,
            ));
            return false;
        }
        true
    }

    fn remove_mount_artifacts(&self, report: &mut RuntimeCleanupReport, deadline: Instant) {
        let mut rootfs_stable = true;
        for relative in self.program.artifacts.iter().rev() {
            if deadline_expired(
                report,
                deadline,
                "while removing runtime-created mount artifacts",
            ) {
                return;
            }
            if let Err(error) = self.program.rootfs.remove_mount_artifact(relative) {
                rootfs_stable = false;
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
        if rootfs_stable {
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
        if let Some(workspace) = prepared.workspace.take() {
            let _preserved = workspace.keep();
        }
        return Some(CleanupIssue::new(
            format!("cleanup deadline exceeded before workspace removal: {error:#}"),
            None,
        ));
    }
    if !all_writers_stopped
        || prepared
            .programs
            .values()
            .any(|program| program.rootfs.ensure_no_mounts().is_err())
    {
        if let Some(workspace) = prepared.workspace.take() {
            let preserved = workspace.keep();
            return Some(CleanupIssue::new(
                format!(
                    "preserved workspace {} because a writer may still be active or a residual mount could not be excluded",
                    preserved.display()
                ),
                None,
            ));
        }
        return None;
    }
    prepared.workspace.take().and_then(|workspace| {
        let result = workspace.close();
        if let Err(error) = budget.check() {
            return Some(CleanupIssue::new(
                format!("cleanup deadline exceeded while removing workspace: {error:#}"),
                None,
            ));
        }
        result.err().map(|error| {
            CleanupIssue::new(
                format!("failed to remove invocation workspace: {error}"),
                error.raw_os_error().map(i64::from),
            )
        })
    })
}

fn record_deadline(report: &mut RuntimeCleanupReport, phase: &str) {
    report.issues.push(CleanupIssue::new(
        format!("runtime filesystem removal deadline exceeded {phase}"),
        None,
    ));
}

fn deadline_expired(report: &mut RuntimeCleanupReport, deadline: Instant, phase: &str) -> bool {
    if Instant::now() < deadline {
        return false;
    }
    record_deadline(report, phase);
    true
}

fn remove_owned_cgroup(path: &Path, runtime_id: &str, deadline: Instant) -> CgroupCleanupReport {
    remove_owned_cgroup_beneath(Path::new("/sys/fs/cgroup"), path, runtime_id, deadline)
}

fn remove_owned_cgroup_beneath(
    cgroup_root: &Path,
    path: &Path,
    runtime_id: &str,
    deadline: Instant,
) -> CgroupCleanupReport {
    if Instant::now() >= deadline {
        return unproved_cgroup(
            "runtime filesystem removal deadline exceeded before cgroup cleanup".to_owned(),
            None,
        );
    }
    if !path.starts_with(cgroup_root)
        || path.file_name().and_then(|name| name.to_str()) != Some(runtime_id)
    {
        return unproved_cgroup(
            format!("refusing to remove a cgroup not owned by runtime id {runtime_id}"),
            None,
        );
    }
    match prove_owned_cgroup_empty(path, deadline) {
        Ok(false) => {
            return CgroupCleanupReport {
                processes_absent: true,
                issue: None,
            };
        }
        Ok(true) => {}
        Err(issue) => {
            return CgroupCleanupReport {
                processes_absent: false,
                issue: Some(issue),
            };
        }
    }
    if Instant::now() >= deadline {
        return CgroupCleanupReport {
            processes_absent: true,
            issue: Some(CleanupIssue::new(
                "runtime filesystem removal deadline exceeded before empty cgroup removal",
                None,
            )),
        };
    }
    let issue = fs::remove_dir(path).err().map(|error| {
        CleanupIssue::new(
            format!(
                "failed to remove proved-empty Engine-owned cgroup {}: {error}",
                path.display()
            ),
            error.raw_os_error().map(i64::from),
        )
    });
    CgroupCleanupReport {
        processes_absent: true,
        issue,
    }
}

fn unproved_cgroup(message: String, code: Option<i64>) -> CgroupCleanupReport {
    CgroupCleanupReport {
        processes_absent: false,
        issue: Some(CleanupIssue::new(message, code)),
    }
}

/// Returns `Ok(false)` for an absent cgroup and `Ok(true)` only after proving
/// the present cgroup and every child-cgroup slot contain no process writers.
fn prove_owned_cgroup_empty(path: &Path, deadline: Instant) -> Result<bool, CleanupIssue> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(CleanupIssue::new(
                format!(
                    "failed to inspect Engine-owned cgroup {}: {error}",
                    path.display()
                ),
                error.raw_os_error().map(i64::from),
            ));
        }
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CleanupIssue::new(
            format!("Engine-owned cgroup {} is not a directory", path.display()),
            None,
        ));
    }
    let processes = match fs::read_to_string(path.join("cgroup.procs")) {
        Ok(processes) => processes,
        Err(error) => {
            return Err(CleanupIssue::new(
                format!(
                    "failed to inspect processes in Engine-owned cgroup {}: {error}",
                    path.display()
                ),
                error.raw_os_error().map(i64::from),
            ));
        }
    };
    if !processes.trim().is_empty() {
        return Err(CleanupIssue::new(
            format!(
                "Engine-owned cgroup {} still contains processes after runc deletion",
                path.display()
            ),
            None,
        ));
    }
    prove_no_child_cgroups(path, deadline)?;
    Ok(true)
}

fn prove_no_child_cgroups(path: &Path, deadline: Instant) -> Result<(), CleanupIssue> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            return Err(CleanupIssue::new(
                format!(
                    "failed to inspect Engine-owned cgroup {}: {error}",
                    path.display()
                ),
                error.raw_os_error().map(i64::from),
            ));
        }
    };
    for entry in entries {
        if Instant::now() >= deadline {
            return Err(CleanupIssue::new(
                "runtime filesystem removal deadline exceeded during cgroup inspection".to_owned(),
                None,
            ));
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return Err(CleanupIssue::new(
                    format!(
                        "failed to inspect an entry in Engine-owned cgroup {}: {error}",
                        path.display()
                    ),
                    error.raw_os_error().map(i64::from),
                ));
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                return Err(CleanupIssue::new(
                    format!(
                        "failed to inspect Engine-owned cgroup entry {}: {error}",
                        entry.path().display()
                    ),
                    error.raw_os_error().map(i64::from),
                ));
            }
        };
        if file_type.is_dir() {
            return Err(CleanupIssue::new(
                format!(
                    "Engine-owned cgroup still contains child cgroup {} after runc deletion",
                    entry.path().display()
                ),
                None,
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proved_empty_cgroup_removal_failure_is_an_ordinary_issue() {
        let workspace = tempfile::tempdir().expect("cgroup fixture");
        let runtime_id = "owned-test-cgroup";
        let path = workspace.path().join(runtime_id);
        fs::create_dir(&path).expect("cgroup directory");
        fs::write(path.join("cgroup.procs"), b"").expect("empty cgroup.procs");

        let report = remove_owned_cgroup_beneath(
            workspace.path(),
            &path,
            runtime_id,
            Instant::now() + std::time::Duration::from_secs(1),
        );
        assert!(report.processes_absent);
        let issue = report.issue.expect("ordinary removal issue");
        assert!(issue.message.contains("proved-empty"), "{}", issue.message);
        assert!(
            path.exists(),
            "fixture forces remove_dir to fail as nonempty"
        );
    }
}
