use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus};

use super::subprocess::{HELPER_OUTPUT_LIMIT, HelperOutput};

pub(super) fn runc_command(runc: &Path, root: &Path) -> Command {
    let mut command = Command::new(runc);
    command.arg("--root").arg(root);
    command
}

pub(super) fn helper_message(operation: &str, output: &HelperOutput) -> String {
    format!(
        "{operation} failed with {}; stdout: {}; stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

pub(super) fn create_failure_message(
    status: ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
    log_path: &Path,
) -> String {
    let log = read_bounded_diagnostic(log_path)
        .unwrap_or_else(|error| format!("<unavailable bounded runc log: {error}>").into_bytes());
    format!(
        "runc create failed with {status}; diagnostic stdout: {}; diagnostic stderr: {}; runc log: {}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr),
        String::from_utf8_lossy(&log)
    )
}

fn read_bounded_diagnostic(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(HELPER_OUTPUT_LIMIT.min(4096));
    match File::open(path) {
        Ok(file) => file
            .take(u64::try_from(HELPER_OUTPUT_LIMIT + 1).expect("diagnostic limit fits u64"))
            .read_to_end(&mut bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(bytes),
        Err(error) => return Err(error),
    };
    if bytes.len() > HELPER_OUTPUT_LIMIT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "runc diagnostic log exceeds 1 MiB",
        ));
    }
    Ok(bytes)
}
