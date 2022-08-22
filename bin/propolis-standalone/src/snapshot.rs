//! Routines and types for saving and restoring a snapshot of a VM.
//!
//! TODO(luqmana) do this in a more structed way, it's a fun mess of toml+json+binary right now
//!
//! The snapshot format is a simple "tag-length-value" (TLV) encoding.
//! The tag is a single byte, length is a fixed 8 bytes (in big-endian)
//! which corresponds the length in bytes of the subsequent value.
//! Possible tags are:
//!     0   - VM Config
//!     1   - Global VM State
//!     2   - VM Device State
//!     3   - Low Mem
//!     4   - High Mem

use std::convert::{TryFrom, TryInto};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::future;
use propolis::{
    chardev::UDSock,
    common::{GuestAddr, GuestRegion},
    instance::{Instance, MigratePhase, MigrateRole, ReqState, State},
    inventory::Order,
    migrate::Migrator,
};
use slog::{error, info, warn};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    runtime::Handle,
};
use tokio::{task, time};

use propolis_standalone_shared as shared;
use shared::{Config, SnapshotTag};

/// Save a snapshot of the current state of the given instance to disk.
pub async fn save(
    log: slog::Logger,
    inst: Arc<Instance>,
    config: Config,
) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let snapshot = format!("{}-{}.bin", config.main.name, now);

    info!(log, "saving snapshot of VM to {}", snapshot);

    // Grab AsyncCtx so we can get to the Dispatcher
    let async_ctx = inst.async_ctx();

    // Mimic state transitions propolis-server would go through for a live migration
    // TODO(luqmana): refactor and share implementation with propolis-server
    let state_change_fut = {
        // Start waiting before we request the transition lest we miss it
        let inst = inst.clone();
        task::spawn_blocking(move || {
            inst.wait_for_state(State::Migrate(
                MigrateRole::Source,
                MigratePhase::Start,
            ));
        })
    };
    inst.set_target_state(ReqState::MigrateStart)?;
    state_change_fut.await?;

    // Grab reference to all the devices that are a part of this Instance
    let mut devices = vec![];
    let _ = inst.inv().for_each_node(Order::Post, |_, rec| {
        devices.push((rec.name().to_owned(), Arc::clone(rec.entity())));
        Ok::<_, ()>(())
    });

    // Ask the instance to begin transitioning to the paused state
    // This will inform each device to pause
    info!(log, "Pausing devices");
    let (pause_tx, pause_rx) = std::sync::mpsc::channel();
    inst.migrate_pause(async_ctx.context_id(), pause_rx)?.notified().await;

    // Ask each device for a future indicating they've finishing pausing
    let mut migrate_ready_futs = vec![];
    for (name, device) in &devices {
        let log = log.new(slog::o!("device" => name.clone()));
        let device = Arc::clone(device);
        let pause_fut = device.paused();
        migrate_ready_futs.push(task::spawn(async move {
            if time::timeout(Duration::from_secs(2), pause_fut).await.is_err() {
                error!(log, "Timed out pausing device");
                return Err(device);
            }
            info!(log, "Paused device");
            Ok(())
        }));
    }

    // Now we wait for all the devices to have paused
    let pause = future::join_all(migrate_ready_futs)
        .await
        // Hoist out the JoinError's
        .into_iter()
        .collect::<Result<Vec<_>, _>>();
    let timed_out = match pause {
        Ok(future_res) => {
            // Grab just the ones that failed
            future_res
                .into_iter()
                .filter(Result::is_err)
                .map(Result::unwrap_err)
                .collect::<Vec<_>>()
        }
        Err(err) => {
            return Err(err).context("Failed to join paused devices future");
        }
    };

    // Bail out if any devices timed out
    if !timed_out.is_empty() {
        anyhow::bail!("Failed to pause all devices: {timed_out:?}");
    }

    let state_change_fut = {
        // Start waiting before we let the instance know we're
        // done pausing so we don't miss the notification
        let inst = inst.clone();
        task::spawn_blocking(move || {
            inst.wait_for_state(State::Migrate(
                MigrateRole::Source,
                MigratePhase::Paused,
            ));
        })
    };

    // Inform the instance state machine we're done pausing
    // and wait for it to pause on its end
    pause_tx.send(()).unwrap();
    state_change_fut.await?;

    let dispctx = async_ctx
        .dispctx()
        .await
        .ok_or_else(|| anyhow::anyhow!("Failed to get DispCtx"))?;

    info!(log, "Serializing global VM state");
    let global_state = {
        let hdl = dispctx.mctx.hdl();
        let global_state =
            hdl.export().context("Failed to export global VM state")?;
        serde_json::to_vec(&global_state)?
    };

    info!(log, "Serializing VM device state");
    let device_states = {
        let mut device_states = vec![];
        inst.inv().for_each_node(Order::Pre, |_, rec| {
            let entity = rec.entity();
            match entity.migrate() {
                Migrator::NonMigratable => {
                    anyhow::bail!(
                    "Can't snapshot instance with non-migratable device ({})",
                    rec.name()
                );
                }
                Migrator::Simple => {}
                Migrator::Custom(migrate) => {
                    let payload = migrate.export(&dispctx);
                    device_states.push((
                        rec.name().to_owned(),
                        serde_json::to_vec(&payload)?,
                    ));
                }
            }
            Ok(())
        })?;
        serde_json::to_vec(&device_states)?
    };

    // TODO(luqmana) clean this up. make mem_bounds do the lo/hi calc? or just use config values?
    const GB: usize = 1024 * 1024 * 1024;
    let memctx = dispctx.mctx.memctx();
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

    let lo_mapping = memctx
        .direct_readable_region(&GuestRegion(GuestAddr(0), lo))
        .ok_or_else(|| anyhow::anyhow!("Failed to get lowmem region"))?;
    let hi_mapping = hi
        .map(|hi| {
            memctx
                .direct_readable_region(&GuestRegion(
                    GuestAddr(0x1_0000_0000),
                    hi,
                ))
                .ok_or_else(|| anyhow::anyhow!("Failed to get himem region"))
        })
        .transpose()?;

    // Write snapshot to disk
    let file = File::create(&snapshot)
        .await
        .context("Failed to create snapshot file")?;
    let mut file = tokio::io::BufWriter::new(file);

    info!(log, "Writing VM config...");
    let config_bytes = toml::to_string(&config)?.into_bytes();
    file.write_u8(SnapshotTag::Config.into()).await?;
    file.write_u64(config_bytes.len().try_into()?).await?;
    file.write_all(&config_bytes).await?;

    info!(log, "Writing global state...");
    file.write_u8(SnapshotTag::Global.into()).await?;
    file.write_u64(global_state.len().try_into()?).await?;
    file.write_all(&global_state).await?;

    info!(log, "Writing device state...");
    file.write_u8(SnapshotTag::Device.into()).await?;
    file.write_u64(device_states.len().try_into()?).await?;
    file.write_all(&device_states).await?;

    info!(log, "Writing memory...");

    // Low Mem
    // Note `pwrite` doesn't update the current position, so we do it manually
    file.write_u8(SnapshotTag::Lowmem.into()).await?;
    file.write_u64(lo.try_into()?).await?;
    let offset = file.stream_position().await?.try_into()?;
    lo_mapping.pwrite(file.get_ref(), lo, offset)?; // Blocks; not great
    file.seek(std::io::SeekFrom::Current(lo.try_into()?)).await?;

    // High Mem
    file.write_u8(SnapshotTag::Himem.into()).await?;
    if let (Some(hi), Some(hi_mapping)) = (hi, hi_mapping) {
        file.write_u64(hi.try_into()?).await?;
        let offset = file.stream_position().await?.try_into()?;
        hi_mapping.pwrite(file.get_ref(), hi, offset)?; // Blocks; not great
        file.seek(std::io::SeekFrom::Current(hi.try_into()?)).await?;
    } else {
        // Even if there's no high mem mapped, write out an empty len to the snapshot
        file.write_u64(0).await?;
    }

    file.flush().await?;

    info!(log, "Snapshot saved to {}", snapshot);

    // Clean up instance.
    drop(dispctx);
    drop(async_ctx);
    inst.set_target_state(ReqState::Halt)?;

    Ok(())
}

/// Create an instance from a previously saved snapshot.
pub async fn restore(
    log: slog::Logger,
    path: impl AsRef<Path>,
) -> anyhow::Result<(Config, Arc<Instance>, Arc<UDSock>)> {
    info!(log, "restoring snapshot of VM from {}", path.as_ref().display());

    let file =
        File::open(&path).await.context("Failed to open snapshot file")?;
    let mut file = tokio::io::BufReader::new(file);

    // First off we need the config
    let config: Config = {
        match SnapshotTag::try_from(file.read_u8().await?) {
            Ok(SnapshotTag::Config) => {}
            _ => anyhow::bail!("Expected VM config"),
        }
        let config_len = file.read_u64().await?;
        let mut config_buf = vec![0; config_len.try_into()?];
        file.read_exact(&mut config_buf).await?;
        toml::from_slice(&config_buf)?
    };

    // We have enough to create the instance so let's do that first
    let (inst, com1_sock) =
        super::setup_instance(log.clone(), config.clone(), Handle::current())
            .context("Failed to create Instance with config in snapshot")?;

    // Mimic state transitions propolis-server would go through for a live migration
    // TODO(luqmana): refactor and share implementation with propolis-server
    let state_change_fut = {
        // Start waiting before we request the transition lest we miss it
        let inst = inst.clone();
        task::spawn_blocking(move || {
            inst.wait_for_state(State::Migrate(
                MigrateRole::Destination,
                MigratePhase::Start,
            ));
        })
    };
    inst.set_target_state(ReqState::MigrateResume)?;
    state_change_fut.await?;

    // Grab a DispCtx to populate the saved state
    let async_ctx = inst.async_ctx();
    let dispctx = async_ctx
        .dispctx()
        .await
        .ok_or_else(|| anyhow::anyhow!("Failed to get DispCtx"))?;

    {
        // Grab the global VM state
        match SnapshotTag::try_from(file.read_u8().await?) {
            Ok(SnapshotTag::Global) => {}
            _ => anyhow::bail!("Expected VM config"),
        }
        let state_len = file.read_u64().await?;
        let mut global_state = vec![0; state_len.try_into()?];
        file.read_exact(&mut global_state).await?;
        let mut deserializer =
            serde_json::Deserializer::from_slice(&global_state);
        let deserializer =
            &mut <dyn erased_serde::Deserializer>::erase(&mut deserializer);

        // Restore it
        dispctx
            .mctx
            .hdl()
            .import(deserializer)
            .context("Failed to import global VM state")?;
    }

    // Next are the devices
    let device_states = {
        match SnapshotTag::try_from(file.read_u8().await?) {
            Ok(SnapshotTag::Device) => {}
            _ => anyhow::bail!("Expected VM config"),
        }
        let state_len = file.read_u64().await?;
        let mut state_buf = vec![0; state_len.try_into()?];
        file.read_exact(&mut state_buf).await?;
        state_buf
    };

    // Finally we have our RAM

    // Get low mem length and offset
    match SnapshotTag::try_from(file.read_u8().await?) {
        Ok(SnapshotTag::Lowmem) => {}
        _ => anyhow::bail!("Expected VM config"),
    }
    let lo_mem: usize = file.read_u64().await?.try_into()?;
    let lo_offset = file.stream_position().await?.try_into()?;

    // Seek past low mem blob and get high mem length and offset
    file.seek(std::io::SeekFrom::Current(lo_mem.try_into()?)).await?;
    match SnapshotTag::try_from(file.read_u8().await?) {
        Ok(SnapshotTag::Himem) => {}
        _ => anyhow::bail!("Expected VM config"),
    }
    let hi_mem: usize = file.read_u64().await?.try_into()?;
    let hi_offset = file.stream_position().await?.try_into()?;

    info!(log, "Low RAM: {}, High RAM: {}", lo_mem, hi_mem);

    let memctx = dispctx.mctx.memctx();

    let lo_mapping = memctx.direct_writable_region_by_name("lowmem")?;
    if lo_mem != lo_mapping.len() {
        anyhow::bail!("Mismatch between expected low mem region and snapshot");
    }
    // Populate from snapshot
    if lo_mem != lo_mapping.pread(file.get_ref(), lo_mem, lo_offset)? {
        anyhow::bail!("Failed to populate low mem from snapshot");
    }

    if hi_mem != 0 {
        let hi_mapping = memctx.direct_writable_region_by_name("highmem")?;
        if hi_mem != hi_mapping.len() {
            anyhow::bail!(
                "Mismatch between expected high mem region and snapshot"
            );
        }
        // Populate from snapshot
        if lo_mem != hi_mapping.pread(file.get_ref(), hi_mem, hi_offset)? {
            anyhow::bail!("Failed to populate high mem from snapshot");
        }
    }

    // Finally, let's restore the device state
    let inv = inst.inv();
    let devices: Vec<(String, Vec<u8>)> =
        serde_json::from_slice(&device_states)
            .context("Failed to deserialize device state")?;
    for (name, payload) in devices {
        let dev_ent = inv.get_by_name(&name).ok_or_else(|| {
            anyhow::anyhow!("unknown device in snapshot {}", name)
        })?;

        match dev_ent.migrate() {
            Migrator::NonMigratable => anyhow::bail!(
                "can't restore snapshot with non-migratable device ({})",
                name
            ),
            Migrator::Simple => {
                // There really shouldn't be a payload for this
                warn!(
                    log,
                    "unexpected device state for device {} in snapshot", name
                );
            }
            Migrator::Custom(migrate) => {
                let mut deserializer =
                    serde_json::Deserializer::from_slice(&payload);
                let deserializer = &mut <dyn erased_serde::Deserializer>::erase(
                    &mut deserializer,
                );
                migrate
                    .import(dev_ent.type_name(), deserializer, &dispctx)
                    .with_context(|| {
                        anyhow::anyhow!(
                            "Failed to restore device state for {}",
                            name
                        )
                    })?;
            }
        }
    }

    Ok((config, inst, com1_sock))
}
