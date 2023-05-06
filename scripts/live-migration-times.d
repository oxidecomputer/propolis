#!/usr/sbin/dtrace -s

/*
 * Measures the length of individual phases of the propolis-server live
 * migration protocol.
 *
 * Usage: ./live-migration-times.d <propolis-server PID> [v]
 *
 * Use "v" for more verbose output.
 *
 *
 * Some implementation notes:
 * - A pid is required because multiple migrations might be running on the same
 *   machine. It's possible to bifurcate the data here based on pid using
 *   aggregations, but the tradeoff is that formatting output is a bit more
 *   difficult. So for now, we require a pid.
 * - This script relies on the fact that each phase in the migration will only
 *   fire the migrate_phase_{begin,end} probes once. If our architecture
 *   changes, this script might break.
 * - We also assume that each phase has a unique name, which is passed as an
 *   arugment into the migrate_phase_{begin,end} probes. We use the name as
 *   a key for tracking the phase deltas. If those names change, or phases are
 *   added/removed, this script will break.
 */

#pragma D option quiet
#pragma D option defaultargs

enum vm_paused {
	VM_UNPAUSED = 0,
	VM_PAUSED = 1,
};

uint64_t xfer_pages[uint8_t];
uint64_t xfer_bytes[uint8_t];

dtrace:::BEGIN
{
	if ($$1 == "") {
		printf("ERROR: propolis-server pid required\n");
		exit(1);
	}

	printf("tracing live migration protocol times for pid %d...\n", $1);
	printf("\n");

	if ($$2 == "v") {
		printf("%-12s %-10s %30s\n", "PHASE", "", "TIMESTAMP");
	}

	this->phase = "";
}

propolis$1:::migrate_xfer_ram_page
{
	xfer_pages[arg2]++;
	xfer_bytes[arg2] += arg1;
}

propolis$1:::migrate_phase_begin
{
	this->phase = copyinstr(arg0);
	start_times[this->phase] = timestamp;

	if ($$2 == "v") {
		printf("%-12s %-10s %30d\n", this->phase, "BEGIN",
		    start_times[this->phase]);
	}
}

propolis$1:::migrate_phase_end
{
	this->phase = copyinstr(arg0);
	this->start = start_times[this->phase];
	this->end = timestamp;

	if (this->start != 0) {
		delta[this->phase] = this->end - this->start;
	} else {
		printf("WARNING: phase \"%s\" could not be measured\n",
		    this->phase);
	}

	if ($$2 == "v") {
		printf("%-12s %-10s %30d\n", this->phase, "END",
		    this->end);
	}
}

dtrace:::END
{
	this->sync = "Sync";
	this->rpush_pre = "RamPushPrePause";
	this->pause = "Pause";
    this->rpush_post = "RamPushPostPause";
	this->dev = "DeviceState";
	this->rpull = "RamPull";
	this->fin = "Finish";

	this->d_sync = delta[this->sync];
	this->d_rpush_pre = delta[this->rpush_pre];
	this->d_pause = delta[this->pause];
	this->d_rpush_post = delta[this->rpush_post];
	this->d_dev = delta[this->dev];
	this->d_rpull = delta[this->rpull];
	this->d_fin = delta[this->fin];

	this->total = 0;

	/* Print header */
	if ($$1 != "") {
		printf("\n\n");
		printf("%-15s %30s\n", "PHASE", "TIME ELAPSED (usec)");
	}

	/* Print the values of each phase, if they occurred */
	if (this->d_sync != 0) {
		printf("%-16s %29d\n", this->sync, this->d_sync / 1000);
		this->total += this->d_sync;
	}
	if (this->d_rpush_pre != 0) {
		printf("%-16s %29d\n", this->rpush_pre, this->d_rpush_pre / 1000);
		this->total += this->d_rpush_pre;
	}
	if (this->d_pause != 0) {
		printf("%-16s %29d\n", this->pause, this->d_pause / 1000);
		this->total += this->d_pause;
	}
	if (this->d_rpush_post != 0) {
		printf("%-16s %29d\n", this->rpush_post, this->d_rpush_post / 1000);
		this->total += this->d_rpush_post;
	}
	if (this->d_dev != 0) {
		printf("%-16s %29d\n", this->dev, this->d_dev / 1000);
		this->total += this->d_dev;
	}
	if (this->d_rpull != 0) {
		printf("%-16s %29d\n", this->rpull, this->d_rpull / 1000);
		this->total += this->d_rpull;
	}
	if (this->d_fin != 0) {
		printf("%-16s %29d\n", this->fin, this->d_fin / 1000);
		this->total += this->d_fin;
	}

	/* Print total elapsed time */
	if ($$1 != "") {
		printf("%-15s %30d\n", "TOTAL", this->total / 1000);
		printf("\n");
	}

	xfer_pages_total = xfer_pages[VM_PAUSED] + xfer_pages[VM_UNPAUSED];
	xfer_bytes_total = xfer_bytes[VM_PAUSED] + xfer_bytes[VM_UNPAUSED];

	/* Print summary of RAM pages transferred */
	if ($$1 != "" && xfer_pages_total != 0) {
		printf("%-25s %20d\n", "NPAGES XFERED (total)", xfer_pages_total);
		printf("%-25s %20d\n", "NBYTES XFERED (total)", xfer_bytes_total);

		printf("%-25s %20d\n",
				"NPAGES XFERED (unpaused)",
				xfer_pages[VM_UNPAUSED]);
		printf("%-25s %20d\n",
				"NBYTES XFERED (unpaused)",
				xfer_bytes[VM_UNPAUSED]);
		if (this->d_rpush_pre != 0 && xfer_bytes[VM_UNPAUSED] != 0) {
			printf("%-25s %20d\n", "KiB/SEC (unpaused)",
					((xfer_bytes[VM_UNPAUSED] * 1000000000) / 1024) /
						this->d_rpush_pre);
		}

		printf("%-25s %20d\n",
				"NPAGES XFERED (paused)",
				xfer_pages[VM_PAUSED]);
		printf("%-25s %20d\n",
				"NBYTES XFERED (paused)",
				xfer_bytes[VM_PAUSED]);
		if (this->d_rpush_post != 0 && xfer_bytes[VM_PAUSED] != 0) {
			printf("%-25s %20d\n", "KiB/SEC (paused)",
					((xfer_bytes[VM_PAUSED] * 1000000000) / 1024) /
						this->d_rpush_post);
		}
	}
}
