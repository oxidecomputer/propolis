# Propolis Scripts

This is a collection of assorted scripts which may be useful for certain
development activities, such as observing aspects of propolis performance. See
individual files for details.

## Scripts

- `live-migration-times.d`: Measure the length of individual phases of live
  migration on a running propolis-server.
- `nvme_trace.d`: Measure propolis-emulated NVMe read/write latency.
- `time_adjustments.d`: Observe guest timing data adjustments on the target host
  of a live migration.
- `vm_exit_codes.d`: Measure VM exits and information about them both for
  #VMEXIT events and returns to Propolis.
