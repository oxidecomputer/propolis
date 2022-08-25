// Copyright 2022 Oxide Computer Company
//! Experimental progenitor-generated propolis-server API client.

progenitor::generate_api!(
    spec = "../../openapi/propolis-server.json",
    interface = Builder,
    tags = Separate,
);
