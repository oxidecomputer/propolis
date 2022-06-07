//! Routines and types for saving and restoring a snapshot of a VM.
//!
//! TODO(luqmana) do this in a more structed way, it's a fun mess of toml+json+binary right now
//!
//! The snapshot format is a simple "tag-length-value" (TLV) encoding.
//! The tag is a single byte, length is a fixed 8 bytes (in big-endian)
//! which corresponds the length in bytes of the subsequent value.
//! Possible tags are:
//!     0   - VM Config
//!     1   - VM Device State
//!     2   - Low Mem
//!     3   - High Mem

use std::convert::TryInto;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::future;
use propolis::{
    common::{GuestAddr, GuestRegion},
    instance::{Instance, MigratePhase, MigrateRole, ReqState, State},
    inventory::Order,
    migrate::Migrator,
};
use slog::{error, info};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::{task, time};

use crate::config::Config;

/// Save a snapshot of the current state of the given instance to disk.
pub async fn save(
    log: slog::Logger,
    inst: Arc<Instance>,
    config: Config,
) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    let snapshot = format!("{}-{}.bin", config.get_name(), now);

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
    inst.set_target_state(ReqState::StartMigrate)?;
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
    inst.migrate_pause(async_ctx.context_id(), pause_rx)?;

    // Ask each device for a future indicating they've finishing pausing
    let mut migrate_ready_futs = vec![];
    for (name, device) in &devices {
        let log = log.new(slog::o!("device" => name.clone()));
        let device = Arc::clone(device);
        let pause_fut = device.paused();
        migrate_ready_futs.push(task::spawn(async move {
            if let Err(_) =
                time::timeout(Duration::from_secs(2), pause_fut).await
            {
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

    // Inform the instance state machine we're done pausing
    pause_tx.send(()).unwrap();

    let dispctx = async_ctx
        .dispctx()
        .await
        .ok_or(anyhow::anyhow!("Failed to get DispCtx"))?;

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
                    device_states.push((rec.name().to_owned(), payload));
                }
            }
            Ok(())
        })?;
        serde_json::ser::to_vec(&device_states)?
    };

    // TODO(luqmana) clean this up. make mem_bounds do the lo/hi calc? or just use config values?
    const GB: usize = 1024 * 1024 * 1024;
    let memctx = dispctx.mctx.memctx();
    let mem_bounds = memctx
        .mem_bounds()
        .ok_or(anyhow::anyhow!("Failed to get VM RAM bounds"))?;
    let len: usize = (mem_bounds.end.0 - mem_bounds.start.0 + 1).try_into()?;
    let (lo, hi) = if len > 3 * GB {
        (3 * GB, Some(len.saturating_sub(4 * GB)))
    } else {
        (len, None)
    };
    info!(log, "Low RAM: {}, High RAM: {:?}", lo, hi);

    let lo_mapping = memctx
        .direct_readable_region(&GuestRegion(GuestAddr(0), lo))
        .ok_or(anyhow::anyhow!("Failed to get lowmem region"))?;
    let hi_mapping = hi
        .map(|hi| {
            memctx
                .direct_readable_region(&GuestRegion(
                    GuestAddr(0x1_0000_0000),
                    hi,
                ))
                .ok_or(anyhow::anyhow!("Failed to get himem region"))
        })
        .transpose()?;

    // Write snapshot to disk
    let file = tokio::fs::File::create(&snapshot)
        .await
        .context("Failed to create snapshot file")?;
    let mut file = tokio::io::BufWriter::new(file);

    info!(log, "Writing VM config...");
    let config_bytes = toml::to_string(&config)?.into_bytes();
    file.write_u8(0).await?;
    file.write_u64(config_bytes.len().try_into()?).await?;
    file.write_all(&config_bytes).await?;

    info!(log, "Writing device state...");
    file.write_u8(1).await?;
    file.write_u64(device_states.len().try_into()?).await?;
    file.write_all(&device_states).await?;

    info!(log, "Writing memory...");

    // Low Mem
    // Note `pwrite` doesn't update the current position, so we do it manually
    file.write_u8(2).await?;
    file.write_u64(lo.try_into()?).await?;
    let offset = file.stream_position().await?.try_into()?;
    lo_mapping.pwrite(file.get_ref(), lo, offset)?; // Blocks; not great
    file.seek(std::io::SeekFrom::Current(lo.try_into()?)).await?;

    if let (Some(hi), Some(hi_mapping)) = (hi, hi_mapping) {
        // High Mem
        file.write_u8(3).await?;
        file.write_u64(hi.try_into()?).await?;
        let offset = file.stream_position().await?.try_into()?;
        hi_mapping.pwrite(file.get_ref(), hi, offset)?; // Blocks; not great
        file.seek(std::io::SeekFrom::Current(hi.try_into()?)).await?;
    }

    drop(file);
    info!(log, "Snapshot saved to {}", snapshot);

    // Clean up instance.
    drop(dispctx);
    drop(async_ctx);
    inst.set_target_state(ReqState::Halt)?;

    Ok(())
}
