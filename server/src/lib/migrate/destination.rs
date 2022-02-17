use futures::{SinkExt, StreamExt};
use std::sync::Arc;

use hyper::upgrade::Upgraded;
use propolis::dispatch::AsyncCtx;
use propolis::instance::Instance;
use slog::{error, info};
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
    {
        // TODO: Not exactly the right error
        let ctx = async_context
            .dispctx()
            .await
            .ok_or(MigrateError::InstanceNotInitialized)?;
        let machine = ctx.mctx;
        let _vmm_hdl = machine.hdl();
    }

    let mut proto = DestinationProtocol {
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
    proto.end();
    Ok(())
}

struct DestinationProtocol {
    migrate_context: Arc<MigrateContext>,
    #[allow(dead_code)]
    instance: Arc<Instance>,
    #[allow(dead_code)]
    async_context: AsyncCtx,
    conn: Framed<Upgraded, codec::LiveMigrationFramer>,
    log: slog::Logger,
}

impl DestinationProtocol {
    fn start(&mut self) {
        info!(self.log, "Entering Destination Migration Task");
    }

    async fn sync(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Sync).await;
        let preamble: Preamble = match self.read_msg().await? {
            codec::Message::Serialized(s) => {
                ron::de::from_str(&s).map_err(codec::ProtocolError::from)?
            }
            msg => {
                error!(
                    self.log,
                    "expected serialized preamble but received: {msg:?}"
                );
                Err(MigrateError::UnexpectedMessage)
            }
        }?;
        info!(self.log, "Src read Preamble: {:?}", preamble);
        // XXX: For demonstration purposes only.
        if preamble.vm_descr.vcpus != vec![0u32, 1, 2, 3] {
            error!(
                self.log,
                "invalid CPU count in preamble ({:?})", preamble.vm_descr.vcpus
            );
            return Err(MigrateError::InvalidInstanceState);
        }
        self.send_msg(codec::Message::Okay).await
    }

    async fn ram_push(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::RamPush).await;
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;
        // TODO(cross): Implement the rest of the RAM transfer protocol here. :-)
        let m = self.read_msg().await?;
        info!(self.log, "ram_push: got end {:?}", m);
        self.send_msg(codec::Message::MemDone).await
    }

    async fn device_state(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Device).await;
        self.send_msg(codec::Message::Okay).await?;
        self.read_ok().await
    }

    async fn arch_state(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Arch).await;
        self.send_msg(codec::Message::Okay).await?;
        self.read_ok().await
    }

    async fn ram_pull(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::RamPull).await;
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log, "ram_push: got end {:?}", m);
        self.send_msg(codec::Message::MemDone).await
    }

    async fn finish(&mut self) -> Result<(), MigrateError> {
        self.migrate_context.set_state(MigrationState::Finish).await;
        self.send_msg(codec::Message::Okay).await?;
        let _ = self.read_ok().await; // A failure here is ok.
        Ok(())
    }

    fn end(&mut self) {
        info!(self.log, "Destination Migration Successful");
    }

    async fn read_msg(&mut self) -> Result<codec::Message, MigrateError> {
        Ok(self.conn.next().await.unwrap()?)
    }

    async fn read_ok(&mut self) -> Result<(), MigrateError> {
        match self.read_msg().await? {
            codec::Message::Okay => Ok(()),
            msg => {
                error!(self.log, "expected `Okay` but received: {msg:?}");
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