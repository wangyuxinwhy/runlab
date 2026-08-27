#![forbid(unsafe_code)]

mod cli;
mod filesystem;
mod image;
mod run;
mod runtime_config;
mod state;
mod storage;

fn main() {
    let exit = match cli::run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error:#}");
            1
        }
    };
    std::process::exit(exit.into());
}
