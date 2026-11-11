#!/usr/sbin/dtrace -qCs

#pragma D option defaultargs
#pragma D option quiet

/*
 * Report accesses to a guest's xAPIC and x2APIC spaces.
 *
 * Usage: ./apic.d
 *
 * Select register accesses (such as the xAPIC ICR) are printed in more verbose
 * manners. This hopefully covers any registers of concern, but for missed cases
 * this script will print out a fallback line about a `wrmsr`, `rdmsr`, or
 * `mmio`. This currently measures any VM's xAPIC/x2APIC accesses on the
 * system, so it can be quite noisy with multiple active VMs.
 */

BEGIN {
	/*
	 * Get this from `mdb -ke 'tsc_freq/E'` - this happens to be the tick
	 * rate for the TSC on my 7950x.
	 */
	TICKS_PER_SEC = 4499488615;
	TICKS_PER_US = TICKS_PER_SEC / 1000000;
	START = timestamp;
}

/*
 * > The MSR address range 800H through BFFH is architecturally reserved and
 * > dedicated for accessing APIC registers in x2APIC mode
 */

fbt::vm_handle_wrmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x802 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) write: local APIC ID <- %08x\n",
	    timestamp - START, arg2, ((struct vm_exit*)arg2)->u.msr.wval);
}

fbt::vm_handle_wrmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x803 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) write: local APIC version <- %08x\n",
	    timestamp - START, arg2, ((struct vm_exit*)arg2)->u.msr.wval);
}

fbt::vm_handle_wrmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x80B / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) write: EOI register <- %08x\n",
	    timestamp - START, arg2, ((struct vm_exit*)arg2)->u.msr.wval);
}

fbt::vm_handle_wrmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x830 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) write: Interrupt Command Register <- %08x\n",
	    timestamp - START, arg2, ((struct vm_exit*)arg2)->u.msr.wval);
}

fbt::vm_handle_wrmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x838 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) write: Initial Count Register <- %08x\n",
	    timestamp - START, arg2, ((struct vm_exit*)arg2)->u.msr.wval);
}

fbt::vm_handle_wrmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x839 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) write: Current Count Register <- %08x\n",
	    timestamp - START, arg2, ((struct vm_exit*)arg2)->u.msr.wval);
}

fbt::vm_handle_wrmsr:entry / ((struct vm_exit*)arg2)->u.msr.code > 0x300 / {
	if (!self->handled) {
		printf("wrmsr %x <- %x\n",
		    ((struct vm_exit*)arg2)->u.msr.code,
		    ((struct vm_exit*)arg2)->u.msr.wval
		);
	}

	self->handled = 0;
}

fbt::vm_handle_rdmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x802 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) read: local APIC ID\n",
	    timestamp - START, arg2);
}

fbt::vm_handle_rdmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x803 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) read: local APIC version\n",
	    timestamp - START, arg2);
}

fbt::vm_handle_rdmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x80B / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) read: EOI register\n",
	    timestamp - START, arg2);
}

fbt::vm_handle_rdmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x830 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) read: Interrupt Command Register\n",
	    timestamp - START, arg2);
}

fbt::vm_handle_rdmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x838 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) read: Initial Count Register\n",
	    timestamp - START, arg2);
}

fbt::vm_handle_rdmsr:entry / ((struct vm_exit*)arg2)->u.msr.code == 0x839 / {
	self->handled = 1;
	printf("x2apic t=%d (vcpu=%d) read: Current Count Register\n",
	    timestamp - START, arg2);
}

fbt::vm_handle_rdmsr:entry / ((struct vm_exit*)arg2)->u.msr.code > 0x300 / {
	if (!self->handled) {
		printf("rdmsr %x\n",
		    ((struct vm_exit*)arg2)->u.msr.code
		);
	}

	self->handled = 0;
}

fbt::vie_mmio_read:entry / arg3 == 0xfee00020 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: local APIC ID (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / arg3 == 0xfee00020 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: local APIC Version (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / arg3 == 0xfee00080 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: Task Priority Register (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / arg3 == 0xfee00090 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: Arbitration Priority Register (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / arg3 == 0xfee000a0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: Processor Priority Register (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / arg3 == 0xfee000b0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: EOI Register (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / arg3 == 0xfee000d0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: Remote Read Register (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / arg3 == 0xfee000e0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: Logical Destination Register (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / arg3 == 0xfee000f0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) read: Destination Format Register (%d bytes)\n",
	    timestamp - START, arg2, arg5);
}

fbt::vie_mmio_read:entry / (arg3 & 0xffff0000) == 0xfee00000 / {
	if (!self->handled) {
	  printf("xapic t=%d (vcpu=%d) read: %d bytes at %p\n",
		    timestamp - START, arg2, arg5, arg3);
	}
	self->handled = 0;
}

fbt::vie_mmio_write:entry / arg3 == 0xfee00020 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: local APIC ID <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee00020 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: local APIC Version <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee00080 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: Task Priority Register <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee00090 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: Arbitration Priority Register <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee000a0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: Processor Priority Register <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee000b0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: EOI Register <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee000d0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: Remote Read Register <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee000e0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: Logical Destination Register <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee000f0 / {
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: Destination Format Register <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    arg5,
	    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee00310 / {
	self->icr_hi = arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)));
	self->handled = 1;
	printf("xapic t=%d (vcpu=%d) write: ICR_HI <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    self->icr_hi,
	    arg5
	);
}

/*
 * From the SDM:
 *
 * > The act of writing to the low doubleword of the ICR causes the IPI to be sent.
 */
fbt::vie_mmio_write:entry / arg3 == 0xfee00300 / {
	self->handled = 1;
	self->icr_lo = arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)));
	printf("xapic t=%d (vcpu=%d) write: ICR_LO <- %x (%d bytes)\n",
	    timestamp - START,
	    arg2,
	    self->icr_lo,
	    arg5
	);
	self->icr = self->icr_lo | (self->icr_hi << 32);
	self->destination = self->icr >> 56;
	self->shorthand = (self->icr >> 18) & 0x3;
	self->level = (self->icr >> 15) & 1;
	self->l_assert = (self->icr >> 14) & 1;
	self->send_pending = (self->icr >> 12) & 1;
	self->logical = (self->icr >> 11) & 1;
	self->mode = (self->icr >> 8) & 0x7;
	self->vector = (self->icr) & 0xff;
	printf("xapic t=%d (vcpu=%d) ICR write: vector=%d, destination=%d (%s)\n",
	    timestamp - START,
	    arg2,
	    self->vector,
	    self->destination,
	    self->logical ? "logical" : "physical"
	);

	self->modename = self->mode > 3 ?
			(self->mode > 5 ?
					(self->mode == 6 ? "Start Up" : "Reserved") :
					(self->mode == 4 ? "NMI" : "INIT")
			) :
			(self->mode > 1 ?
					(self->mode == 2 ? "SMI" : "Reserved") :
					(self->mode == 0 ? "Fixed" : "Lowest Pri")
			);

	printf("    delivery mode 0x%x (%s) shorthand=%x, status=%s, level=%s, trigger=%s\n",
	    self->mode, self->modename,
			self->shorthand,
			self->l_assert ? "assert" : "de-assert",
			self->send_pending ? "Send Pending" : "Idle",
			self->level ? "Level" : "Edge"
	);
}

fbt::vie_mmio_write:entry / arg3 == 0xfee00380 / {
	self->handled = 1;
	self->value = arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)));
	self->us = self->value / TICKS_PER_US;
	self->rem = self->value % TICKS_PER_US;
	self->frac = (TICKS_PER_US * 100) / self->rem;
	printf("xapic t=%d (vcpu=%d) write: Initial Count Register <- %d (%d bytes, %d.%02dus)\n",
	    timestamp - START,
	    arg2,
	    self->value,
	    arg5,
	    self->us, self->frac
	);
}

fbt::vie_mmio_write:entry / (arg3 & 0xffff0000) == 0xfee00000 / {
	if (!self->handled) {
		printf("mmio write: %d bytes at %p: %x\n",
		    arg5,
		    arg3,
		    arg4 & (0xffffffffffffffff >> (8 * (8 - arg5)))
		);
	}
	self->handled = 0;
}
