use futures::{future, SinkExt, StreamExt};
use propolis::inventory::{Entity, Order};
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::{task, time};

use hyper::upgrade::Upgraded;
use propolis::instance::ReqState;
use slog::{error, info, warn};
use tokio_util::codec::Framed;

use crate::migrate::codec::{self, LiveMigrationFramer};
use crate::migrate::preamble::Preamble;
use crate::migrate::{MigrateContext, MigrateError, MigrationState};

pub async fn migrate(
    mctx: Arc<MigrateContext>,
    conn: Upgraded,
) -> Result<(), MigrateError> {
    let mut proto = SourceProtocol::new(mctx, conn);
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
    /// The migration context which also contains the Instance handle.
    mctx: Arc<MigrateContext>,

    /// Transport to the destination Instance.
    conn: Framed<Upgraded, LiveMigrationFramer>,

    /// List of devices attached to device
    devices: Vec<(String, Arc<dyn Entity>)>,
}

impl SourceProtocol {
    fn new(mctx: Arc<MigrateContext>, conn: Upgraded) -> Self {
        // Grab a reference to all the devices that are a part of this Instance
        let mut devices = vec![];
        mctx.instance.inv().for_each_node(Order::Pre, |_, rec| {
            devices.push((rec.name().to_owned(), Arc::clone(rec.entity())))
        });

        let codec_log = mctx.log.new(slog::o!());
        Self {
            mctx,
            conn: Framed::new(conn, LiveMigrationFramer::new(codec_log)),
            devices,
        }
    }

    fn log(&self) -> &slog::Logger {
        &self.mctx.log
    }

    fn start(&mut self) {
        info!(self.log(), "Entering Source Migration Task");
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Sync).await;
        let preamble = Preamble::new(self.mctx.instance.as_ref());
        let s = ron::ser::to_string(&preamble)
            .map_err(codec::ProtocolError::from)?;
        self.send_msg(codec::Message::Serialized(s)).await?;
        self.read_ok().await
    }

    async fn ram_push(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::RamPush).await;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_push: got query {:?}", m);
        // TODO(cross): Implement the rest of the RAM transfer protocol here. :-)
        self.pause().await?;
        self.mctx.set_state(MigrationState::RamPushDirty).await;
        self.send_msg(codec::Message::MemEnd(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_push: got done {:?}", m);
        Ok(())
    }

    async fn pause(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Pause).await;

        // Ask the instance to begin transitioning to the paused state
        // This will inform each device to pause.
        info!(self.log(), "Pausing devices");
        let (pause_tx, pause_rx) = std::sync::mpsc::channel();
        self.mctx
            .instance
            .migrate_pause(self.mctx.async_ctx.context_id(), pause_rx)?;

        // Ask each device for a future indicating they've finishing pausing
        let mut migrate_ready_futs = vec![];
        for (name, device) in &self.devices {
            if let Some(migrate_hdl) = device.migrate() {
                let log = self.log().new(slog::o!("device" => name.clone()));
                let device = Arc::clone(device);
                let pause_fut = migrate_hdl.paused();
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
            } else {
                warn!(self.log(), "No migrate handle for {}", name);
                continue;
            }
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
                error!(
                    self.log(),
                    "joining paused devices future failed: {err}"
                );
                return Err(MigrateError::SourcePause);
            }
        };

        // Bail out if any devices timed out
        // TODO: rollback already paused devices
        if !timed_out.is_empty() {
            error!(self.log(), "Failed to pause all devices: {timed_out:?}");
            return Err(MigrateError::SourcePause);
        }

        // Inform the instance state machine we're done pausing
        pause_tx.send(()).unwrap();

        Ok(())
    }

    async fn device_state(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Device).await;
        self.read_ok().await?;
        self.send_msg(codec::Message::Okay).await
    }

    async fn arch_state(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Arch).await;
        self.read_ok().await?;
        self.send_msg(codec::Message::Okay).await
    }

    async fn ram_pull(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::RamPush).await;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got query {:?}", m);
        self.mctx.set_state(MigrationState::Pause).await;
        self.mctx.set_state(MigrationState::RamPushDirty).await;
        self.send_msg(codec::Message::MemEnd(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log(), "ram_pull: got done {:?}", m);
        Ok(())
    }

    async fn finish(&mut self) -> Result<(), MigrateError> {
        self.mctx.set_state(MigrationState::Finish).await;
        self.read_ok().await?;
        let _ = self.send_msg(codec::Message::Okay).await; // A failure here is ok.
        Ok(())
    }

    fn end(&mut self) -> Result<(), MigrateError> {
        self.mctx.instance.set_target_state(ReqState::Halt)?;
        info!(self.log(), "Source Migration Successful");
        Ok(())
    }

    async fn read_msg(&mut self) -> Result<codec::Message, MigrateError> {
        Ok(self.conn.next().await.ok_or_else(|| {
            codec::ProtocolError::Io(io::Error::from(io::ErrorKind::BrokenPipe))
        })??)
    }

    async fn read_ok(&mut self) -> Result<(), MigrateError> {
        match self.read_msg().await? {
            codec::Message::Okay => Ok(()),
            msg => {
                error!(self.log(), "expected `Okay` but received: {msg:?}");
                Err(MigrateError::UnexpectedMessage)
            }
        }
    }

    async fn send_msg(
        &mut self,
        m: codec::Message,
    ) -> Result<(), MigrateError> {
        Ok(self.conn.send(m).await?)
    }
}
