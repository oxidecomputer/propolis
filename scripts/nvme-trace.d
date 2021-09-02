#!/usr/sbin/dtrace -s

/*
 * nvme-trace.d     Print propolis emulated NVMe read/write latency.
 *
 * USAGE: ./nvme-trace.d -p propolis-pid
 */

#pragma D option quiet

dtrace:::BEGIN
{
    printf("Tracing propolis PID %d... Hit Ctrl-C to end.\n", $target);
}

struct io_info {
    string op;
    uint64_t ts;
    uint64_t slba;
    uint16_t nlb;
};

struct io_info io[uint64_t];

propolis$target:::nvme_read_enqueue,
propolis$target:::nvme_write_enqueue
{
    cid = args[0];
    op = (probename == "nvme_read_enqueue") ? "read" : "write";
    io[cid].op = op;
    io[cid].ts = timestamp;
    io[cid].slba = args[1];
    io[cid].nlb = args[2];
}

propolis$target:::nvme_read_complete,
propolis$target:::nvme_write_complete
/io[args[0]].ts != 0/
{
    cid = args[0];
    elapsed = timestamp - io[cid].ts;
    elapsed_us = elapsed / 1000;
    @time[io[cid].op] = quantize(elapsed_us);
    printf("%s(cid=%u) %d blocks from LBA 0x%x in %uus\n",
           io[cid].op, cid, io[cid].nlb, io[cid].slba, elapsed_us);
}

dtrace:::END
{
}
