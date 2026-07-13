// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

pub mod control;
pub mod interrupt;

pub mod migrate {
    use serde::{Deserialize, Serialize};

    use super::control::migrate::ControlEndpointV1;
    use super::interrupt::migrate::InterruptInEndpointV1;

    #[derive(Serialize, Deserialize)]
    pub enum EndpointV1 {
        Control(ControlEndpointV1),
        InterruptIn(InterruptInEndpointV1),
    }
}
