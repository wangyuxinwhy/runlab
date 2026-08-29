#![forbid(unsafe_code)]

mod cli;
mod docs;
mod error;
// macOS reuses these modules' validated CLI value types while their State
// implementation runs only in the managed Linux guest.
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod filesystem;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod image;
mod managed_vm;
mod metadata;
mod observation;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod public_schema;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod query;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod run;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod run_deletion;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod run_record;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod runtime_config;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod state;
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod storage;
#[cfg(target_os = "linux")]
mod storage_management;

fn main() {
    let exit = match cli::run() {
        Ok(code) => code,
        Err(error) => {
            error::emit(&error);
            1
        }
    };
    std::process::exit(exit.into());
}
