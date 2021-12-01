use futures::{SinkExt, StreamExt};
use std::sync::Arc;

use dropshot::RequestContext;
use hyper::upgrade::Upgraded;
use slog::info;
use tokio_util::codec::Framed;

use crate::migrate::codec;
use crate::migrate::preamble::Preamble;
use crate::migrate::{MigrateContext, MigrateError, MigrationState};
use crate::server::Context;

type Result<T> = anyhow::Result<T, MigrateError>;

pub async fn migrate(
    request_context: Arc<RequestContext<Context>>,
    migrate_context: Arc<MigrateContext>,
    conn: Upgraded,
    log: slog::Logger,
) -> Result<()> {
    let mut proto = DestinationProtocol {
        request_context,
        migrate_context,
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

struct DestinationProtocol {
    request_context: Arc<RequestContext<Context>>,
    migrate_context: Arc<MigrateContext>,
    conn: Framed<Upgraded, codec::LiveMigrationFramer>,
    log: slog::Logger,
}

impl DestinationProtocol {
    fn start(&mut self) {
        info!(self.log, "Entering Destination Migration Task");
    }

    async fn sync(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::Sync).await;
        let preamble: Preamble = match self.read_msg().await? {
            codec::Message::Serialized(s) => {
                ron::de::from_str(&s).map_err(|_| MigrateError::Encoding)
            }
            _ => Err(MigrateError::Protocol),
        }?;
        info!(self.log, "Src read Preamble: {:?}", preamble);
        // XXX: For demonstration purposes only.
        if preamble.vm_descr.vcpus != vec![0u32, 1, 2, 3] {
            return Err(MigrateError::Protocol);
        }
        self.send_msg(codec::Message::Okay).await
    }

    async fn ram_push(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::RamPush).await;
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;
        // TODO(cross): Implement the rest of the RAM transfer protocol here. :-)
        let m = self.read_msg().await?;
        info!(self.log, "ram_push: got end {:?}", m);
        self.send_msg(codec::Message::MemDone).await
    }

    async fn device_state(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::Device).await;
        self.send_msg(codec::Message::Okay).await?;
        self.read_ok().await
    }

    async fn arch_state(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::Arch).await;
        self.send_msg(codec::Message::Okay).await?;
        self.read_ok().await
    }

    async fn ram_pull(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::RamPull).await;
        self.send_msg(codec::Message::MemQuery(0, !0)).await?;
        let m = self.read_msg().await?;
        info!(self.log, "ram_push: got end {:?}", m);
        self.send_msg(codec::Message::MemDone).await
    }

    async fn finish(&mut self) -> Result<()> {
        self.migrate_context.set_state(MigrationState::Finish).await;
        self.send_msg(codec::Message::Okay).await?;
        let _ = self.read_ok().await; // A failure here is ok.
        Ok(())
    }

    fn end(&mut self) {
        info!(self.log, "Destination Migration Successful");
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
