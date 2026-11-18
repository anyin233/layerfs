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
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::Arc,
    time::Duration,
};

pub const CACHE_BYTES: usize = 64 * 1024 * 1024;
const COMMAND_UID: u32 = 1000;
const COMMAND_GID: u32 = 1000;

pub struct Runtime {
    service: Service,
    owner: Owner,
    mounts: PathBuf,
    overlay: PathBuf,
    stack: LayerStackId,
    release_confirmed: bool,
}

pub fn run(branch: &OsStr, command: &OsStr, args: &[OsString]) -> Result<u8> {
    let mut runtime = Runtime::open(None)?;
    let outcome = runtime.command(branch, |directory| {
        Command::new(command)
            .args(args)
            .current_dir(directory)
            .uid(COMMAND_UID)
            .gid(COMMAND_GID)
            .status()
            .map_err(|error| {
                format!("could not launch {command:?} in the Workspace: {error}").into()
            })
    });
    runtime.finish(outcome)
}

impl Runtime {
    pub fn open(shared: Option<Arc<layerfs_store_service::store::Store>>) -> Result<Self> {
        let manifest = project::manifest()?;
        if manifest.locator != project::STORE || manifest.profile != StoreProfile::Disposable {
            return Err("project manifest must name the guest's Disposable Store".into());
        }
        let device =
            fs::symlink_metadata("/dev/fuse").map_err(|error| format!("/dev/fuse: {error}"))?;
        if !device.file_type().is_char_device() || device.uid() != 0 || device.mode() & 0o077 != 0 {
            return Err(
                "/dev/fuse must be a private root-owned character device (mode 0600)".into(),
            );
        }
        unshare(CloneFlags::CLONE_NEWNS)
            .map_err(|error| format!("private mount namespace: {error}"))?;
        mount::<str, str, str, str>(None, "/", None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None)
            .map_err(|error| format!("private mount propagation: {error}"))?;

        let store = match shared {
            Some(store) => store,
            None => open_store(
                project::store_config(Path::new(project::STORE)),
                &manifest.binding,
                manifest.cursor_key,
                1,
                CACHE_BYTES,
                ReservationBlocks::default(),
            )?,
        };
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

        Ok(Self {
            service,
            owner,
            mounts,
            overlay,
            stack: LayerStackId::from_bytes(manifest.stack)?,
            release_confirmed: true,
        })
    }

    pub fn command(
        &mut self,
        branch: &OsStr,
        launch: impl FnOnce(&str) -> Result<ExitStatus>,
    ) -> Result<u8> {
        let name = HistoryName::new(branch.to_str().ok_or("branch name must be UTF-8")?)?;
        let branch = BranchId::named(self.stack, &name);
        let workspace =
            WorkspaceId::from_authority(project::random_bytes(&mut File::open("/dev/urandom")?)?)?;

        self.release_confirmed = false;
        let service = &self.service;
        let Reply::Bound { token, .. } = control(service, Request::Mount { workspace, branch })?
        else {
            unreachable!("Mount reply");
        };
        let outcome = execute(service, token, launch);
        let terminal = (|| -> Result<()> {
            let request = Request::Unmount(UnmountControl {
                token,
                discard: matches!(outcome, Ok(code) if code != 0),
            });
            let unmounted = service
                .execute_control(&request)
                .map_err(|error| format!("Unmount: {error:?}"))?;
            if let Reply::Dirty(report) = unmounted.reply {
                return Err(
                    format!("Unmount refused local changes after Commit: {report:?}").into(),
                );
            }
            assert_eq!(unmounted.reply, Reply::Unmounted(token));
            if let Some(kernel) = unmounted.kernel {
                kernel.removed?;
            }
            self.release_confirmed = true;
            Ok(())
        })();
        if let Err(error) = terminal {
            let detail = format!(
                "Workspace teardown at {} (overlay {}): {error}",
                self.mounts.display(),
                self.overlay.display()
            );
            if matches!(outcome, Ok(0)) {
                return Err(detail.into());
            }
            eprintln!("layerfs-guest: {detail}");
        }
        outcome
    }

    pub fn finish_task(self, outcome: Result<u8>) -> Result<u8> {
        let outcome = match outcome {
            Ok(crate::task::DISCARD) if self.release_confirmed => Ok(0),
            other => other,
        };
        self.finish(outcome)
    }

    pub fn finish(self, outcome: Result<u8>) -> Result<u8> {
        let cleanup = (|| -> Result<()> {
            drop(self.service);
            self.owner.stop()?;
            if self.release_confirmed {
                fs::remove_dir(self.mounts)?;
                fs::remove_file(self.overlay.join("overlay.sqlite"))?;
                fs::remove_dir(&self.overlay)?;
            } else {
                eprintln!(
                    "layerfs-guest: unconfirmed Workspace release; overlay retained at {}",
                    self.overlay.display()
                );
            }
            Ok(())
        })();
        if let Err(error) = cleanup {
            let detail = format!("Workspace cleanup at {}: {error}", self.overlay.display());
            if matches!(outcome, Ok(0)) {
                return Err(detail.into());
            }
            eprintln!("layerfs-guest: {detail}");
        }
        outcome
    }
}

fn execute(
    service: &Service,
    token: WorkspaceToken,
    launch: impl FnOnce(&str) -> Result<ExitStatus>,
) -> Result<u8> {
    let Reply::Ready(ready) = control(service, Request::Attach(token))? else {
        unreachable!("Attach reply");
    };
    let status = launch(&ready.directory)?;
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

// PID 1 of a fresh PID, mount, IPC, UTS and network namespace. The inherited
// cwd pins just this Workspace while a private /run hides the parent's mounts.
pub fn prepare_namespace(directory: &Path) -> Result<()> {
    for (at, mode) in [
        ("/request", "mode=000"),
        ("/tmp", "mode=1777"),
        ("/run", "mode=0755"),
    ] {
        mount(
            Some("tmpfs"),
            at,
            Some("tmpfs"),
            MsFlags::empty(),
            Some(mode),
        )?;
    }
    fs::create_dir_all(directory)?;
    mount(
        Some("."),
        directory,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )?;
    mount(
        Some("sys"),
        "/sys",
        Some("sysfs"),
        MsFlags::empty(),
        None::<&str>,
    )?;
    mount(
        Some("fusectl"),
        "/sys/fs/fuse/connections",
        Some("fusectl"),
        MsFlags::empty(),
        None::<&str>,
    )?;
    if !Command::new("/bin/busybox")
        .args(["ip", "link", "set", "lo", "up"])
        .status()?
        .success()
    {
        return Err("could not bring up private loopback".into());
    }
    Ok(())
}

pub fn isolated(directory: &OsStr, command: &OsStr, args: &[OsString]) -> Result<u8> {
    let directory = Path::new(directory);
    prepare_namespace(directory)?;
    let status = Command::new(command)
        .args(args)
        .current_dir(directory)
        .uid(COMMAND_UID)
        .gid(COMMAND_GID)
        .status()?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().expect("signal exit")) as u8)
}
