// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Routines and types for saving and restoring a snapshot of a VM.
//!
//! The snapshot format is a tar file structured as follows:
//! - `config.toml`: VM configuration data
//! - `global.json`: Global state data for the instance
//! - `devices/*.json`: Exported state for each device
//! - `memory/<start>-<end>.bin`: Raw memory covering guest-physical address
//!   range [start, end), with those addresses formatted in hex.

use std::convert::TryInto;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bhyve_api::{
    vdi_field_entry_v1, vdi_time_info_v1, ApiVersion, VAI_BOOT_HRTIME,
    VDC_VMM_ARCH, VDC_VMM_TIME,
};
use propolis::{
    chardev::UDSock,
    common::{GuestAddr, GuestRegion},
    migrate::{
        MigrateCtx, Migrator, PayloadOffer, PayloadOffers, PayloadOutputs,
    },
    vmm::VmmHdl,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use slog::{debug, info, warn};

use super::config::Config;
use super::Instance;

#[derive(Deserialize, Serialize)]
struct SnapshotDevice {
    pub instance_name: String,
    pub payload: Vec<SnapshotDevicePayload>,
}
#[derive(Deserialize, Serialize)]
struct SnapshotDevicePayload {
    pub kind: String,
    pub version: u32,
    pub data: String,
}

const DEVICE_DIR: &str = "devices";
const MEMORY_DIR: &str = "memory";
const CONFIG_NAME: &str = "config.toml";
const GLOBAL_NAME: &str = "global.json";

/// Save a snapshot of the current state of the given instance to disk.
pub(crate) fn save(
    guard: &mut super::InstState,
    config: &Config,
    log: &slog::Logger,
) -> anyhow::Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let snapshot = format!("{}-{}.tar", config.main.name, now.as_millis());

    info!(log, "saving snapshot of VM to {}", snapshot);

    let file =
        File::create(&snapshot).context("Failed to create snapshot file")?;
    let mut builder = TarBuilder::new(file);
    let mut header = tar::Header::new_gnu();
    header.set_mode(0o444);
    header.set_mtime(now.as_secs());

    info!(log, "Serializing VM config");
    {
        let config_bytes = toml::to_string(config)?.into_bytes();
        header.set_size(config_bytes.len() as u64);
        builder.append_data(&mut header, CONFIG_NAME, &config_bytes[..])?;
    }

    // Being called from the Quiesce state, all of the device pause work should
    // be done for us already.
    let machine = guard.machine.as_ref().unwrap();
    let hdl = machine.hdl.clone();
    let memctx = machine.acc_mem.access().unwrap();
    let migratectx = MigrateCtx { mem: &memctx };

    info!(log, "Serializing global VM state");
    {
        let global_config =
            export_global(&hdl).context("Failed to export global VM state")?;
        let global_bytes = serde_json::to_vec(&global_config)?;
        header.set_size(global_bytes.len() as u64);
        builder.append_data(&mut header, "global.json", &global_bytes[..])?;
    }

    // Add directories for devices and memory
    {
        let mut dir_header = tar::Header::new_gnu();
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_mode(0o555);
        dir_header.set_mtime(now.as_secs());
        dir_header.set_size(0);

        builder.append_dir(&mut dir_header, format!("{DEVICE_DIR}/"))?;
        builder.append_dir(&mut dir_header, format!("{MEMORY_DIR}/"))?;
    }

    info!(log, "Serializing VM device state");
    for (name, dev) in guard.inventory.devs.iter() {
        let device_data = match dev.migrate() {
            Migrator::NonMigratable => {
                anyhow::bail!(
                    "Can't snapshot instance with non-migratable device ({})",
                    name
                );
            }
            Migrator::Empty => continue,
            Migrator::Single(mech) => {
                let output = mech.export(&migratectx)?;
                SnapshotDevice {
                    instance_name: name.to_owned(),
                    payload: vec![SnapshotDevicePayload {
                        kind: output.kind.to_owned(),
                        version: output.version,
                        data: serde_json::to_string(&output.payload)?,
                    }],
                }
            }
            Migrator::Multi(mech) => {
                let mut outputs = PayloadOutputs::new();
                mech.export(&mut outputs, &migratectx)?;

                let mut payloads = Vec::new();
                for part in outputs {
                    payloads.push(SnapshotDevicePayload {
                        kind: part.kind.to_owned(),
                        version: part.version,
                        data: serde_json::to_string(&part.payload)?,
                    });
                }
                SnapshotDevice {
                    instance_name: name.to_owned(),
                    payload: payloads,
                }
            }
        };

        let device_bytes = serde_json::to_vec(&device_data)?;
        header.set_size(device_bytes.len() as u64);
        builder.append_data(
            &mut header,
            format!("{}/{}.json", DEVICE_DIR, name),
            &device_bytes[..],
        )?;
    }

    // TODO(luqmana) clean this up. make mem_bounds do the lo/hi calc? or just
    // use config values?
    const GB: usize = 1024 * 1024 * 1024;
    let mem_bounds = memctx
        .mem_bounds()
        .ok_or_else(|| anyhow::anyhow!("Failed to get VM RAM bounds"))?;
    let len: usize =
        (mem_bounds.end().0 - mem_bounds.start().0 + 1).try_into()?;
    let (lo, hi) = if len > 3 * GB {
        (3 * GB, Some(len.saturating_sub(4 * GB)))
    } else {
        (len, None)
    };
    info!(log, "Low RAM: {}, High RAM: {:?}", lo, hi);

    info!(log, "Writing low memory...");
    {
        let lo_mapping = memctx
            .direct_readable_region(&GuestRegion(GuestAddr(0), lo))
            .ok_or_else(|| anyhow::anyhow!("Failed to get lowmem region"))?;
        header.set_size(lo_mapping.len() as u64);
        let off = builder.append_space(
            &mut header,
            format!("{}/{:08x}-{:08x}.bin", MEMORY_DIR, 0, lo),
        )?;
        lo_mapping.pwrite(&builder.rawfd(), lo_mapping.len(), off as i64)?;
    }

    if let Some(hi) = hi {
        info!(log, "Writing high memory...");
        let start = 0x1_0000_0000;
        let hi_mapping = memctx
            .direct_readable_region(&GuestRegion(GuestAddr(start), hi))
            .ok_or_else(|| anyhow::anyhow!("Failed to get himem region"))?;

        header.set_size(hi_mapping.len() as u64);
        let end = start + hi as u64;
        let off = builder.append_space(
            &mut header,
            format!("{}/{:08x}-{:08x}.bin", MEMORY_DIR, start, end),
        )?;
        hi_mapping.pwrite(&builder.rawfd(), hi_mapping.len(), off as i64)?;
    }

    builder.inner_builder().finish()?;
    builder.into_file()?.flush()?;

    info!(log, "Snapshot saved to {}", snapshot);

    Ok(())
}

fn parse_mem_name(name: &str) -> anyhow::Result<(usize, usize)> {
    if let Some(addrs) = name.strip_suffix(".bin") {
        let mut fields = addrs.split('-');
        if let (Some(start), Some(end), None) =
            (fields.next(), fields.next(), fields.next())
        {
            let start = usize::from_str_radix(start, 16)?;
            let end = usize::from_str_radix(end, 16)?;
            if start >= end {
                anyhow::bail!("bad memory bounds {start} {end}");
            }
            return Ok((start, end));
        }
        anyhow::bail!("could not parse bounds of memory file {name}");
    } else {
        anyhow::bail!("memory file '{name}'does not end with .bin")
    }
}

/// Create an instance from a previously saved snapshot.
pub(crate) fn restore(
    path: impl AsRef<Path>,
    log: &slog::Logger,
) -> anyhow::Result<(Instance, Arc<UDSock>)> {
    info!(log, "restoring snapshot of VM from {}", path.as_ref().display());

    let file = File::open(&path).context("Failed to open snapshot file")?;
    let mut archive = TarArchive::new(file);

    let config: Config = {
        let config_ent = archive.named_entry(CONFIG_NAME)?;
        let config_bytes =
            config_ent.bytes().collect::<Result<Vec<u8>, _>>()?;
        toml::from_str(
            std::str::from_utf8(&config_bytes[..])
                .context("config should be valid utf-8")?,
        )
        .context("could not parse config")?
    };

    // We have enough to create the instance so let's do that first
    let (inst, com1_sock) = super::setup_instance(config, true, log)
        .context("Failed to create Instance with config in snapshot")?;

    let guard = inst.lock().unwrap();
    let machine = guard.machine.as_ref().unwrap();
    let hdl = machine.hdl.clone();
    let memctx = machine.acc_mem.access().unwrap();

    // Set the kernel VMM state to paused, so that devices can be consistently
    // loaded without timers and such attempting to fire.
    hdl.pause()?;

    // Ensure vCPUs are in the active state
    for vcpu in machine.vcpus.iter() {
        vcpu.activate().context("Failed to activate vCPU")?;
    }

    // Mimic state transitions propolis-server would go through for a live migration
    // XXX put instance in migrate-source state

    let global: VmGlobalState = {
        let global_ent = archive.named_entry(GLOBAL_NAME)?;
        let global_bytes =
            global_ent.bytes().collect::<Result<Vec<u8>, _>>()?;
        serde_json::from_slice(&global_bytes[..])
            .context("could not parse global data")?
    };
    import_global(&hdl, &global).context("failed to import global VM state")?;

    // Read device state data
    let device_data = archive
        .entries()?
        .filter_map(|ent| {
            let mut ent = ent.ok()?;
            let path = ent.path().ok()?.into_owned();

            let mut parts = path.components();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(dir), Some(_name), None)
                    if (dir.as_ref() as &Path) == Path::new(DEVICE_DIR) =>
                {
                    let mut dev_bytes = Vec::with_capacity(ent.size() as usize);
                    ent.read_to_end(&mut dev_bytes).ok()?;

                    Some(dev_bytes)
                }
                _ => None,
            }
        })
        .map(|data| serde_json::from_slice(&data[..]))
        .collect::<Result<Vec<SnapshotDevice>, serde_json::Error>>()?;

    // Locate and import RAM data
    let mem_segs = archive
        .entries()?
        .filter_map(|ent| {
            let ent = ent.ok()?;
            let path = ent.path().ok()?;

            let mut parts = path.components();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(dir), Some(name), None)
                    if (dir.as_ref() as &Path) == Path::new(MEMORY_DIR) =>
                {
                    Some((name.as_os_str().to_str()?.to_owned(), ent))
                }
                _ => None,
            }
        })
        .map(|(name, entry)| {
            let (start, end) = parse_mem_name(&name)?;
            let file_off = entry.raw_file_position();
            Ok((start, end, file_off))
        })
        .collect::<Result<Vec<_>, anyhow::Error>>()?;

    let fp = archive.as_file();
    for (start, end, file_off) in mem_segs {
        debug!(log, "Loading guest memory region {start:08x}-{end:08x}");
        let region = GuestRegion(GuestAddr(start as u64), end - start);
        let mapping =
            memctx.direct_writable_region(&region).ok_or_else(|| {
                anyhow::anyhow!("could not map region {region:?}")
            })?;
        mapping.pread(fp, mapping.len(), file_off as i64)?;
    }

    // Finally, let's restore the device state
    let migratectx = MigrateCtx { mem: &memctx };
    for snap_dev in device_data {
        let name = &snap_dev.instance_name;
        let dev = guard.inventory.devs.get(name).ok_or_else(|| {
            anyhow::anyhow!("unknown device in snapshot {}", name)
        })?;

        match dev.migrate() {
            Migrator::NonMigratable => anyhow::bail!(
                "can't restore snapshot with non-migratable device ({})",
                name
            ),
            Migrator::Empty => {
                // There really shouldn't be a payload for this
                warn!(
                    log,
                    "unexpected device state for device {} in snapshot", name
                );
            }
            Migrator::Single(mech) => {
                if snap_dev.payload.len() != 1 {
                    anyhow::bail!(
                        "Unexpected payload count {}",
                        snap_dev.payload.len()
                    );
                }
                let payload = &snap_dev.payload[0];
                let mut deser_data =
                    serde_json::Deserializer::from_str(&payload.data);

                let offer = PayloadOffer {
                    kind: &payload.kind,
                    version: payload.version,
                    payload: Box::new(<dyn erased_serde::Deserializer>::erase(
                        &mut deser_data,
                    )),
                };
                debug!(log, "Importing data into device {name}");
                mech.import(offer, &migratectx).with_context(|| {
                    format!("Failed to restore device state for {name}")
                })?;
            }
            Migrator::Multi(mech) => {
                let mut payload_desers: Vec<
                    serde_json::Deserializer<serde_json::de::StrRead>,
                > = Vec::with_capacity(snap_dev.payload.len());
                let mut metadata: Vec<(&str, u32)> =
                    Vec::with_capacity(snap_dev.payload.len());
                for payload in snap_dev.payload.iter() {
                    payload_desers.push(serde_json::Deserializer::from_str(
                        &payload.data,
                    ));
                    metadata.push((&payload.kind, payload.version));
                }
                let offer_iter = metadata
                    .iter()
                    .zip(payload_desers.iter_mut())
                    .map(|(meta, deser)| PayloadOffer {
                        kind: meta.0,
                        version: meta.1,
                        payload: Box::new(
                            <dyn erased_serde::Deserializer>::erase(deser),
                        ),
                    });

                let mut offer = PayloadOffers::new(offer_iter);
                debug!(log, "Importing data into device {name}");
                mech.import(&mut offer, &migratectx).with_context(|| {
                    format!("Failed to restore device state for {name}",)
                })?;

                let remain = offer.remaining().count();
                if remain > 0 {
                    return Err(anyhow::anyhow!(
                        "Device {} had {} remaining payload(s)",
                        name,
                        remain
                    ));
                }
            }
        }
    }

    drop(memctx);
    drop(guard);
    Ok((inst, com1_sock))
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct VmGlobalState {
    // Just using the raw boot_hrtime leaves room for all sorts of failures,
    // especially if a saved state file is used after a subsequent reboot of the
    // host.  These problems can be addressed later.
    pub boot_hrtime: i64,
    // Fixing up the guest TSC is left as an exercise for later
}

fn export_global(hdl: &VmmHdl) -> io::Result<VmGlobalState> {
    if hdl.api_version()? > ApiVersion::V11 {
        let info = hdl.data_op(VDC_VMM_TIME, 1).read::<vdi_time_info_v1>()?;

        Ok(VmGlobalState { boot_hrtime: info.vt_boot_hrtime })
    } else {
        let arch_entries: Vec<bhyve_api::vdi_field_entry_v1> =
            hdl.data_op(VDC_VMM_ARCH, 1).read_all()?;
        let boot_ent = arch_entries
            .iter()
            .find(|ent| ent.vfe_ident == VAI_BOOT_HRTIME)
            .expect("VAI_BOOT_HRTIME should be present");

        Ok(VmGlobalState { boot_hrtime: boot_ent.vfe_value as i64 })
    }
}
fn import_global(hdl: &VmmHdl, state: &VmGlobalState) -> io::Result<()> {
    if hdl.api_version()? > ApiVersion::V11 {
        let mut info =
            hdl.data_op(VDC_VMM_TIME, 1).read::<vdi_time_info_v1>()?;

        info.vt_boot_hrtime = state.boot_hrtime;
        hdl.data_op(VDC_VMM_TIME, 1).write(&info)?;

        Ok(())
    } else {
        let arch_entry =
            vdi_field_entry_v1::new(VAI_BOOT_HRTIME, state.boot_hrtime as u64);
        hdl.data_op(VDC_VMM_ARCH, 1).write(&arch_entry)?;
        Ok(())
    }
}

/// Add some convenience wrappers atop [tar::Builder]
struct TarBuilder(tar::Builder<File>);
impl TarBuilder {
    fn new(fp: File) -> Self {
        Self(tar::Builder::new(fp))
    }
    fn append_data(
        &mut self,
        header: &mut tar::Header,
        path: impl AsRef<Path>,
        data: impl Read,
    ) -> io::Result<()> {
        self.0.append_data(header, path, data)
    }

    fn append_space(
        &mut self,
        header: &mut tar::Header,
        path: impl AsRef<Path>,
    ) -> io::Result<u64> {
        let size = header.size()?;
        self.0.append_data(header, path, io::empty())?;

        let position = self.0.get_mut().stream_position()?;
        // data region should be in multiple of tar block size
        let seek_off = match size % 512 {
            0 => size,
            rem => size + (512 - rem),
        };
        self.0.get_mut().seek(SeekFrom::Current(seek_off as i64))?;

        Ok(position)
    }

    fn append_dir(
        &mut self,
        header: &mut tar::Header,
        path: impl AsRef<Path>,
    ) -> io::Result<()> {
        assert!(header.entry_type().is_dir());
        self.0.append_data(header, path, io::empty())
    }

    fn rawfd(&mut self) -> RawFd {
        self.0.get_mut().as_raw_fd()
    }
    fn inner_builder(&mut self) -> &mut tar::Builder<File> {
        &mut self.0
    }
    fn into_file(self) -> io::Result<File> {
        self.0.into_inner()
    }
}

/// Hold either a [tar::Archive] built atop a [File], or the [File] itself
enum TarInner {
    Raw(Option<File>),
    Tar(Option<tar::Archive<File>>),
}
impl TarInner {
    fn as_tar(&mut self) -> Option<&mut tar::Archive<File>> {
        match self {
            TarInner::Raw(..) => None,
            TarInner::Tar(arc) => Some(arc.as_mut().unwrap()),
        }
    }
    fn as_file(&mut self) -> Option<&mut File> {
        match self {
            TarInner::Raw(fp) => Some(fp.as_mut().unwrap()),
            TarInner::Tar(..) => None,
        }
    }
}

/// Wrap some of the [tar::Archive] functionality, making it easier to
/// repeatedly iterate over entries and/or gain access to the underlying [File]
/// resource.
struct TarArchive(TarInner);
impl TarArchive {
    fn new(fp: File) -> Self {
        Self(TarInner::Raw(Some(fp)))
    }

    fn reset_tar(&mut self) -> io::Result<&mut tar::Archive<File>> {
        self.0 = match &mut self.0 {
            TarInner::Raw(ref mut fp) => {
                TarInner::Tar(Some(tar::Archive::new(fp.take().unwrap())))
            }
            TarInner::Tar(ref mut arc) => {
                let mut fp = arc.take().unwrap().into_inner();
                let _ = fp.seek(SeekFrom::Start(0))?;
                TarInner::Tar(Some(tar::Archive::new(fp)))
            }
        };
        Ok(self.0.as_tar().unwrap())
    }

    fn as_file(&mut self) -> &mut File {
        self.0 = match &mut self.0 {
            TarInner::Raw(ref mut fp) => {
                TarInner::Raw(Some(fp.take().unwrap()))
            }
            TarInner::Tar(ref mut arc) => {
                TarInner::Raw(Some(arc.take().unwrap().into_inner()))
            }
        };
        self.0.as_file().unwrap()
    }

    fn named_entry(&mut self, name: &str) -> io::Result<tar::Entry<File>> {
        let tar = self.reset_tar()?;

        let entry = tar
            .entries_with_seek()?
            .find(|ent| {
                if let Ok(ent) = ent {
                    if let Ok(path) = ent.path() {
                        return path == Path::new(name);
                    }
                }
                false
            })
            .map(Result::unwrap);
        entry.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("file '{name}' not found in archive"),
            )
        })
    }

    fn entries(&mut self) -> io::Result<tar::Entries<File>> {
        let tar = self.reset_tar()?;
        tar.entries_with_seek()
    }
}
