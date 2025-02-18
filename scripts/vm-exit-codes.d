#pragma D option quiet

/*
 * Report reasons why a VM's vCPU has exited, and exits out to the VMM.
 *
 * A vCPU's exits are often handled entirely in bhyve, with the VMM unaware, and
 * an exit to VMM may not be the direct consequence of a VM exit. An exit to VMM
 * likely will only occur after at least one VMEXIT, though, when other
 * conditions are re-evaluated before resuming the vCPU. This script measures
 * and reports both SVM VMEXIT and exits to VMM because while separate they are
 * likely related in some way.
 */

/*
 * From AMD APM Volume 2 Appendix C: "SVM Intercept Exit Codes"
 *
 * Subset of exit codes this script is particularly interested in.
 */
enum svm_exitcode {
	VMEXIT_INTR = 0x60,
	VMEXIT_VINTR = 0x64,
	VMEXIT_CPUID = 0x72,
	VMEXIT_IOIO = 0x7b,
	VMEXIT_NPF = 0x400
};

/*
 * Exit codes that a VMM may receive after running a vCPU, taken from
 * illumos' `intel/sys/vmm.h`.
 */
enum vm_exitcode {
	VM_EXITCODE_INOUT,
	VM_EXITCODE_VMX,
	VM_EXITCODE_BOGUS,
	VM_EXITCODE_RDMSR,
	VM_EXITCODE_WRMSR,
	VM_EXITCODE_HLT,
	VM_EXITCODE_MTRAP,
	VM_EXITCODE_PAUSE,
	VM_EXITCODE_PAGING,
	VM_EXITCODE_INST_EMUL,
	VM_EXITCODE_RUN_STATE,
	VM_EXITCODE_MMIO_EMUL,
	VM_EXITCODE_DEPRECATED,	/* formerly RUNBLOCK */
	VM_EXITCODE_IOAPIC_EOI,
	VM_EXITCODE_SUSPENDED,
	VM_EXITCODE_MMIO,
	VM_EXITCODE_TASK_SWITCH,
	VM_EXITCODE_MONITOR,
	VM_EXITCODE_MWAIT,
	VM_EXITCODE_SVM,
	VM_EXITCODE_DEPRECATED2, /* formerly REQIDLE */
	VM_EXITCODE_DEBUG,
	VM_EXITCODE_VMINSN,
	VM_EXITCODE_BPT,
	VM_EXITCODE_HT,
	VM_EXITCODE_MAX
};

BEGIN {
	misunderstood_exits = 0;
}

fbt::vm_run:entry {
	/*
	 * Some functions of interest here are only interesting when called
	 * under vm_run, but may be called elsewhere as well. Keep track of
	 * if we're in vm_run to scope other probes correspondingly.
	 */
	self->in_vm_run = 1;
	/*
	 * Assuming we'll exit vm_run at some point, presume we don't know
	 * why that exit occurred. We'll flip this to true in cases the script
	 * knows about. Any exits that are not understood are a sign the script
	 * is stale, the kernel has changed, or both.
	 */
	self->exit_understood = 0;
	self->next_exit_reason = "unknown";
}

fbt::svm_launch:return {
	self->vcpu = (struct svm_vcpu *)NULL;
}

fbt::svm_vmexit:entry {
	self->vcpu = &((struct svm_softc*)arg0)->vcpu[arg1];
	self->ctrl = self->vcpu->vmcb.ctrl;
	self->state = self->vcpu->vmcb.state;
	self->vmexit = (struct vm_exit*)arg2;

	@exits[self->ctrl.exitcode] = count();

	if (self->ctrl.exitcode == VMEXIT_IOIO) {
		this->opsz = (self->ctrl.exitinfo1 >> 4) & 7;
		this->addrsz = (self->ctrl.exitinfo1 >> 7) & 7;
		@io_info[
		  self->ctrl.exitinfo1 >> 16,
		  self->ctrl.exitinfo1 & 1 == 0 ? "out" : "in",
		  this->opsz == 1 ? "8b" :
		    this->opsz == 2 ? "16b" :
		      this->opsz == 4 ? "32b" : "bogus",
		  this->addrsz == 1 ? "16b" :
		    this->addrsz == 2 ? "32b" :
		      this->addrsz == 4 ? "64b" : "bogus"
		] = count();
	}

	if (self->ctrl.exitcode == VMEXIT_NPF) {
		@npf_info[
			self->ctrl.exitinfo2,
			self->state.rip,
			/*
			 * Instruction/Data access
			 */
			(self->ctrl.exitinfo1 >> 4) & 1 ? "I" : "D",
			/*
			 * Processor read 1 in a PTE's reserved bits
			 */
			(self->ctrl.exitinfo1 >> 3) & 1 ? "R" : "-",
			/*
			 * User/Supervisor (CPL=3 or not 3)
			 */
			(self->ctrl.exitinfo1 >> 2) & 1 ? "U" : "S",
			/*
			 * Access is write or read
			 */
			(self->ctrl.exitinfo1 >> 1) & 1 ? "W" : "R",
			/*
			 * Page is present or not
			 */
			(self->ctrl.exitinfo1 >> 0) & 1 ? "P" : "-"
		] = count();
	}
}

fbt::vcpu_entry_bailout_checks:return / self->in_vm_run == 1 /{
	if (arg1 != 0) {
		self->exit_understood = 1;
		self->next_exit_reason = "early_bailout";
	}
}

fbt::vcpu_run_state_pending:return / self->in_vm_run == 1 /{
	if (arg1 != 0) {
		self->exit_understood = 1;
		self->next_exit_reason = "run_state_pending";
	}
}

fbt::vm_run:return / self->in_vm_run == 1 && self->vmexit != NULL / {
	self->in_vm_run = 0;

	if (!self->exit_understood) {
		misunderstood_exits += 1;
	}
	if (self->vmexit->exitcode == VM_EXITCODE_BOGUS) {
		@bogus_reasons[self->next_exit_reason] = count();
	}
}

tick-1s {
	printf("=== Exit summary, one second ending at %Y ===\n",
	    walltimestamp);
	printf("  %8s  %s\n", "SVM code", "Count");
	printa("  %8x  %@8u\n", @exits);

	printf("IOIO SVM exits:\n");
	printf("  %4s  %3s  %4s  %4s  %5s\n",
	    "Port", "Op", "OpSz", "Addr", "Count");
	printa("  %4x  %3s  %4s  %4s  %@5d\n", @io_info);

	printf("NPF SVM exits:\n");
	printf("  %-16s  %-16s  %5s  %s\n",
	    "Guest PA", "Guest RIP", "#PF flags", "Count");
	printa("  %16x  %16x  %5s%s%s%s%s  %@8u\n", @npf_info);

	printf("vm_run() VM_EXITCODE_BOGUS reasons:\n");
	printa("  %20s %@8u\n", @bogus_reasons);

	if (misunderstood_exits > 0) {
		printf("Exits this script did not understand: %d\n",
		    misunderstood_exits);
		misunderstood_exits = 0;
	}

	printf("\n");

	/*
	 * Clear all accumulated data, but keep the most common keys to churn a
	 * little less if there is relatively little activity and a key flips
	 * from zero to non-zero counts regularly.
	 */
	trunc(@exits, 10);
	clear(@exits);
	trunc(@io_info, 10);
	clear(@io_info);
	trunc(@npf_info, 10);
	clear(@npf_info);
	trunc(@bogus_reasons, 10);
	clear(@bogus_reasons);
}
