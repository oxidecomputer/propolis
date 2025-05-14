#!/usr/sbin/dtrace -s

/*
 * xhci-trace.d     Print propolis emulated xHCI MMIO and TRB ring activity.
 *
 * USAGE: ./xhci-trace.d -p propolis-pid
 */

#pragma D option quiet

string trb_types[64];

dtrace:::BEGIN
{
    printf("Tracing propolis PID %d... Hit Ctrl-C to end.\n", $target);

    trb_types[0] = "Reserved";

    trb_types[1] = "Normal Transfer";
    trb_types[2] = "Setup Stage Transfer";
    trb_types[3] = "Data Stage Transfer";
    trb_types[4] = "Status Stage Transfer";
    trb_types[5] = "Isoch Transfer";
    trb_types[6] = "Link Transfer";
    trb_types[7] = "Event Data Transfer";
    trb_types[8] = "No Op Transfer";

    trb_types[9] = "Enable Slot Cmd";
    trb_types[10] = "Disable Slot Cmd";
    trb_types[11] = "Address Device Cmd";
    trb_types[12] = "Configure Endpoint Cmd";
    trb_types[13] = "Evaluate Context Cmd";
    trb_types[14] = "Reset Endpoint Cmd";
    trb_types[15] = "Stop Endpoint Cmd";
    trb_types[16] = "Set T R Dequeue Pointer Cmd";
    trb_types[17] = "Reset Device Cmd";
    trb_types[18] = "Force Event Cmd";
    trb_types[19] = "Negotiate Bandwidth Cmd";
    trb_types[20] = "Set Latency Tolerance Value Cmd";
    trb_types[21] = "Get Port Bandwidth Cmd";
    trb_types[22] = "Force Header Cmd";
    trb_types[23] = "No Op Cmd";
    trb_types[24] = "Get Extended Property Cmd";
    trb_types[25] = "Set Extended Property Cmd";

    trb_types[26] = "Reserved";
    trb_types[27] = "Reserved";
    trb_types[28] = "Reserved";
    trb_types[29] = "Reserved";
    trb_types[30] = "Reserved";
    trb_types[31] = "Reserved";

    trb_types[32] = "Transfer Event";
    trb_types[33] = "Command Completion Event";
    trb_types[34] = "Port Status Change Event";
    trb_types[35] = "Bandwidth Request Event";
    trb_types[36] = "Doorbell Event";
    trb_types[37] = "Host Controller Event";
    trb_types[38] = "Device Notification Event";
    trb_types[39] = "Mf Index Wrap Event";

    trb_types[40] = "Reserved";
    trb_types[41] = "Reserved";
    trb_types[42] = "Reserved";
    trb_types[43] = "Reserved";
    trb_types[44] = "Reserved";
    trb_types[45] = "Reserved";
    trb_types[46] = "Reserved";
    trb_types[47] = "Reserved";

    trb_types[48] = "Vendor";
    trb_types[49] = "Vendor";
    trb_types[50] = "Vendor";
    trb_types[51] = "Vendor";
    trb_types[52] = "Vendor";
    trb_types[53] = "Vendor";
    trb_types[54] = "Vendor";
    trb_types[55] = "Vendor";
    trb_types[56] = "Vendor";
    trb_types[57] = "Vendor";
    trb_types[58] = "Vendor";
    trb_types[59] = "Vendor";
    trb_types[60] = "Vendor";
    trb_types[61] = "Vendor";
    trb_types[62] = "Vendor";
    trb_types[63] = "Vendor";
}

struct io_info {
    string op;
    uint64_t ts;
    uint64_t offset_bytes;
    uint64_t size_bytes;
};

struct io_info io[uint64_t];

propolis$target:::xhci_consumer_ring_dequeue_trb /* (offset, parameter, trb_type) */
{
    if (arg2 < 64) {
        printf("[%Y] [0x%08x] Dequeued %s TRB (type %d) with parameter 0x%x\n", walltimestamp, arg0, trb_types[arg2], arg2, arg1);
    } else {
        printf("[%Y] [0x%08x] Dequeued invalid TRB type (%d)\n", walltimestamp, arg0, arg2);
    }
}

propolis$target:::xhci_consumer_ring_set_dequeue_ptr /* (pointer, cycle_state) */
{
    printf("[%Y] [0x%08x] Guest xHCD set consumer ring dequeue pointer; cycle state %d\n", walltimestamp, arg0, arg1);
}

propolis$target:::xhci_producer_ring_enqueue_trb /* (offset, data, trb_type) */
{
    if (arg2 < 64) {
        printf("[%Y] [0x%08x] Enqueued %s TRB (type %d) with parameter 0x%x\n", walltimestamp, arg0, trb_types[arg2], arg2, arg1);
    } else {
        printf("[%Y] [0x%08x] Enqueued invalid TRB type (%d)\n", walltimestamp, arg0, arg2);
    }
}

propolis$target:::xhci_producer_ring_set_dequeue_ptr /* (pointer) */
{
    printf("[%Y] [0x%08x] Guest xHCD consumed Event TRBs\n", walltimestamp, arg0);
}

propolis$target:::xhci_reg_read /* (name, value, index) */
{
    if (arg2 >= 0) {
        printf("[%Y] Read from %s[%d]: 0x%x\n", walltimestamp, copyinstr(arg0), arg2, arg1);
    } else {
        printf("[%Y] Read from %s: 0x%x\n", walltimestamp, copyinstr(arg0), arg1);
    }
}

propolis$target:::xhci_reg_write /* (name, value, index) */
{
    if (arg2 >= 0) {
        printf("[%Y] Write to %s[%d]: 0x%x\n", walltimestamp, copyinstr(arg0), arg2, arg1);
    } else {
        printf("[%Y] Write to %s: 0x%x\n", walltimestamp, copyinstr(arg0), arg1);
    }
}

propolis$target:::xhci_interrupter_pending /* (intr_num) */
{
    printf("[%Y] Interrupter %d pending\n", walltimestamp, arg0);
}

propolis$target:::xhci_interrupter_fired /* (intr_num) */
{
    printf("[%Y] Interrupter %d fired\n", walltimestamp, arg0);
}

propolis$target:::xhci_reset /* () */
{
    printf("\n[%Y] xHC reset\n\n", walltimestamp);
}

dtrace:::END
{
}
