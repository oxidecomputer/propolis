//! Routines and types for saving and restoring a snapshot of a VM.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use futures::future;
use propolis::{
    instance::{Instance, MigratePhase, MigrateRole, ReqState, State},
    inventory::Order,
    migrate::Migrator,
};
use slog::{error, info};
use tokio::{task, time};

pub async fn save(
    log: slog::Logger,
    inst: Arc<Instance>,
) -> anyhow::Result<()> {
    info!(log, "saving snapshot of VM");

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
    drop(dispctx);

    // TODO(luqmana) write to file in structured way
    serde_json::to_writer(std::io::stdout(), &device_states)?;

    // Clean up instance.
    inst.set_target_state(ReqState::Halt)?;

    Ok(())
}
