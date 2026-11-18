//! Publishes one sealed Store and its original authority together on the guest disk.
use crate::Result;
use layerfs_api::{InitRequest, ProjectApi};
use layerfs_bridge::{duplex::MAX_PLAINTEXT_BYTES, provision::StoreManifest};
use layerfs_history::{BranchId, HistoryCatalogConfig, HistoryName, LayerStackId};
use layerfs_persistence::{PersistenceConfig, SqlitePersistenceProfile};
use layerfs_storage::StoragePolicy;
use layerfs_telemetry::timer::Timing;
use nix::unistd::{mkdtemp, Uid};
use std::{
    fs::{self, File},
    io::Read,
    os::unix::fs::MetadataExt,
    path::Path,
    time::{Duration, Instant},
};

const DATA: &str = "/data";
pub const PROJECT: &str = "/data/layerfs-guest";
pub const STORE: &str = "/data/layerfs-guest/store.sqlite";
const MANIFEST: &str = "manifest";

pub fn check_guest() -> Result<()> {
    if !Uid::effective().is_root() {
        return Err("layerfs-guest must be started as root inside the guest".into());
    }
    let data = fs::symlink_metadata(DATA).map_err(|error| format!("{DATA}: {error}"))?;
    if !data.is_dir() || data.uid() != 0 || data.mode() & 0o022 != 0 {
        return Err(
            "/data must be a root-owned directory without group or other write access".into(),
        );
    }
    Ok(())
}

pub fn init() -> Result<()> {
    match fs::symlink_metadata(PROJECT) {
        Ok(_) => return Err(format!("project destination already exists at {PROJECT}").into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    let staging = mkdtemp("/data/.layerfs-guest-init.XXXXXX")?;
    let publish = || -> Result<()> {
        let mut random = File::open("/dev/urandom")?;
        let stack = LayerStackId::from_authority(random_bytes(&mut random)?);
        let branch_name = HistoryName::new("work")?;
        let request = InitRequest {
            source: "/input".into(),
            store: store_config(&staging.join("store.sqlite")),
            locator: STORE.into(),
            policy: StoragePolicy::frozen_default(),
            catalog: HistoryCatalogConfig {
                binding_key: random_bytes::<32>(&mut random)?.to_vec(),
                cursor_key: random_bytes(&mut random)?,
                incarnation: 1,
            },
            stack,
            stack_name: HistoryName::new("project")?,
            branch: BranchId::named(stack, &branch_name),
            branch_name,
            scope_seed: random_bytes(&mut random)?,
            deadline: Instant::now() + Duration::from_secs(u64::from(u32::MAX)),
        };
        let sealed =
            Timing::disabled("guest.init", |timer| ProjectApi::new().init(request, timer)).0?;
        fs::write(staging.join(MANIFEST), sealed.manifest.encode()?)?;
        // Both files become visible together. A concurrently published project
        // already contains both files, so rename cannot replace it.
        fs::rename(&staging, PROJECT)?;
        Ok(())
    };
    publish().map_err(|error| format!("Init at {}: {error}", staging.display()).into())
}

pub fn manifest() -> Result<StoreManifest> {
    let directory =
        fs::symlink_metadata(PROJECT).map_err(|error| format!("project at {PROJECT}: {error}"))?;
    if !directory.is_dir() || directory.uid() != 0 || directory.mode() & 0o077 != 0 {
        return Err(format!("{PROJECT} must be a private root-owned directory").into());
    }
    let mut bytes = Vec::new();
    File::open(Path::new(PROJECT).join(MANIFEST))?
        .take((MAX_PLAINTEXT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    Ok(StoreManifest::decode(&bytes)?)
}

pub fn store_config(path: &Path) -> PersistenceConfig {
    PersistenceConfig::sqlite(path).with_sqlite_profile(SqlitePersistenceProfile::Disposable)
}

pub fn random_bytes<const N: usize>(source: &mut File) -> std::io::Result<[u8; N]> {
    let mut bytes = [0; N];
    source.read_exact(&mut bytes)?;
    Ok(bytes)
}
