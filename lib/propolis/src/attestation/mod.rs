// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use slog::{info, error, Logger};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

// See: https://github.com/oxidecomputer/oana
pub const ATTESTATION_PORT: u16 = 605;
pub const ATTESTATION_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), ATTESTATION_PORT);

pub struct AttestationSock {
    join_hdl: JoinHandle<()>,
    hup_send: oneshot::Sender<()>,
}

// TODO:
//

impl AttestationSock {
    pub async fn new(log: Logger) -> io::Result<Self> {
        info!(log, "attestation server created");
        let listener = TcpListener::bind(ATTESTATION_ADDR).await?;
        let (hup_send, hup_recv) = oneshot::channel::<()>();
        let join_hdl = tokio::spawn(async move {
            Self::run(log, listener, hup_recv).await;
        });
        Ok(Self { join_hdl, hup_send })
    }

    pub async fn halt(self) {
        let Self { join_hdl, hup_send } = self;

        // Signal the socket listener to hang up, then wait for it to bail
        let _ = hup_send.send(());
        let _ = join_hdl.await;
    }

    async fn handle_conn(log: Logger, conn: TcpStream) {
        info!(log, "handling connection");
        // TODO
    }

    pub async fn run(
        log: Logger,
        listener: TcpListener,
        mut hup_recv: oneshot::Receiver<()>,
    ) {
        info!(log, "attestation server running");

        loop {
            tokio::select! {
                biased;

                _ = &mut hup_recv => {
                    return;
                },
                sock_res = listener.accept() => {
                    info!(log, "new client connected");
                    match sock_res {
                        Ok((sock, addr)) => {
                            tokio::spawn(Self::handle_conn(log.clone(), sock));
                        }
                        Err(e) => {
                            error!(log, "Attestation TCP listener error: {:?}", e);
                        }
                    }
                },
            };
        }
    }
}
