use futures::{SinkExt, StreamExt};
use std::sync::Arc;

use dropshot::RequestContext;
use hyper::upgrade::Upgraded;
use slog::info;
use tokio_util::codec::Framed;

use crate::migrate::codec;
use crate::migrate::{MigrateContext, MigrateError, MigrationState};
use crate::server::Context;

pub async fn migrate(
    _request_context: Arc<RequestContext<Context>>,
    migrate_context: Arc<MigrateContext>,
    conn: Upgraded,
    log: slog::Logger,
) -> Result<(), MigrateError> {
    info!(log, "Enter Migrate Task");

    let mut framer = Framed::new(conn, codec::LiveMigrationFramer::new());

    // TODO: actual migration protocol, for now just send some stuff back and forth
    for _x in 0..10 {
        let v = vec![0u8, 1, 2, 3, 4, 5, 6, 7, 8];
        let b = codec::Message::Blob(v);
        framer.send(b).await.map_err(|_| MigrateError::Protocol)?;
        let read =
            framer.next().await.unwrap().map_err(|_| MigrateError::Protocol)?;
        info!(log, "Src Read: {:?}", read);
        //assert_eq!(read, b);
    }

    // Random state demonstration
    migrate_context.set_state(MigrationState::Device).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // for x in 10..20 {
    //     let read = conn.read_u32().await.map_err(|_| MigrateError::Protocol)?;
    //     info!(log, "Dest Read: {:?}", read);
    //     assert_eq!(read, x);
    // }

    // More random state demonstration
    migrate_context.set_state(MigrationState::Resume).await;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    migrate_context.set_state(MigrationState::Finish).await;

    info!(log, "Migrate Successful");

    Ok(())
}
