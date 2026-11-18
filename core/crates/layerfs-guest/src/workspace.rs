//! Runs a command against an attached kernel Workspace with local control.
use crate::{project, Result};
use layerfs_bridge::{
    control::{CommitControl, Reply, Request, UnmountControl, WorkspaceToken},
    provision::StoreProfile,
};
use layerfs_daemon::{
    bootstrap::open_store,
    control::{KernelConfig, KernelServing, Service},
    Owner, OwnerConfig,
};
use layerfs_history::{BranchId, HistoryName, LayerStackId, WorkspaceId};
use layerfs_overlay::ProfileConfig;
use layerfs_storage::ReservationBlocks;
use nix::{
    mount::{mount, MsFlags},
    sched::{unshare, CloneFlags},
    unistd::mkdtemp,
};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, Permissions},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::Command,
    sync::Arc,
    time::Duration,
};

const COMMAND_UID: u32 = 1000;
const COMMAND_GID: u32 = 1000;

pub fn run(branch: &OsStr, command: &OsStr, args: &[OsString]) -> Result<u8> {
    let manifest = project::manifest()?;
    if manifest.locator != project::STORE || manifest.profile != StoreProfile::Disposable {
        return Err("project manifest must name the guest's Disposable Store".into());
    }
    let name = HistoryName::new(branch.to_str().ok_or("branch name must be UTF-8")?)?;
    let branch = BranchId::named(LayerStackId::from_bytes(manifest.stack)?, &name);
    let workspace =
        WorkspaceId::from_authority(project::random_bytes(&mut File::open("/dev/urandom")?)?)?;

    let device =
        fs::symlink_metadata("/dev/fuse").map_err(|error| format!("/dev/fuse: {error}"))?;
    if !device.file_type().is_char_device() || device.uid() != 0 || device.mode() & 0o077 != 0 {
        return Err("/dev/fuse must be a private root-owned character device (mode 0600)".into());
    }
    unshare(CloneFlags::CLONE_NEWNS)
        .map_err(|error| format!("private mount namespace: {error}"))?;
    mount::<str, str, str, str>(None, "/", None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None)
        .map_err(|error| format!("private mount propagation: {error}"))?;

    let store = open_store(
        project::store_config(Path::new(project::STORE)),
        &manifest.binding,
        manifest.cursor_key,
        1,
        64 * 1024 * 1024,
        ReservationBlocks::default(),
    )?;
    store.set_serial_low_water(0);
    let overlay = mkdtemp(
        Path::new(project::PROJECT)
            .join("workspace.XXXXXX")
            .as_path(),
    )?;
    let template = overlay.join("overlay.sqlite");
    let owner = Owner::start(
        &template,
        ProfileConfig::default(),
        OwnerConfig { namespaces: 1 },
    )?;
    let mounts = mkdtemp("/run/layerfs-guest.XXXXXX")?;
    fs::set_permissions(&mounts, Permissions::from_mode(0o711))?;
    let kernel = KernelServing::start(KernelConfig {
        mounts: mounts.clone(),
        owner_uid: 0,
        owner_gid: 0,
        command_uid: COMMAND_UID,
        command_gid: COMMAND_GID,
        ready_wait: Duration::from_secs(5),
        drain_wait: Duration::from_secs(5),
    })?;
    let service = Service::with_kernel(store, &owner, Arc::new(kernel));

    let Reply::Bound { token, .. } = control(&service, Request::Mount { workspace, branch })?
    else {
        unreachable!("Mount reply");
    };
    let outcome = execute(&service, token, command, args);
    let terminal = (|| -> Result<()> {
        let request = Request::Unmount(UnmountControl {
            token,
            discard: matches!(outcome, Ok(code) if code != 0),
        });
        let unmounted = service
            .execute_control(&request)
            .map_err(|error| format!("Unmount: {error:?}"))?;
        if let Reply::Dirty(report) = unmounted.reply {
            return Err(format!("Unmount refused local changes after Commit: {report:?}").into());
        }
        assert_eq!(unmounted.reply, Reply::Unmounted(token));
        drop(service);
        owner.stop()?;
        if let Some(kernel) = unmounted.kernel {
            kernel.removed?;
        }
        fs::remove_dir(&mounts)?;
        fs::remove_file(&template)?;
        fs::remove_dir(&overlay)?;
        Ok(())
    })();
    if let Err(error) = terminal {
        let detail = format!(
            "Workspace teardown at {} (overlay {}): {error}",
            mounts.display(),
            overlay.display()
        );
        if matches!(outcome, Ok(0)) {
            return Err(detail.into());
        }
        eprintln!("layerfs-guest: {detail}");
    }
    outcome
}

fn execute(
    service: &Service,
    token: WorkspaceToken,
    command: &OsStr,
    args: &[OsString],
) -> Result<u8> {
    let Reply::Ready(ready) = control(service, Request::Attach(token))? else {
        unreachable!("Attach reply");
    };
    let status = Command::new(command)
        .args(args)
        .current_dir(&ready.directory)
        .uid(COMMAND_UID)
        .gid(COMMAND_GID)
        .status()
        .map_err(|error| format!("could not launch {command:?} in the Workspace: {error}"))?;
    if !status.success() {
        // Shell-compatible exit status for a child terminated by a signal.
        return Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().expect("signal exit"))
            as u8);
    }
    match control(
        service,
        Request::Commit(CommitControl {
            token,
            force: false,
            message: None,
        }),
    )? {
        Reply::Committed(_) | Reply::MergeCommitted(_) => Ok(0),
        Reply::Conflict(report) => Err(format!("Commit refused: {report:?}").into()),
        _ => unreachable!("Commit reply"),
    }
}

fn control(service: &Service, request: Request) -> Result<Reply> {
    service
        .execute_control(&request)
        .map(|success| success.reply)
        .map_err(|error| format!("{request:?}: {error:?}").into())
}
