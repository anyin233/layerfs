//! The serial channel signals ownership of each request file. Payloads use hostfs.
use crate::{store, workspace::Runtime, Result};
use layerfs_bridge::sdk_values::unhex;
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Read, Write},
    process::Command,
};

pub fn run() -> Result<u8> {
    let mut runtime = None;
    let outcome = store::with_store(
        crate::workspace::CACHE_BYTES,
        |service, defaults, retain| {
            let mut channel = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/ttyS0")?;
            channel.write_all(b"R")?;
            for index in 0u64.. {
                let mut op = [0];
                if channel.read(&mut op)? == 0 || op == *b"Q" {
                    return Ok(0);
                }
                if op != *b"C" {
                    return Err("invalid session command".into());
                }
                let (method, args): (String, Value) =
                    serde_json::from_slice(&std::fs::read(format!("/request/{index}.in"))?)?;
                if method == "workspace.run" {
                    let active = match &mut runtime {
                        Some(active) => active,
                        None => {
                            if !Command::new("/init").arg("devices").status()?.success() {
                                return Err("could not prepare Workspace devices".into());
                            }
                            runtime.insert(Runtime::open(Some(service.store.clone()))?)
                        }
                    };
                    let code = workspace(active, index, args)?;
                    fs::write(format!("/request/{index}.out"), code.to_string())?;
                    if code != 0 {
                        channel.write_all(b"F")?;
                        return Ok(code);
                    }
                    channel.write_all(b"W")?;
                    continue;
                }
                let answer = store::dispatch(service, defaults, &method, store::arguments(args)?);
                *retain = store::uncertain(&answer) || service.retained() != 0;
                let reply = BufWriter::new(File::create(format!("/request/{index}.out"))?);
                let code = store::output(answer, reply)?;
                if code != 0 || *retain {
                    channel.write_all(b"E")?;
                    return Ok(1);
                }
                channel.write_all(b"D")?;
            }
            unreachable!()
        },
    );
    match runtime {
        Some(active) => active.finish(outcome),
        None => outcome,
    }
}

fn workspace(runtime: &mut Runtime, index: u64, args: Value) -> Result<u8> {
    let Value::Object(mut fields) = args else {
        return Err("workspace.run expects an object".into());
    };
    let argv: Vec<String> = serde_json::from_value(fields.remove("argv").ok_or("missing argv")?)?;
    if argv.first().is_none_or(String::is_empty) {
        return Err("workspace.run needs a command".into());
    }
    let branch = match fields.remove("branch") {
        Some(value) => serde_json::from_value::<String>(value)?,
        None => "work".to_owned(),
    };
    let input = match fields.remove("stdin") {
        None => Vec::new(),
        Some(Value::String(text)) => text.into_bytes(),
        Some(Value::Object(obj)) if obj.len() == 1 => obj
            .get("$bytes")
            .and_then(Value::as_str)
            .filter(|s| s.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .and_then(unhex)
            .ok_or("stdin $bytes must be hexadecimal")?,
        _ => return Err("stdin must be text or a $bytes object".into()),
    };
    if !fields.is_empty() {
        return Err("unknown workspace.run argument".into());
    }
    let path = |name: &str| format!("/request/{index}.{name}");
    fs::write(path("stdin"), input)?;
    let stdout = File::create(path("stdout"))?;
    let mut stderr = File::create(path("stderr"))?;
    let outcome = runtime.command(branch.as_ref(), |directory| {
        Ok(Command::new("/bin/busybox")
            .args([
                "unshare",
                "-mipunf",
                "--mount-proc",
                "/bin/layerfs-guest",
                "exec",
                directory,
            ])
            .args(argv)
            .current_dir(directory)
            .stdin(File::open(path("stdin"))?)
            .stdout(stdout)
            .stderr(stderr.try_clone()?)
            .status()?)
    });
    match outcome {
        Ok(code) => Ok(code),
        Err(error) => {
            writeln!(stderr, "layerfs-guest: {error}")?;
            Ok(1)
        }
    }
}
