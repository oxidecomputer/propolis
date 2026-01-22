#!/usr/sbin/dtrace -qCs

#pragma D option quiet

/*
 * Dtrace ioctl calls into the viona kernel module as they occur, and show
 * in-kernel ring notifications.
 *
 * This script needs the `viona` module to be loaded. If you want to run it
 * before starting a VM utter: `modload /usr/kernel/drv/amd64/viona`
 */

#define	VNA_IOC	(('V' << 16)|('C' << 8))
#define	VNA_IOC_CREATE			(VNA_IOC | 0x01)
#define	VNA_IOC_DELETE			(VNA_IOC | 0x02)
#define	VNA_IOC_VERSION			(VNA_IOC | 0x03)
#define	VNA_IOC_DEFAULT_PARAMS		(VNA_IOC | 0x04)

#define	VNA_IOC_RING_INIT		(VNA_IOC | 0x10)
#define	VNA_IOC_RING_RESET		(VNA_IOC | 0x11)
#define	VNA_IOC_RING_KICK		(VNA_IOC | 0x12)
#define	VNA_IOC_RING_SET_MSI		(VNA_IOC | 0x13)
#define	VNA_IOC_RING_INTR_CLR		(VNA_IOC | 0x14)
#define	VNA_IOC_RING_SET_STATE		(VNA_IOC | 0x15)
#define	VNA_IOC_RING_GET_STATE		(VNA_IOC | 0x16)
#define	VNA_IOC_RING_PAUSE		(VNA_IOC | 0x17)
#define	VNA_IOC_RING_INIT_MODERN	(VNA_IOC | 0x18)

#define	VNA_IOC_INTR_POLL		(VNA_IOC | 0x20)
#define	VNA_IOC_SET_FEATURES		(VNA_IOC | 0x21)
#define	VNA_IOC_GET_FEATURES		(VNA_IOC | 0x22)
#define	VNA_IOC_SET_NOTIFY_IOP		(VNA_IOC | 0x23)
#define	VNA_IOC_SET_PROMISC		(VNA_IOC | 0x24)
#define	VNA_IOC_GET_PARAMS		(VNA_IOC | 0x25)
#define	VNA_IOC_SET_PARAMS		(VNA_IOC | 0x26)
#define	VNA_IOC_GET_MTU			(VNA_IOC | 0x27)
#define	VNA_IOC_SET_MTU			(VNA_IOC | 0x28)
#define	VNA_IOC_SET_NOTIFY_MMIO		(VNA_IOC | 0x29)
#define	VNA_IOC_INTR_POLL_MQ		(VNA_IOC | 0x2a)

#define	VNA_IOC_GET_PAIRS		(VNA_IOC | 0x30)
#define	VNA_IOC_SET_PAIRS		(VNA_IOC | 0x31)
#define	VNA_IOC_GET_USEPAIRS		(VNA_IOC | 0x32)
#define	VNA_IOC_SET_USEPAIRS		(VNA_IOC | 0x33)

BEGIN {
	printf("Tracing...\n");
}

viona_ioctl:entry {
	self->dptr = arg2;
	self->rvp = args[5];
}

viona_ioctl:entry/arg1 == VNA_IOC_CREATE/ {
	self->cmd = "CREATE";
	create = (vioc_create_t *)copyin(arg2, sizeof (vioc_create_t));
	printf("%s (link 0x%x)\n", self->cmd, create->c_linkid);
}

viona_ioctl:entry/arg1 == VNA_IOC_DELETE/ {
	self->cmd = "DELETE";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_VERSION/ {
	self->cmd = "VERSION";
	self->pending = arg1;
}

viona_ioctl:return/self->pending == VNA_IOC_VERSION/ {
	printf("VERSION 0x%x\n", *self->rvp);
}

viona_ioctl:entry/arg1 == VNA_IOC_DEFAULT_PARAMS/ {
	self->cmd = "DEFAULT_PARAMS";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_INIT/ {
	self->cmd = "RING_INIT";
	init = (vioc_ring_init_t *)copyin(arg2, sizeof (vioc_ring_init_t));
	printf("%s 0x%x %x %x\n", self->cmd,
	    init->ri_index, init->ri_qaddr, init->ri_qsize);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_INIT_MODERN/ {
	self->cmd = "RING_INIT_MODERN";
	initm = (vioc_ring_init_modern_t *)copyin(arg2,
	    sizeof (vioc_ring_init_modern_t));
	printf("%s 0x%x %x/%x/%x %x\n", self->cmd,
	    initm->rim_index, initm->rim_qaddr_desc, initm->rim_qaddr_avail,
	    initm->rim_qaddr_used, initm->rim_qsize);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_RESET/ {
	self->cmd = "RING_RESET";
	printf("%s 0x%x\n", self->cmd, arg2);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_KICK/ {
	self->cmd = "RING_KICK";
	printf("%s 0x%x\n", self->cmd, arg2);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_SET_MSI/ {
	self->cmd = "RING_SET_MSI";
	msi = (vioc_ring_msi_t *)copyin(arg2, sizeof (vioc_ring_msi_t));
	printf("%s 0x%x addr=%x msg=%x\n", self->cmd,
	    msi->rm_index, msi->rm_addr, msi->rm_msg);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_INTR_CLR/ {
	self->cmd = "RING_INTR_CLR";
	printf("%s 0x%x\n", self->cmd, arg2);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_SET_STATE/ {
	self->cmd = "RING_SET_STATE";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_GET_STATE/ {
	self->cmd = "RING_GET_STATE";
	self->pending = arg1;
}

viona_ioctl:return/self->pending == VNA_IOC_RING_GET_STATE/ {
	self->pending = 0;
	statep = (vioc_ring_state_t *)copyin(self->dptr,
	    sizeof (vioc_ring_state_t));
	printf("GET_STATE %x %x/%x/%x AV %x USED %x\n",
	    statep->vrs_index, statep->vrs_qaddr_desc, statep->vrs_qaddr_avail,
	    statep->vrs_qaddr_used, statep->vrs_avail_idx,
	    statep->vrs_used_idx);
}

viona_ioctl:entry/arg1 == VNA_IOC_RING_PAUSE/ {
	self->cmd = "RING_PAUSE";
	printf("%s 0x%x\n", self->cmd, arg2);
}

viona_ioctl:entry/arg1 == VNA_IOC_INTR_POLL_MQ/ {
	self->cmd = "INTR_POLL_MQ";
	self->pending = arg1;
}

viona_ioctl:return/self->pending == VNA_IOC_INTR_POLL_MQ/ {
	self->pending = 0;
	intrp = (vioc_intr_poll_mq_t *)copyin(self->dptr,
	    sizeof (vioc_intr_poll_mq_t));
	printf("INTR_POLL %x set (0x%08x)\n", *self->rvp,
	    intrp->vipm_status[0]);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_FEATURES/ {
	self->cmd = "SET_FEATURES";
	featp = (uint64_t *)copyin(arg2, sizeof (uint64_t));
	printf("%s 0x%x\n", self->cmd, *featp);
}

viona_ioctl:entry/arg1 == VNA_IOC_GET_FEATURES/ {
	self->cmd = "GET_FEATURES";
	self->pending = arg1;
}

viona_ioctl:return/self->pending == VNA_IOC_GET_FEATURES/ {
	self->pending = 0;
	gfeatp = (uint64_t *)copyin(self->dptr, sizeof (uint64_t));
	printf("GET_FEATURES 0x%x\n", *gfeatp);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_NOTIFY_IOP/ {
	self->cmd = "SET_NOTIFY_IOP";
	printf("%s 0x%x\n", self->cmd, arg2);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_PROMISC/ {
	self->cmd = "SET_PROMISC";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_GET_PARAMS/ {
	self->cmd = "GET_PARAMS";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_PARAMS/ {
	self->cmd = "SET_PARAMS";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_GET_MTU/ {
	self->cmd = "GET_MTU";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_MTU/ {
	self->cmd = "SET_MTU";
	printf("%s %u\n", self->cmd, arg2);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_NOTIFY_MMIO && !arg2/ {
	self->cmd = "SET_NOTIFY_MMIO <NULL>";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_NOTIFY_MMIO && arg2/ {
	self->cmd = "SET_NOTIFY_MMIO";
	mmio = (vioc_notify_mmio_t *)copyin(arg2, sizeof (vioc_notify_mmio_t));
	printf("%s %x+%x\n", self->cmd, mmio->vim_address, mmio->vim_size);
}

viona_ioctl:entry/arg1 == VNA_IOC_GET_PAIRS/ {
	self->cmd = "GET_PAIRS";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_PAIRS/ {
	self->cmd = "SET_PAIRS";
	printf("%s 0x%x\n", self->cmd, arg2);
}

viona_ioctl:entry/arg1 == VNA_IOC_GET_USEPAIRS/ {
	self->cmd = "GET_USEPAIRS";
	printf("%s\n", self->cmd);
}

viona_ioctl:entry/arg1 == VNA_IOC_SET_USEPAIRS/ {
	self->cmd = "SET_USEPAIRS";
	printf("%s 0x%x\n", self->cmd, arg2);
}

viona_notify_iop:entry/arg1/ {
	printf("IOP NOTIFY read %x+%x\n", arg2, arg3);
}

viona_notify_iop:entry/!arg1/ {
	printf("IOP NOTIFY write %x+%x = %x\n", arg2, arg3, *args[4]);
}

viona_notify_mmio:entry/!arg1/ {
	printf("MMIO NOTIFY read %x+%x\n", arg2, arg3);
}

viona_notify_mmio:entry/arg1/ {
	printf("MMIO NOTIFY write %x+%x = %x\n", arg2, arg3, *args[4]);
}

viona_ioctl:return/self->cmd != 0 && arg1 != 0/ {
	printf("ioctl(%s) returning 0x%x\n", self->cmd, arg1);
	self->cmd = 0;
}

