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

	/* virtio_vq_notify(virtio_dev_addr, virtqueue_id) */
	probe virtio_vq_notify(uint64_t, uint16_t);
	/* virtio_vq_pop(vq_addr, avail_idx) */
	probe virtio_vq_pop(uint64_t, uint16_t);
	/* virtio_vq_pop(vq_addr, used_idx, used_len) */
	probe virtio_vq_push(uint64_t, uint16_t, uint32_t);
};
