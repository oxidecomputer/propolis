#!/usr/sbin/dtrace -s

/*
 * Time bhyve operations related to VM setup and teardown.
 *
 * Usage: ./lifecycle-times.d
 *
 * This does not take a particular target Propolis PID; I found it more useful
 * to measure all 0-2 propolis-{server,standalone} on my test system. If you
 * wanted to measure these times on a sled, the target propolis-server will be
 * started by sled-agent and we can't predict the PID anyway!
 *
 * This could reasonably measure *propolis*-side time for VM lifecycle
 * operations, in the future!
 */

BEGIN {
	NEW_FAULTS = 0;
}

fbt::vm_mmap_memseg:entry {
	self->gpa = arg1;
	self->len = arg4;
	self->prot = arg5;
	self->flags = arg6;
	self->mapstart = timestamp;
}

fbt::vm_mmap_memseg:return {
	self->maptime = timestamp - self->mapstart;
	printf("mapped %12x bytes as %x at gpa %012p in %dus\n",
	    self->len,
	    self->prot,
	    self->gpa,
	    self->maptime / 1000);

	self->gpa = 0;
	self->len = 0;
	self->prot = 0;
	self->flags = 0;
	self->mapstart = 0;
}

/*
 * vmc_fault() is used in service of establishing the PTEs for a guest's
 * physical memory, if we've lazily mapped guest physical memory. If guest
 * physical memory is "wired", you won't see this happen at all; in that case
 * the NPT's PTEs were eagerly populated before the VM was even running (and
 * `vm_mmap_memseg` was somewhat slower as a result).
 */
fbt::vmc_fault:entry {
	self->faultstart = timestamp;
}

fbt::vmc_fault:return {
	self->faulttime = timestamp - self->faultstart;

	@fault_hist["ns"] = quantize(self->faulttime);
	@fault_total["ns"] = sum(self->faulttime);

	NEW_FAULTS += 1;
}

fbt::vm_cleanup:entry {
	self->cleanupstart = timestamp;
}

fbt::vm_cleanup:return {
	self->cleanuptime = timestamp - self->cleanupstart;
	printf("vm_cleanup took %dus\n", self->cleanuptime / 1000);
}

tick-1s / NEW_FAULTS / {
	printa(@fault_hist);
	printa("total time in vmc_fault: %@dns\n", @fault_total);
	NEW_FAULTS = 0;
}
