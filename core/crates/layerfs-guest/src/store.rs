//! Guest CLI transport for the original in-process Store dispatchers.
use crate::{project, Result};
use layerfs_bridge::{
    control::{SdkCall, SdkRefusal, SdkValue},
    provision::{StoreManifest, StoreProfile},
    sdk_values::{hex, unhex},
};
use layerfs_history::{BranchId, LayerStackId};
use layerfs_store_service::{
    history::lookup::{failed, invalid, refuse, Answer},
    OpenRefusal, StoreService,
};
use nix::unistd::mkdtemp;
use serde_json::{json, Value};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
    path::Path,
};

pub fn call(method: &OsStr, args: &OsStr) -> Result<u8> {
    let method = method.to_str().ok_or("Store method must be UTF-8")?;
    let args = args.to_str().ok_or("Store arguments must be UTF-8 JSON")?;
    let fields = arguments(serde_json::from_str(args)?)?;
    with_store(|service, defaults, retain| {
        let answer = dispatch(service, defaults, method, fields);
        *retain = uncertain(&answer);
        output(answer, io::stdout().lock())
    })
}

pub fn arguments(args: Value) -> Result<Vec<(String, SdkValue)>> {
    let Value::Object(args) = args else {
        return Err("Store arguments must be a JSON object".into());
    };
    let SdkValue::Map(fields) = sdk_value(Value::Object(args))? else {
        return Err("Store arguments must be a JSON object of method fields".into());
    };
    Ok(fields)
}

pub fn uncertain(answer: &Answer) -> bool {
    answer
        .as_ref()
        .is_err_and(|refusal| refusal.code == "Unknown")
}

pub fn with_store(
    work: impl FnOnce(&StoreService, &mut Defaults<'_>, &mut bool) -> Result<u8>,
) -> Result<u8> {
    let manifest = project::manifest()?;
    if manifest.locator != project::STORE || manifest.profile != StoreProfile::Disposable {
        return Err("project manifest must name the guest's Disposable Store".into());
    }

    let scratch = mkdtemp(
        Path::new(project::PROJECT)
            .join("store-call.XXXXXX")
            .as_path(),
    )?;
    let template = scratch.join("overlay.sqlite");
    let opened = StoreService::open_direct(
        Path::new(project::STORE),
        &manifest.binding,
        manifest.cursor_key,
        &template,
    );
    let service = match opened {
        Ok(service) => service,
        Err(error) => {
            if let Err(cleanup) = fs::remove_dir_all(&scratch) {
                eprintln!("layerfs-guest: cleanup at {}: {cleanup}", scratch.display());
            }
            let refusal = match error {
                OpenRefusal::Unsupported(reason) => refuse("Unsupported", reason.0, SdkValue::Null),
                OpenRefusal::Missing(reason) => refuse("NoStore", reason, SdkValue::Null),
                OpenRefusal::Failed(reason) => failed(reason),
            };
            return output(Err(refusal), io::stdout().lock());
        }
    };

    let mut retained = false;
    let mut defaults = Defaults {
        manifest: &manifest,
        project: None,
        branch: None,
    };
    let outcome = work(&service, &mut defaults, &mut retained);
    retained |= service.retained() != 0;
    let cleanup = service.close().and_then(|()| {
        if !retained {
            fs::remove_file(&template)
                .and_then(|()| fs::remove_dir(&scratch))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    });
    if retained {
        eprintln!("layerfs-guest: overlay retained at {}", scratch.display());
    }
    if let Err(error) = &cleanup {
        eprintln!("layerfs-guest: cleanup at {}: {error}", scratch.display());
    }
    outcome.map(|status| status.max(u8::from(cleanup.is_err())))
}

// Sessions own the disk. The Store API can move heads, but cannot rename a project or Branch.
pub struct Defaults<'a> {
    manifest: &'a StoreManifest,
    project: Option<String>,
    branch: Option<String>,
}

fn name(
    slot: &mut Option<String>,
    load: impl FnOnce() -> std::result::Result<String, SdkRefusal>,
) -> Answer {
    let name = match slot {
        Some(name) => name,
        None => slot.insert(load()?),
    };
    Ok(SdkValue::text(name.as_str()))
}

pub fn dispatch(
    service: &StoreService,
    defaults: &mut Defaults<'_>,
    method: &str,
    mut fields: Vec<(String, SdkValue)>,
) -> Answer {
    if !fields.iter().any(|(name, _)| name == "project") {
        let project = name(&mut defaults.project, || {
            let stack = LayerStackId::from_bytes(defaults.manifest.stack).map_err(failed)?;
            let record = service
                .store
                .published(|history| history.layer_stack(stack))
                .map_err(failed)?
                .ok_or_else(|| failed("the manifest's project is missing"))?;
            Ok(record.name.as_str().to_owned())
        })?;
        fields.push(("project".into(), project));
    }
    // Fork keeps the SDK's default of the published main Layer.
    if (method.starts_with("fs.")
        || method.starts_with("backend.store.fs.")
        || matches!(
            method,
            "branch.list_commits"
                | "branch.merge"
                | "branch.publish"
                | "backend.store.branch.list_commit_storage"
        ))
        && !fields.iter().any(|(name, _)| name == "branch")
    {
        let branch = name(&mut defaults.branch, || {
            let id = BranchId::from_bytes(defaults.manifest.branch).map_err(failed)?;
            let record = service
                .store
                .published(|history| history.branch(id))
                .map_err(failed)?
                .ok_or_else(|| failed("the manifest's Branch is missing"))?;
            if record.name.as_str() == "main" {
                return Err(invalid("the manifest's Branch uses the reserved name main"));
            }
            Ok(record.name.as_str().to_owned())
        })?;
        fields.push(("branch".into(), branch));
    }
    let call = SdkCall {
        method: method.to_owned(),
        args: SdkValue::Map(fields),
    };
    if method.starts_with("backend.store.") {
        layerfs_store_service::operator::dispatch(service, &call)
    } else {
        layerfs_api::call(service, &call)
    }
}

fn sdk_value(value: Value) -> Result<SdkValue> {
    Ok(match value {
        Value::Null => SdkValue::Null,
        Value::Bool(value) => SdkValue::Bool(value),
        Value::Number(value) => SdkValue::Int(
            value
                .as_i64()
                .ok_or("JSON numbers must be signed 64-bit integers")?,
        ),
        Value::String(value) => SdkValue::Text(value),
        Value::Array(items) => {
            SdkValue::List(items.into_iter().map(sdk_value).collect::<Result<_>>()?)
        }
        Value::Object(mut fields) => {
            if let Some(bytes) = fields.remove("$bytes") {
                if !fields.is_empty() {
                    return Err("a $bytes object must have exactly one field".into());
                }
                let text = bytes
                    .as_str()
                    .filter(|text| text.bytes().all(|byte| byte.is_ascii_hexdigit()))
                    .ok_or("$bytes must be a hex string")?;
                SdkValue::Bytes(unhex(text).ok_or("$bytes must contain pairs of hex digits")?)
            } else {
                SdkValue::Map(
                    fields
                        .into_iter()
                        .map(|(name, value)| Ok((name, sdk_value(value)?)))
                        .collect::<Result<_>>()?,
                )
            }
        }
    })
}

fn json_value(value: SdkValue) -> Value {
    match value {
        SdkValue::Null => Value::Null,
        SdkValue::Bool(value) => Value::Bool(value),
        SdkValue::Int(value) => Value::from(value),
        SdkValue::Text(value) => Value::String(value),
        SdkValue::Bytes(value) => json!({ "$bytes": hex(&value) }),
        SdkValue::List(items) => Value::Array(items.into_iter().map(json_value).collect()),
        SdkValue::Map(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, value)| (name, json_value(value)))
                .collect(),
        ),
    }
}

pub fn output(answer: Answer, mut stdout: impl Write) -> Result<u8> {
    let (value, status) = match answer {
        Ok(value) => (json_value(value), 0),
        Err(SdkRefusal {
            code,
            message,
            detail,
        }) => (
            json!({ "refused": { "code": code, "message": message, "detail": json_value(detail) } }),
            1,
        ),
    };
    serde_json::to_writer(&mut stdout, &value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(status)
}
