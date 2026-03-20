// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;

use slog::{error, info, Logger};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use dice_verifier::sled_agent::AttestSledAgent;
use dice_verifier::Attest;

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

    async fn handle_conn(log: Logger, conn: TcpStream, sa_addr: SocketAddrV6) {
        info!(log, "handling connection");

        // Read in the request.
        const MAX_LINE_LENGTH: u32 = 1024;
        //let reader = BufReader::with_capacity(MAX_LINE_LENGTH, conn?);

        // XXX: these are hard-coded qualifying data values that match a test OS image
        // https://github.com/oxidecomputer/vm-attest-demo/blob/main/test-data/vm-instance-cfg.json
        let uuid = "db5bf54c-48c5-4455-a1e1-6c7dfc26e351";
        let img_digest =
            "be4df4e085175f3de0c8ac4837e1c2c9a34e8983209dac6b549e94154f7cdd9c";

        //let ox_attest: Box<dyn Attest> = Box::new(AttestSledAgent(sa_addr, log));

        //let response = match ox_attest.attest(&qualifying_data) {
        //Ok(a) =>
        //}
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
                            //tokio::spawn(Self::handle_conn(log.clone(), sock));
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
