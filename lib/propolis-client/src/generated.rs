// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Experimental progenitor-generated propolis-server API client.

progenitor::generate_api!(
    spec = "../../openapi/propolis-server.json",
    interface = Builder,
    tags = Separate,
);
