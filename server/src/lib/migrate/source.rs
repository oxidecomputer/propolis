use futures::{future, SinkExt, StreamExt};
use propolis::inventory::Order;
use std::sync::Arc;
use std::time::Duration;
use tokio::{task, time};

use hyper::upgrade::Upgraded;
use propolis::dispatch::AsyncCtx;
use propolis::instance::{Instance, ReqState};
use slog::{error, info, warn};
use tokio_util::codec::Framed;

use crate::migrate::codec;
use crate::migrate::preamble::Preamble;
use crate::migrate::{MigrateContext, MigrateError, MigrationState};

pub async fn migrate(
    migrate_context: Arc<MigrateContext>,
    instance: Arc<Instance>,
    async_context: AsyncCtx,
    conn: Upgraded,
    log: slog::Logger,
) -> Result<(), MigrateError> {
    let mut proto = SourceProtocol {
        migrate_context,
        instance,
        async_context,
        conn: Framed::new(
            conn,
            codec::LiveMigrationFramer::new(log.new(slog::o!())),
        ),
        log,
    };
    proto.start();
    proto.sync().await?;
    proto.ram_push().await?;
    proto.device_state().await?;
    proto.arch_state().await?;
    proto.ram_pull().await?;
    proto.finish().await?;
    proto.end()?;
    Ok(())
}

struct SourceProtocol {
    migrate_context: Arc<MigrateContext>,
    instance: Arc<Instance>,
    #[allow(dead_code)]
    async_context: AsyncCtx,
    conn: Framed<Upgraded, codec::LiveMigrationFramer>,
    log: slog::Logger,
}

impl SourceProtocol {
    fn start(&mut self) {
        info!(self.log, "Entering Source Migration Task");
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Sync).await;
        let preamble = Preamble::new(self.instance.as_ref());
        let s = ron::ser::to_string(&preamble)
            .map_err(codec::ProtocolError::from)?;
        self.send_msg(codec::Message::Serialized(s)).await?;
        self.read_ok().await
    }

    async fn ram_push(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::RamPush).await;
        let m = self.read_msg().await?;
        info!(self.log, "ram_push: got query {:?}", m);
        // TODO(cross): Implement the rest of the RAM transfer protocol here. :-)
        self.pause().await?;
        self.migrate_context.set_state(MigrationState::RamPushDirty).await;
        self.send_msg(codec::Message::MemEnd(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log, "ram_push: got done {:?}", m);
        Ok(())
    }

    async fn pause(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Pause).await;

        // Ask the instance to begin transitioning to the paused state
        // This will inform each device to pause.
        info!(self.log, "Pausing devices");
        let (pause_tx, pause_rx) = std::sync::mpsc::channel();
        self.instance
            .migrate_pause(self.async_context.context_id(), pause_rx)?;

        // Grab a reference to all the devices that are a part of this Instance
        let mut devices = vec![];
        let inv = self.instance.inv();
        inv.for_each_node(Order::Pre, |_, rec| {
            devices.push((rec.name().to_owned(), Arc::clone(rec.entity())))
        });

        // Ask each device for a future indicating they've finishing pausing
        let mut migrate_ready_futs = vec![];
        for (name, device) in &devices {
            if let Some(migrate_hdl) = device.migrate() {
                let log = self.log.new(slog::o!("device" => name.clone()));
                let pause_fut = migrate_hdl.paused();
                migrate_ready_futs.push(task::spawn(async move {
                    if let Err(_) =
                        time::timeout(Duration::from_secs(2), pause_fut).await
                    {
                        error!(log, "Timed out pausing device");
                        return Err(());
                    }
                    info!(log, "Paused device");
                    Ok(())
                }));
            } else {
                warn!(self.log, "No migrate handle for {}", name);
                continue;
            }
        }

        // Now we wait for all the devices to have paused
        future::join_all(migrate_ready_futs)
            .await
            // Hoist out the JoinError's
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            // TODO: Better error
            .map_err(|_| MigrateError::InvalidInstanceState)?
            // Hoist out the pause task errors if any
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            // TODO: Better error
            .unwrap();

        // Inform the instance state machine we're done pausing
        pause_tx.send(()).unwrap();

        Ok(())
    }

    async fn device_state(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Device).await;
        self.read_ok().await?;
        self.send_msg(codec::Message::Okay).await
    }

    async fn arch_state(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Arch).await;
        self.read_ok().await?;
        self.send_msg(codec::Message::Okay).await
    }

    async fn ram_pull(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::RamPush).await;
        let m = self.read_msg().await?;
        info!(self.log, "ram_pull: got query {:?}", m);
        self.migrate_context.set_state(MigrationState::Pause).await;
        self.migrate_context.set_state(MigrationState::RamPushDirty).await;
        self.send_msg(codec::Message::MemEnd(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log, "ram_pull: got done {:?}", m);
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Finish).await;
        self.read_ok().await?;
        let _ = self.send_msg(codec::Message::Okay).await; // A failure here is ok.
        Ok(())
    }

    fn end(&mut self) -> Result<(), MigrateError> {
        self.instance.set_target_state(ReqState::Halt)?;
        info!(self.log, "Source Migration Successful");
        Ok(())
    }

    async fn read_msg(&mut self) -> Result<codec::Message, MigrateError> {
        Ok(self.conn.next().await.unwrap()?)
    }

    async fn read_ok(&mut self) -> Result<(), MigrateError> {
        match self.read_msg().await? {
            codec::Message::Okay => Ok(()),
            _ => todo!(),
        }
    }

    async fn send_msg(
        &mut self,
        m: codec::Message,
    ) -> Result<(), MigrateError> {
        Ok(self.conn.send(m).await?)
    }
}