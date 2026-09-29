#!/usr/sbin/dtrace -s

/*
 * gpe-trace.d     Print propolis ACPI general-purpose event (GPE) register
 *                 block activity.
 *
 * Each line is prefixed with the time in microseconds since tracing started
 * and the I/O port address of the GPE register block.
 *
 * Register values are printed in hex. STS and EN register indexes refer to the
 * byte within the GPEx_STS or GPEx_EN register. The raise bit is relative to
 * the register block (the GPE number minus the block base. Propolis currently
 * only uses GPE0, so the base can be assumed to always be 0).
 *
 * USAGE: ./gpe-trace.d -p propolis-pid
 */

#pragma D option quiet

dtrace:::BEGIN
{
    printf("Tracing propolis PID %d... Hit Ctrl-C to end.\n", $target);
    start = timestamp;
}

/* gpe_read(addr, reg, idx, value) */
propolis$target:::gpe_read
{
    printf("%10d [0x%04x] read   %s[%d] = 0x%02x\n",
        (timestamp - start) / 1000,
        arg0,
        copyinstr(arg1),
        arg2,
        arg3);
}

/* gpe_write(addr, reg, idx, value, old, new) */
propolis$target:::gpe_write
{
    printf("%10d [0x%04x] write  %s[%d] <- 0x%02x (0x%02x -> 0x%02x)\n",
        (timestamp - start) / 1000,
        arg0,
        copyinstr(arg1),
        arg2,
        arg3,
        arg4,
        arg5);
}

/* gpe_sts_raise(addr, bit, sts, en) */
propolis$target:::gpe_sts_raise
{
    printf("%10d [0x%04x] raise  bit %d (STS[%d] = 0x%02x, EN[%d] = 0x%02x)\n",
        (timestamp - start) / 1000,
        arg0,
        arg1,
        arg1 / 8,
        arg2,
        arg1 / 8,
        arg3);
}

/* gpe_set_level(addr, bit, sts, en, level) */
propolis$target:::gpe_set_level
{
    printf("%10d [0x%04x] level  bit %d %s (STS[%d] = 0x%02x, EN[%d] = 0x%02x, LVL[%d] = 0x%02x)\n",
        (timestamp - start) / 1000,
        arg0,
        arg1,
        (arg4 & (1 << (arg1 % 8))) ? "asserted" : "deasserted",
        arg1 / 8,
        arg2,
        arg1 / 8,
        arg3,
        arg1 / 8,
        arg4);
}

/* gpe_update_sci(addr, state) */
propolis$target:::gpe_update_sci
{
    printf("%10d [0x%04x] sci    %s\n",
        (timestamp - start) / 1000,
        arg0,
        arg1 ? "asserted" : "deasserted");
}

/* gpe_reset(addr) */
propolis$target:::gpe_reset
{
    printf("%10d [0x%04x] reset\n",
        (timestamp - start) / 1000,
        arg0);
}
