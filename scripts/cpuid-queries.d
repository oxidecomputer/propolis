#pragma D option defaultargs
#pragma D option quiet

/*
 * Report guest CPUID queries and returned leaves.
 *
 * Usage: ./cpuid-queries.d <target VM's struct vm*>
 *
 * You probably have a VM name you care about, not its `struct vm*`. How do you
 * go from the name to the pointer required here? And why?
 *
 * To the first question, something like this will get you the `vmm_vm` pointer
 * to use with this script:
 * ```
 * mdb -ke '::walk vmm | ::print struct vmm_softc vmm_vm vmm_name ! grep -B 1 <VM_NAME>'
 * ```
 *
 * Why? Because the VM's name is on vmm_softc, which has a reference to the
 * structure used in CPUID emulation and so on, but there's no back reference.
 * So this is slightly less convoluted than capturing the `struct vm` pointer at
 * some earlier point when we still have a softc. This may well get simplified
 * with a backref in the future.
 */

BEGIN {
	if ($$1 == "") {
		printf("target `struct vm*` required");
		exit(1);
	}

	target_vm = $1;
}

fbt::vcpu_emulate_cpuid:entry / arg0 == target_vm / {
	self->interested = 1;
	self->rax = (uint64_t*)arg2;
	self->rbx = (uint64_t*)arg3;
	self->rcx = (uint64_t*)arg4;
	self->rdx = (uint64_t*)arg5;
	self->leaf = *self->rax;
	self->subleaf = *self->rcx;
}

fbt::vcpu_emulate_cpuid:return / self->interested == 1 / {
	printf("CPUID query: leaf/subleaf 0x%08x/0x%08x, returns rax = 0x%08x, rbx = 0x%08x, rcx = 0x%08x, rdx = 0x%08x\n",
		self->leaf,
	 	self->subleaf,
		*self->rax,
		*self->rbx,
		*self->rcx,
		*self->rdx
	);
	self->interested = 0;
}
