// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Handle to the OPTE port backing an XDE-class viona device.
pub struct OptePort {
    port_name: String,

    // TODO remove once we have a real backend? do we need to track anything
    // else here? (handle to the xde device to issue ioctls?)
    log: slog::Logger,
}

impl OptePort {
    pub fn new(port_name: String, log: &slog::Logger) -> Self {
        let log = log.new(slog::o!("port" => port_name.clone()));
        Self { port_name, log }
    }
}
