//! Guest-local entry point for one persistent project and one command per Workspace.
#![forbid(unsafe_code)]

#[cfg(target_os = "linux")]
mod project;
#[cfg(target_os = "linux")]
mod session;
#[cfg(target_os = "linux")]
mod store;
#[cfg(target_os = "linux")]
mod workspace;

use std::process::ExitCode;

#[cfg(target_os = "linux")]
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    use std::{env, ffi::OsStr};

    let args: Vec<_> = env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [init] if init == OsStr::new("init") => project::check_guest()
            .and_then(|()| project::init())
            .map(|()| 0),
        [call, method, args] if call == OsStr::new("call") => {
            project::check_guest().and_then(|()| store::call(method, args))
        }
        [op] if op == OsStr::new("session") => project::check_guest().and_then(|()| session::run()),
        [op, dir, cmd, rest @ ..] if op == OsStr::new("exec") => {
            project::check_guest().and_then(|()| workspace::isolated(dir, cmd, rest))
        }
        [run, branch, delimiter, command, arguments @ ..]
            if run == OsStr::new("run") && delimiter == OsStr::new("--") =>
        {
            project::check_guest().and_then(|()| workspace::run(branch, command, arguments))
        }
        _ => {
            eprintln!("usage: layerfs-guest init | layerfs-guest run BRANCH -- COMMAND [ARG ...] | layerfs-guest call METHOD JSON | layerfs-guest session");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("layerfs-guest: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("layerfs-guest requires a Linux guest with kernel FUSE");
    ExitCode::FAILURE
}
