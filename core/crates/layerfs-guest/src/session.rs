//! The serial channel signals ownership of each request file. Payloads use hostfs.
use crate::{store, Result};
use layerfs_bridge::sdk_values::unhex;
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufWriter, Read, Write},
    os::unix::process::ExitStatusExt,
    process::Command,
};

pub fn run() -> Result<u8> {
    store::with_store(|service, defaults, retain| {
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
                let code = workspace(index, args)?;
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
    })
}

fn workspace(index: u64, args: Value) -> Result<u8> {
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
    let status = Command::new("/bin/busybox")
        .args([
            "unshare",
            "-mipunf",
            "--mount-proc",
            "/init",
            "workspace",
            &branch,
            "--",
        ])
        .args(argv)
        .stdin(File::open(path("stdin"))?)
        .stdout(File::create(path("stdout"))?)
        .stderr(File::create(path("stderr"))?)
        .status()?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().expect("signal exit")) as u8)
}
