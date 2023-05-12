#!/usr/sbin/dtrace -s

/*
 * Provides visibility into adjustments made on time-related data on a
 * destination live migration host.
 *
 * Usage: ./time_adjustments.d <propolis-server PID>
 */

#pragma D option defaultargs
#pragma D option quiet

uint64_t	start_gf;
uint64_t	usr_adj_gf;
uint64_t	krn_adj_gf;

uint64_t	start_gtsc;
uint64_t	usr_adj_gtsc;
uint64_t	krn_adj_gtsc;

int64_t		start_bhrt;
int64_t		usr_adj_bhrt;
int64_t		krn_adj_bhrt;

uint64_t	migrate_time;
uint64_t	guest_uptime;

uint64_t	usr_time;
uint64_t	krn_time;


dtrace:::BEGIN
{
	if ($$1 == "") {
		printf("ERROR: propolis-server pid required\n");
		exit(1);
	}

	printf("tracing pid %d...\n", $1);
}

propolis$1:::migrate_time_data_before
{
	start_gf = args[0];
	start_gtsc = args[1];
	start_bhrt = args[2];
	usr_time = timestamp;
}


propolis$1:::migrate_time_data_after
{
	usr_adj_gf = args[0];
	usr_adj_gtsc = args[1];
	usr_adj_bhrt = args[2];

	this->start = usr_time;
	this->end = timestamp;

	usr_time = this->end - this->start;
	vm_uptime = args[3];
	migrate_time = args[4];
}

fbt::vmm_data_write_vmm_time:entry
{
	self->vm = (struct vm *)args[0];
	self->req = (struct vdi_time_info_v1 *)args[1]->vdr_data;
	self->ts = timestamp;

	this->gf = self->req->vt_guest_freq;
	this->gtsc = self->req->vt_guest_tsc;
	this->bhrt = self->req->vt_boot_hrtime;

	if (usr_adj_gtsc != this->gtsc) {
		printf("ERROR: propolis and VMM data guest TSC differ\n");
		printf("propolis_val = %lu, bhyve_val = %lu\n",
		    usr_adj_gtsc, this->gtsc);
	}

	if (usr_adj_gf != this->gf) {
		printf("ERROR: propolis and VMM data guest freq differ\n");
		printf("propolis_val = %lu, bhyve_val = %lu\n",
		    usr_adj_gf, this->gf);
	}

	if (usr_adj_bhrt != this->bhrt) {
		printf("ERROR: propolis and VMM data boot_hrtime differ\n");
		printf("propolis_val = %ld, bhyve_val = %ld\n",
		    usr_adj_bhrt, (int64_t)this->bhrt);
	}
}

fbt::vmm_data_write_vmm_time:return
/ self->vm /
{
	if (args[1] == 0) {
		krn_adj_bhrt = self->vm->boot_hrtime;
		krn_adj_gf = self->vm->guest_freq;
	} else {
		print(args[1]);
	}

	krn_time = timestamp - self->ts;

	self->vm = 0;
	self->req = 0;
	self->ts = 0;

	printf("time data imported; press CTRL+C for summary\n");
}

fbt::calc_tsc_offset:entry
/ self->vm /
{
	krn_adj_gtsc = args[1];
}

dtrace:::END
{
	this->total = migrate_time + usr_time + krn_time;
	this->usr_d_tsc = usr_adj_gtsc - start_gtsc;
	this->usr_d_bhrt = usr_adj_bhrt - start_bhrt;
	this->krn_d_tsc = krn_adj_gtsc - usr_adj_gtsc;
	this->krn_d_bhrt = krn_adj_bhrt - usr_adj_bhrt;

	printf("%15s %20s %20s\n",
	    "GUEST FREQ (Hz)", "UPTIME (usec)", "TOTAL TIME (usec)");
	printf("%15lu %20lu %20lu\n", start_gf,vm_uptime / 1000,
	    this->total / 1000);
	printf("\n");

	printf("%-10s %20s %20s %20s\n",
	    "EVENT", "DURATION (usec)", "GUEST TSC", "BOOT HRTIME");
	printf("%-10s %20lu %20lu %20ld\n", "Migration", migrate_time / 1000,
	    start_gtsc, start_bhrt);
	printf("%-30s  %20lu %20ld\n", "[adjustment]",
	    this->usr_d_tsc, this->usr_d_bhrt);
	printf("%-10s %20lu %20lu %20ld\n", "Propolis",
	    usr_time / 1000, usr_adj_gtsc, usr_adj_bhrt);
	printf("%-30s  %20lu %20ld\n", "[adjustment]",
	    this->krn_d_tsc, this->krn_d_bhrt);
	printf("%-10s %20d %20d %20d\n", "Kernel",
	    krn_time / 1000, krn_adj_gtsc, krn_adj_bhrt);

}
