# bhyve-api

This crate exposes the interfaces from the bhyve kernel VMM.  Since those
interfaces are Private and subject to change, it means this crate must be kept
in sync with changes, and Propolis binaries built against a given version of
the crate (and implied interface version) will only run on systems with that
exact OS version.
