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
    this->cid = args[0];
    this->op = (probename == "nvme_read_enqueue") ? "read" : "write";
    io[this->cid].op = this->op;
    io[this->cid].ts = timestamp;
    io[this->cid].slba = args[1];
    io[this->cid].nlb = args[2];
}

propolis$target:::nvme_read_complete,
propolis$target:::nvme_write_complete
/io[args[0]].ts != 0/
{
    this->cid = args[0];
    this->elapsed = timestamp - io[this->cid].ts;
    this->elapsed_us = this->elapsed / 1000;
    @time[io[this->cid].op] = quantize(this->elapsed_us);
    printf("%s(cid=%u) %d blocks from LBA 0x%x in %uus\n",
           io[this->cid].op, this->cid, io[this->cid].nlb, io[this->cid].slba, this->elapsed_us);
}

dtrace:::END
{
}
