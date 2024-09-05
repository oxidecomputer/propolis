// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2024 Oxide Computer Company

//! Types for tracking statistics about virtual disks.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

// NOTE: TOML definitions of timeseries are centralized in Omicron, so this file
// lives in that repo, at
// `./omicron/oximeter/oximeter/schema/virtual-machine.toml`.
oximeter::use_timeseries!("virtual-disk.toml");
use chrono::Utc;
use oximeter::{
    histogram::{Histogram, Record as _},
    types::Cumulative,
    MetricsError, Producer, Sample,
};
use propolis::block::{self, Operation};
use propolis_api_types::InstanceMetadata;
use uuid::Uuid;

use self::virtual_disk::{
    BytesRead, BytesWritten, FailedFlushes, FailedReads, FailedWrites, Flushes,
    IoLatency, IoSize, Reads, VirtualDisk, Writes,
};

/// Type for tracking virtual disk stats.
///
/// This is shared between the oximeter producer registry and the actual block
/// device that implements the I/O processing. As I/Os are completed, this is
/// called back into in order to update the stats. Note that the actual shared
/// type is an Arc+Mutex around this, the [`VirtualDiskProducer`].
#[derive(Debug)]
struct VirtualDiskStats {
    /// The oximeter::Target representing this disk.
    disk: VirtualDisk,
    /// Cumulative number of reads.
    reads: Reads,
    /// Cumulative number of bytes read.
    bytes_read: BytesRead,
    /// Cumulative number of failed reads, by failure reason.
    failed_reads: [FailedReads; N_FAILURE_KINDS],
    /// Cumulative number of writes.
    writes: Writes,
    /// Cumulative number of bytes written.
    bytes_written: BytesWritten,
    /// Cumulative number of failed writes, by failure reason.
    failed_writes: [FailedWrites; N_FAILURE_KINDS],
    /// Cumulative number of flushes.
    flushes: Flushes,
    /// Cumulative number of failed flushes, by failure reason.
    failed_flushes: [FailedFlushes; N_FAILURE_KINDS],
    /// Histogram tracking I/O latency, by the I/O kind.
    io_latency: [IoLatency; N_IO_KINDS],
    /// Histogram tracking I/O sizes, by the I/O kind.
    ///
    /// Note that we have 1 fewer histogram here, since flush operations do not
    /// have a size.
    io_size: [IoSize; N_IO_KINDS - 1],
}

impl VirtualDiskStats {
    /// Update the tracked statistics with the result of an I/O completion.
    fn on_completion(
        &mut self,
        op: block::Operation,
        result: block::Result,
        duration: Duration,
    ) {
        match op {
            Operation::Read(_, len) => {
                self.on_read_completion(result, len, duration)
            }
            Operation::Write(_, len) => {
                self.on_write_completion(result, len, duration)
            }
            Operation::Flush => self.on_flush_completion(result, duration),
        }
    }

    fn on_read_completion(
        &mut self,
        result: block::Result,
        len: usize,
        duration: Duration,
    ) {
        let index = match result {
            block::Result::Success => {
                let _ = self.io_latency[READ_INDEX]
                    .datum
                    .sample(duration.as_nanos() as u64);
                let _ = self.io_size[READ_INDEX].datum.sample(len as u64);
                self.reads.datum += 1;
                self.bytes_read.datum += len as u64;
                return;
            }
            block::Result::Failure => FAILURE_INDEX,
            block::Result::ReadOnly => READONLY_INDEX,
            block::Result::Unsupported => UNSUPPORTED_INDEX,
        };
        self.failed_reads[index].datum.increment();
    }

    fn on_write_completion(
        &mut self,
        result: block::Result,
        len: usize,
        duration: Duration,
    ) {
        let index = match result {
            block::Result::Success => {
                let _ = self.io_latency[WRITE_INDEX]
                    .datum
                    .sample(duration.as_nanos() as u64);
                let _ = self.io_size[WRITE_INDEX].datum.sample(len as u64);
                self.writes.datum += 1;
                self.bytes_written.datum += len as u64;
                return;
            }
            block::Result::Failure => FAILURE_INDEX,
            block::Result::ReadOnly => READONLY_INDEX,
            block::Result::Unsupported => UNSUPPORTED_INDEX,
        };
        self.failed_writes[index].datum.increment();
    }

    fn on_flush_completion(
        &mut self,
        result: block::Result,
        duration: Duration,
    ) {
        let index = match result {
            block::Result::Success => {
                let _ = self.io_latency[FLUSH_INDEX]
                    .datum
                    .sample(duration.as_nanos() as u64);
                self.flushes.datum += 1;
                return;
            }
            block::Result::Failure => FAILURE_INDEX,
            block::Result::ReadOnly => READONLY_INDEX,
            block::Result::Unsupported => UNSUPPORTED_INDEX,
        };
        self.failed_flushes[index].datum.increment();
    }
}

/// Number of I/O kinds we track.
const N_IO_KINDS: usize = 3;

/// Indices into arrays tracking operations broken out by I/O kind.
const READ_INDEX: usize = 0;
const WRITE_INDEX: usize = 1;
const FLUSH_INDEX: usize = 2;

/// String representations of I/O kinds we report to Oximeter.
const READ_KIND: &str = "read";
const WRITE_KIND: &str = "write";
const FLUSH_KIND: &str = "flush";

/// Number of failure kinds we track.
const N_FAILURE_KINDS: usize = 3;

/// Indices into arrays tracking operations broken out by failure kind.
const FAILURE_INDEX: usize = 0;
const READONLY_INDEX: usize = 1;
const UNSUPPORTED_INDEX: usize = 2;

/// String representations of failure kinds we report to Oximeter.
const FAILURE_KIND: &str = "failed";
const READONLY_KIND: &str = "read-only";
const UNSUPPORTED_KIND: &str = "unsupported";

/// Latency is measured in nanoseconds. We want to track between 1 microsecond,
/// which is 10 ** 3 nanos, and 10s, which is 10 * 1e9 == 10 ** 10 nanoseconds.
const LATENCY_POWERS: (u16, u16) = (3, 10);

/// Sizes are measured in powers of 2 from 512B to 1GiB.
///
/// We use 512B as the minimum since that is the minimum supported block size.
const SIZE_POWERS: (u16, u16) = (9, 30);

/// A [`Producer`] that emits statistics about virtual disks.
///
/// This type is shared between the block devie that handles guest I/Os, and the
/// oximeter producer server, which publishes data to oximeter when it polls us.
/// As I/Os are completed, the [`VirtualDiskProducer::on_completion()`] method
/// is called, to update stats with the results of the I/O operation.
///
/// As oximeter polls us, the producer server also collects these updated
/// statistics.
#[derive(Clone, Debug)]
pub struct VirtualDiskProducer {
    // Shareable inner type actually managing the stats.
    inner: Arc<Mutex<VirtualDiskStats>>,
}

impl VirtualDiskProducer {
    /// Create a producer to track a virtual disk.
    pub fn new(
        block_size: u32,
        instance_id: Uuid,
        disk_id: Uuid,
        metadata: &InstanceMetadata,
    ) -> Self {
        let disk = VirtualDisk {
            attached_instance_id: instance_id,
            block_size,
            disk_id,
            project_id: metadata.project_id,
            silo_id: metadata.silo_id,
        };
        let now = Utc::now();
        let datum = Cumulative::with_start_time(now, 0);
        let latency_histogram = Self::latency_histogram();
        let size_histogram = Self::size_histogram();
        let inner = VirtualDiskStats {
            disk,
            reads: Reads { datum },
            bytes_read: BytesRead { datum },
            failed_reads: [
                FailedReads { failure_reason: FAILURE_KIND.into(), datum },
                FailedReads { failure_reason: READONLY_KIND.into(), datum },
                FailedReads { failure_reason: UNSUPPORTED_KIND.into(), datum },
            ],
            writes: Writes { datum },
            bytes_written: BytesWritten { datum },
            failed_writes: [
                FailedWrites { failure_reason: FAILURE_KIND.into(), datum },
                FailedWrites { failure_reason: READONLY_KIND.into(), datum },
                FailedWrites { failure_reason: UNSUPPORTED_KIND.into(), datum },
            ],
            flushes: Flushes { datum },
            failed_flushes: [
                FailedFlushes { failure_reason: FAILURE_KIND.into(), datum },
                FailedFlushes { failure_reason: READONLY_KIND.into(), datum },
                FailedFlushes {
                    failure_reason: UNSUPPORTED_KIND.into(),
                    datum,
                },
            ],
            io_latency: [
                IoLatency {
                    io_kind: READ_KIND.into(),
                    datum: latency_histogram.clone(),
                },
                IoLatency {
                    io_kind: WRITE_KIND.into(),
                    datum: latency_histogram.clone(),
                },
                IoLatency {
                    io_kind: FLUSH_KIND.into(),
                    datum: latency_histogram.clone(),
                },
            ],
            io_size: [
                IoSize {
                    io_kind: READ_KIND.into(),
                    datum: size_histogram.clone(),
                },
                IoSize {
                    io_kind: WRITE_KIND.into(),
                    datum: size_histogram.clone(),
                },
            ],
        };
        Self { inner: Arc::new(Mutex::new(inner)) }
    }

    /// A callback that updates statistics with the result of a completed I/O.
    pub fn on_completion(
        &self,
        op: block::Operation,
        result: block::Result,
        duration: Duration,
    ) {
        self.inner.lock().unwrap().on_completion(op, result, duration);
    }

    /// Construct a histogram for tracking I/O latencies.
    ///
    /// This builds a "log-linear" histogram, which has 10 bins for each power
    /// of 10, spaced between 1 microsecond and 10s inclusive.
    fn latency_histogram() -> Histogram<u64> {
        // Safety: This only fails if the bins are not valid.
        Histogram::span_decades(LATENCY_POWERS.0, LATENCY_POWERS.1).unwrap()
    }

    /// Construct a histogram for tracking I/O sizes.
    ///
    /// This creates a power-of-2 histogram for tracking I/O sizes between 512B
    /// and 1GiB.
    fn size_histogram() -> Histogram<u64> {
        let bins: Vec<_> =
            (SIZE_POWERS.0..=SIZE_POWERS.1).map(|p| 1u64 << p).collect();

        // Safety: This only fails if the bins are not valid.
        Histogram::new(&bins).unwrap()
    }
}

impl Producer for VirtualDiskProducer {
    fn produce(
        &mut self,
    ) -> Result<Box<dyn Iterator<Item = Sample>>, MetricsError> {
        // 5 scalar samples (reads, writes, flushes, bytes read / written)
        // 3 scalars broken out by failure kind
        // 2 histograms broken out by I/O kind
        const N_SAMPLES: usize = 5 + 3 * N_FAILURE_KINDS + 2 * N_IO_KINDS;
        let mut out = Vec::with_capacity(N_SAMPLES);
        let inner = self.inner.lock().unwrap();

        // Read statistics.
        out.push(Sample::new(&inner.disk, &inner.reads)?);
        out.push(Sample::new(&inner.disk, &inner.bytes_read)?);
        for failed in inner.failed_reads.iter() {
            out.push(Sample::new(&inner.disk, failed)?);
        }

        // Write statistics.
        out.push(Sample::new(&inner.disk, &inner.writes)?);
        out.push(Sample::new(&inner.disk, &inner.bytes_written)?);
        for failed in inner.failed_writes.iter() {
            out.push(Sample::new(&inner.disk, failed)?);
        }

        // Flushes
        out.push(Sample::new(&inner.disk, &inner.flushes)?);
        for failed in inner.failed_flushes.iter() {
            out.push(Sample::new(&inner.disk, failed)?);
        }

        // Histograms for latency and size.
        for hist in inner.io_latency.iter() {
            out.push(Sample::new(&inner.disk, hist)?);
        }
        for hist in inner.io_size.iter() {
            out.push(Sample::new(&inner.disk, hist)?);
        }
        drop(inner);
        Ok(Box::new(out.into_iter()))
    }
}

#[cfg(test)]
mod test {
    use super::VirtualDiskProducer;
    use super::LATENCY_POWERS;
    use super::SIZE_POWERS;

    #[test]
    fn test_latency_histogram() {
        let hist = VirtualDiskProducer::latency_histogram();
        println!("{:#?}", hist.iter().map(|bin| bin.range).collect::<Vec<_>>());
        // The math here is a bit silly, but we end up with 9 bins in each
        // "interior" power of 10, plus one more bin on the right and left for
        // the bins from [0, 1us) and [10s, inf)
        assert_eq!(
            hist.n_bins(),
            (LATENCY_POWERS.1 - LATENCY_POWERS.0) as usize * 9 + 1 + 1
        );
    }

    #[test]
    fn test_size_histogram() {
        let hist = VirtualDiskProducer::size_histogram();
        println!("{:#?}", hist.iter().map(|bin| bin.range).collect::<Vec<_>>());
        // 1 extra left bin for [0, 512), and 1 because the range is inclusive.
        assert_eq!(
            hist.n_bins(),
            (SIZE_POWERS.1 - SIZE_POWERS.0) as usize + 1 + 1
        );
    }
}
