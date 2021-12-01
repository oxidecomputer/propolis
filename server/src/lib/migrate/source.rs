use futures::{SinkExt, StreamExt};
use std::sync::Arc;

use hyper::upgrade::Upgraded;
use propolis::instance::Instance;
use slog::info;
use tokio_util::codec::Framed;

use crate::migrate::codec;
use crate::migrate::preamble::Preamble;
use crate::migrate::{MigrateContext, MigrateError, MigrationState};

type Result<T> = anyhow::Result<T, MigrateError>;

pub async fn migrate(
    migrate_context: Arc<MigrateContext>,
    instance: Arc<Instance>,
    conn: Upgraded,
    log: slog::Logger,
) -> Result<()> {
    let mut proto = SourceProtocol {
        migrate_context,
        instance,
        conn: Framed::new(conn, codec::LiveMigrationFramer::new()),
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

struct SourceProtocol {
    migrate_context: Arc<MigrateContext>,
    instance: Arc<Instance>,
    conn: Framed<Upgraded, codec::LiveMigrationFramer>,
    log: slog::Logger,
}

impl SourceProtocol {
    fn start(&mut self) {
        info!(self.log, "Entering Source Migration Task");
    }

    async fn sync(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::Sync).await;
        let preamble = Preamble::new(self.instance.as_ref());
        let s = ron::ser::to_string(&preamble)
            .map_err(|_| MigrateError::Encoding)?;
        self.send_msg(codec::Message::Serialized(s)).await?;
        self.read_ok().await
    }

    async fn ram_push(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::RamPush).await;
        let m = self.read_msg().await?;
        info!(self.log, "ram_push: got query {:?}", m);
        // TODO(cross): Implement the rest of the RAM transfer protocol here. :-)
        self.migrate_context.set_state(MigrationState::Pause).await;
        self.migrate_context.set_state(MigrationState::RamPushDirty).await;
        self.send_msg(codec::Message::MemEnd(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log, "ram_push: got done {:?}", m);
        Ok(())
    }

    async fn device_state(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::Device).await;
        self.read_ok().await?;
        self.send_msg(codec::Message::Okay).await
    }

    async fn arch_state(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::Arch).await;
        self.read_ok().await?;
        self.send_msg(codec::Message::Okay).await
    }

    async fn ram_pull(&mut self) -> Result<()> {
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

    async fn finish(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::Finish).await;
        self.read_ok().await?;
        let _ = self.send_msg(codec::Message::Okay).await; // A failure here is ok.
        Ok(())
    }

    fn end(&mut self) {
        info!(self.log, "Source Migration Successful");
    }

    async fn read_msg(&mut self) -> Result<codec::Message> {
        self.conn.next().await.unwrap().map_err(|_| MigrateError::Protocol)
    }

    async fn read_ok(&mut self) -> Result<()> {
        match self.read_msg().await? {
            codec::Message::Okay => Ok(()),
            _ => Err(MigrateError::Protocol),
        }
    }

    async fn send_msg(&mut self, m: codec::Message) -> Result<()> {
        self.conn.send(m).await.map_err(|_| MigrateError::Protocol)
    }
}
