provider propolis {
	/* pio_in(port, bytes, value, was_handled) */
	probe pio_in(uint16_t, uint8_t, uint32_t, uint8_t);
	/* pio_out(port, bytes, value, was_handled) */
	probe pio_out(uint16_t, uint8_t, uint32_t, uint8_t);
	/* mmio_read(addr, bytes, value, was_handled) */
	probe mmio_read(uint64_t, uint8_t, uint64_t, uint8_t);
	/* mmio_write(addr, bytes, value, was_handled) */
	probe mmio_write(uint64_t, uint8_t, uint64_t, uint8_t);

	/* vm_entry(vcpuid) */
	probe vm_entry(uint32_t);

	/* vm_exit(vcpuid, rip, code) */
	probe vm_exit(uint32_t, uint64_t, uint32_t);

	/* nvme_read_enqueue(cid, slba, nlb) */
	probe nvme_read_enqueue(uint16_t, uint64_t, uint16_t);
	/* nvme_read_complete(cid) */
	probe nvme_read_complete(uint16_t);

	/* nvme_write_enqueue(cid, slba, nlb) */
	probe nvme_write_enqueue(uint16_t, uint64_t, uint16_t);
	/* nvme_write_complete(cid) */
	probe nvme_write_complete(uint16_t);
};
