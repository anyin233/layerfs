//! One mounted task. The worker is PID 1, so its descendants exit before publication.
use crate::{workspace, Result};
use nix::{
    poll::{poll, PollFd, PollFlags, PollTimeout},
    sys::signal::{killpg, Signal},
    unistd::Pid,
};
use serde_json::Value;
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::net::UnixStream,
    os::{
        fd::{AsFd, AsRawFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
};

pub const DISCARD: u8 = 73;

pub fn run(branch: &OsStr) -> Result<u8> {
    let mut runtime = workspace::Runtime::open(None)?;
    let outcome = runtime.command(branch, |directory| {
        Ok(Command::new("/bin/busybox")
            .args([
                "unshare",
                "-mipunf",
                "--mount-proc",
                "/bin/layerfs-guest",
                "task-worker",
            ])
            .current_dir(directory)
            .status()?)
    });
    runtime.finish_task(outcome)
}

pub fn worker() -> Result<u8> {
    let requests = File::open("/request")?;
    let root = PathBuf::from(format!("/proc/self/fd/{}", requests.as_raw_fd()));
    let mut serial = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/ttyS0")?;
    let cwd = Path::new("/workspace");
    workspace::prepare_namespace(cwd)?;
    serial.write_all(b"R")?;
    for index in 0u64.. {
        let mut cmd = [0];
        loop {
            if serial.read(&mut cmd)? == 0 || cmd == *b"Q" {
                return Ok(DISCARD);
            }
            if cmd != *b"A" {
                break;
            }
        }
        if cmd != *b"C" {
            return Err("invalid task request".into());
        }
        let path = |suffix| root.join(format!("{index}.{suffix}"));
        let (method, args): (String, Value) = serde_json::from_slice(&fs::read(path("in"))?)?;
        match method.as_str() {
            "commit" | "discard" if args == serde_json::json!({}) => {
                serial.write_all(b"T")?;
                return Ok(if method == "commit" { 0 } else { DISCARD });
            }
            "exec" => (),
            _ => return Err("expected exec, commit or discard".into()),
        }
        let Value::Object(mut fields) = args else {
            return Err("expected exec arguments".into());
        };
        let argv: Vec<String> =
            serde_json::from_value(fields.remove("argv").ok_or("missing argv")?)?;
        let (program, args) = argv.split_first().ok_or("empty argv")?;
        let input: String = serde_json::from_value(
            fields
                .remove("stdin")
                .unwrap_or(Value::String(String::new())),
        )?;
        let stream = fields
            .remove("stream")
            .map(serde_json::from_value::<bool>)
            .transpose()?
            .unwrap_or(false);
        if !fields.is_empty() {
            return Err("unknown exec arguments".into());
        }
        fs::write(path("stdin"), input)?;
        let mut stderr = File::create(path("stderr"))?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", "/bin:/usr/bin")
            .env("HOME", "/tmp")
            .env("PWD", cwd)
            .uid(1000)
            .gid(1000)
            .process_group(0)
            .stdin(File::open(path("stdin"))?)
            .stdout(File::create(path("stdout"))?)
            .stderr(stderr.try_clone()?);
        let output = if stream {
            let (reader, writer) = UnixStream::pair()?;
            reader.set_nonblocking(true)?;
            command
                .stdout(OwnedFd::from(writer.try_clone()?))
                .stderr(OwnedFd::from(writer));
            Some(reader)
        } else {
            None
        };
        let spawned = command.spawn();
        drop(command);
        let status = match spawned {
            Ok(child) => Ok(supervise(child, &mut serial, output, &path("chunk"))?),
            Err(error) => Err(error),
        };
        let code = match status {
            Ok(status) => status
                .code()
                .unwrap_or_else(|| 128 + status.signal().expect("signal exit")),
            Err(e) => {
                writeln!(stderr, "{program}: {e}")?;
                if e.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                }
            }
        };
        fs::write(path("out"), code.to_string())?;
        serial.write_all(b"W")?;
        // PID 1 adopts orphaned helpers. Reap those already finished between commands.
        while matches!(
            nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(-1),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG)
            ),
            Ok(nix::sys::wait::WaitStatus::Exited(..) | nix::sys::wait::WaitStatus::Signaled(..))
        ) {}
    }
    unreachable!()
}

// The task stays alive across tool cancellation. A disconnected owner ends the
// worker; leaving its PID namespace terminates remaining task processes.
fn supervise(
    mut child: Child,
    serial: &mut File,
    mut output: Option<UnixStream>,
    chunk: &Path,
) -> Result<ExitStatus> {
    let group = Pid::from_raw(child.id() as i32);
    let (done, notify) = UnixStream::pair()?;
    std::thread::scope(|scope| {
        let wait = scope.spawn(move || {
            let status = child.wait();
            drop(notify);
            status
        });
        let monitor = (|| -> Result<()> {
            let mut bytes = [0; 16384];
            loop {
                let mut fds = vec![
                    PollFd::new(done.as_fd(), PollFlags::POLLIN),
                    PollFd::new(serial.as_fd(), PollFlags::POLLIN),
                ];
                if let Some(pipe) = &output {
                    fds.push(PollFd::new(pipe.as_fd(), PollFlags::POLLIN));
                }
                poll(&mut fds[..], PollTimeout::NONE)?;
                let ready = |i: usize| !fds[i].revents().unwrap_or(PollFlags::empty()).is_empty();
                let finished = ready(0);
                let readable = output.is_some() && ready(2);
                if ready(1) {
                    let mut signal = [0];
                    serial.read_exact(&mut signal)?;
                    if signal != *b"A" {
                        return Err("unexpected task signal".into());
                    }
                    let _ = killpg(group, Signal::SIGKILL);
                }
                if finished {
                    // Keep bytes queued by the foreground command. A background writer
                    // inheriting its output cannot extend the lifetime of this call.
                    if let Some(pipe) = &output {
                        pipe.shutdown(std::net::Shutdown::Read)?;
                    }
                }
                if readable {
                    let count = output.as_mut().unwrap().read(&mut bytes)?;
                    if count == 0 {
                        output = None;
                    } else {
                        fs::write(chunk, &bytes[..count])?;
                        serial.write_all(b"S")?;
                        loop {
                            let mut ack = [0];
                            serial.read_exact(&mut ack)?;
                            if ack == *b"Y" {
                                break;
                            }
                            if ack != *b"A" {
                                return Err("invalid task chunk acknowledgement".into());
                            }
                            let _ = killpg(group, Signal::SIGKILL);
                        }
                        continue;
                    }
                }
                if finished {
                    return Ok(());
                }
            }
        })();
        if monitor.is_err() {
            let _ = killpg(group, Signal::SIGKILL);
        }
        let status = wait.join().expect("child waiter panicked")?;
        monitor?;
        Ok(status)
    })
}
