#!/usr/sbin/dtrace -s

/*
 * nvme-io-timing.d     Print propolis emulated NVMe read/write latency.
 *
 * USAGE: ./nvme-io-timing.d -p propolis-pid -o your_io_trace.jsonl
 *
 * This script traces the activity of *writes* from Propolis' emulated NVMe
 * controller through file-backed disks, to the host OS and back. The measured
 * time is intended to capture period of an I/O that is opaque from a guest
 * perspective; the I/O has been submitted to a disk, and "magic" is occurring
 * to effect that I/O.  This script measures the "magic".
 *
 * The output from this script is suitable for being fed into
 * [statemaps](https://github.com/bcantrill/statemap), though a trace of an
 * active system will be somewhat... large (at even 5k I/Os/sec, a full second
 * of execution easily produces 50 MiB statemap .svg files). For a long
 * timespan, consider constructing the statemap with `--coalesce` with a large
 * value. Otherwise, states will be .. coalesced ..  until the total number of
 * distinct state rectangles in the map is below a default threshold. For the
 * problem measured by this script, coalescing can be difficult to interpret.
 *
 * ## CONSIDERATIONS
 *
 * This script makes several critical assumptions to account I/O correctly,
 * consider if they still apply when using (and considering the output of) this
 * script:
 *
 * ### Return-path accounting does not measure when a vCPU becomes active.
 *
 * When an I/O is completed, we *may* send an interrupt to the guest. Doorbell
 * Buffers may allow us to elide an interrupt, interrupt coalescing in the
 * future may mean we defer interrupts under load, and the guest may simply be
 * busy-polling on the head of the completion queue to discover that an I/O is
 * comlete before we've even sent an interrupt.
 *
 * In the serial case this script was written for, it is expected none of the
 * above will actually be occurring, but the "this can only get more wrong
 * under load" factor has me hedging in the direction of not introducing the
 * confusion for ourselves. In the serial case it is straightforward to infer
 * from this latency from looking at the delay between a vCPU doorbell notify
 * and worker thread activation, as the maps are otherwise pretty empty.
 *
 * ### Only one I/O is considered per NVMe Submission Queue at a time.
 *
 * This is already wrong. When there are more worker threads than NVMe queues,
 * threads are doubled up (or more) on submission queues, so several I/Os can
 * be in-flight on one queue at a time. This script will not disambiguate such
 * I/Os.
 *
 * Handling this case will probably involve mixing a thread ID into the
 * `devsq_id` that is used as `self->id`, which is doable but was not needed as
 * of writing (where only serial I/Os were being measured).
 *
 * ### I/O is tied to a thread while being processed.
 *
 * This is correct, but only until the plumbing is done (in Propolis and the
 * OS) for file-backed disks to be operated on with async I/O.
 *
 * Once async I/O is on the scene, it may be rather difficult to account time
 * to individual I/O latency - we'll want to follow the devsq_id and individual
 * submission ID, connect that to the allocated structures as that I/O is
 * handed off to the OS, and use pointers for those structures to associate
 * time in kernel functions back to individual guest-originated I/Os. That
 * seems a little tricky, and is obviously much more work than is needed at the
 * moment - and I'm not sure how to spell the DTrace for this currently!
 *
 * ### Some I/O queues throw statemap for a loop
 *
 * `statemap` looks like it expects entitites to be numbers, or at least
 * something that can be written un-quoted as a literal in a javascript object.
 * For many NVMe `devsq_id` this happens to work out, though the number should
 * be interpreted as hex rather than decimal. But for, say, the 14th queue in a
 * device, that yields a devsq_id of something like 04000e, which causes a
 * javascript parse error and breaks the statemap svg.
 *
 * This is workaround-able by quoting the entity in the statemap's javascript
 * if you're afflicted. I'll have to take a look at statemap to see if it's an
 * upstream issue or an issue in the output from this script.
 */

#pragma D option quiet

dtrace:::BEGIN
{
	/*
    printf("Tracing propolis PID %d... Hit Ctrl-C to end.\n", $target);
	*/
	wallstart = timestamp;
	printf("{\n");
	printf("    \"start\": [%d, %d],\n", wallstart / 1000000000, wallstart % 1000000000);
	printf("    \"title\": \"disk io\",\n");
	printf("    \"host\": \"%s\",\n", `utsname.nodename);
	printf("\"states\": {\n");
	printf("    \"idle\": { \"value\": 8, \"color\": \"#e0e0e0\" }, \n");
	printf("    \"nvme-write-enq\": { \"value\": 0, \"color\": \"#2af7a6\" },\n");
	printf("    \"write-os-dispatched\": { \"value\": 1, \"color\": \"#2ac7a6\" },\n");
	printf("    \"write-os-complete\": { \"value\": 2, \"color\": \"#2af7f6\" },\n");
	printf("    \"nvme-write-comp\": { \"value\": 3, \"color\": \"#2a67a6\" },\n");
	printf("    \"nvme-flush-enq\": { \"value\": 4, \"color\": \"#4af706\" },\n");
	printf("    \"flush-os-dispatched\": { \"value\": 5, \"color\": \"#4ac706\" },\n");
	printf("    \"flush-os-complete\": { \"value\": 6, \"color\": \"#4af7f6\" },\n");
	printf("    \"nvme-flush-comp\": { \"value\": 7, \"color\": \"#4a6706\" },\n");
	printf("    \"physio-started\": { \"value\": 9, \"color\": \"#9a0726\" },\n");
	printf("    \"zvol-rawio-started\": { \"value\": 10, \"color\": \"#aa6726\" },\n");
	printf("    \"zvol-rawio-done\": { \"value\": 11, \"color\": \"#aa0726\" },\n");
	printf("    \"physio-done\": { \"value\": 12, \"color\": \"#2a0726\" },\n");
	printf("    \"biowait-on-io\": { \"value\": 13, \"color\": \"#ca9191\" },\n");
	printf("    \"as_pageunlocking\": { \"value\": 14, \"color\": \"#8a41f1\" },\n");
	printf("    \"pageunlock-complete\": { \"value\": 15, \"color\": \"#8a21f1\" },\n");
	printf("    \"as_pagelocking\": { \"value\": 16, \"color\": \"#8a81f1\" },\n");
	printf("    \"pagelock-complete\": { \"value\": 17, \"color\": \"#8a61f1\" },\n");
	printf("    \"kmem-cache-alloc\": { \"value\": 18, \"color\": \"#1ff161\" },\n");
	printf("    \"kmem-cache-free\": { \"value\": 19, \"color\": \"#1fc311\" },\n");
	printf("    \"nvme-sq-doorbell\": { \"value\": 20, \"color\": \"#fa17f6\" },\n");
	printf("    \"nvme-read-enq\": { \"value\": 21, \"color\": \"#aaf7a6\" },\n");
	printf("    \"read-os-dispatched\": { \"value\": 22, \"color\": \"#aac7a6\" },\n");
	printf("    \"read-os-complete\": { \"value\": 23, \"color\": \"#aaf7f6\" },\n");
	printf("    \"nvme-read-comp\": { \"value\": 24, \"color\": \"#aa67a6\" }\n");
	printf("} }\n");
	START = timestamp;
}

propolis$target:::nvme_read_enqueue
{
	self->id = arg0;
	self->state = 21;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

propolis$target:::nvme_read_complete / self->id /
{
	self->state = 8;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
	self->id = 0;
}

propolis$target:::block_begin_read / self->id /
{
	self->state = 22;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

propolis$target:::block_complete_read / self->id /
{
	self->state = 23;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

propolis$target:::nvme_doorbell / arg2 == 0 / {
	self->state = 20;
	/*
	 * do not set `self->state = arg1` here. while arg1 is the correct entity
	 * we're tracking the state of (I/Os on queue device/queue `arg1`), we use
	 * the non-zeroness of that field to key other probes below. this thread
	 * will not continue the I/O (it's the vCPU thread!) so setting self->id
	 * would prime probes for f.ex scheduler activity which is actually not in
	 * the processing path for this I/O.
	 */
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, arg1);
}

propolis$target:::nvme_write_enqueue
{
	self->id = arg0;
	self->state = 0;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

propolis$target:::nvme_write_complete / self->id /
{
	self->state = 8;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
	self->id = 0;
}

propolis$target:::block_begin_write / self->id /
{
	self->state = 1;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

propolis$target:::block_complete_write / self->id /
{
	self->state = 2;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

propolis$target:::nvme_flush_enqueue
{
	self->id = arg0;
	self->state = 4;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

propolis$target:::nvme_flush_complete / self->id /
{
	self->state = 8;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
	self->id = 0;
}

propolis$target:::block_begin_flush / self->id /
{
	self->state = 5;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

propolis$target:::block_complete_flush / self->id /
{
	self->state = 6;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

fbt::default_physio:entry / self->id / {
	self->state = 9;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
	self->in_default_physio = 1;
}

fbt::zvol_rawio:entry / self->id / {
	self->state = 10;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

fbt::zvol_rawio:return / self->id / {
	self->state = 11;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

fbt::default_physio:return / self->id / {
	self->state = 12;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
	self->in_default_physio = 0;
}

fbt::kmem_cache_alloc:entry / self->id && self->in_default_physio / {
	/*
	 * don't record a specific self->state here: kmem_cache_alloc (and free)
	 * are treated like momentary interruptions of the previous state, whatever
	 * it was. we'll emit a record "restoring" the previous state on return.
	 */
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", 18, timestamp - START, self->id);
}

fbt::kmem_cache_alloc:return / self->id && self->in_default_physio / {
	/*
	 * see the note on `kmem_cache_alloc:entry`.
	 */
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

fbt::kmem_cache_free:entry / self->id && self->in_default_physio / {
	/*
	 * same situation as kmem_cache_alloc:entry above.
	 */
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", 19, timestamp - START, self->id);
}

fbt::kmem_cache_free:return / self->id && self->in_default_physio / {
	/*
	 * see the note on `kmem_cache_alloc:entry`.
	 */
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

fbt::as_pagelock:entry / self->id && self->in_default_physio / {
	self->state = 16;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

fbt::as_pageunlock:entry / self->id && self->in_default_physio / {
	self->state = 14;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

fbt::as_pagelock:return / self->id && self->in_default_physio / {
	self->state = 17;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

fbt::as_pageunlock:return / self->id && self->in_default_physio / {
	self->state = 15;
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

/*
 * An earlier version of this script found it modestly interesting to look at
 * the off-cpu and on-cpu time for worker threads once they've gotten into the
 * kernel to submit disk I/Os.
 *
 * Measuring `biowait` directly, rather than the off-cpu/on-cpu events it
 * implies, seems slightly more interpretable, so do that instead.
 *
 * Similar to kmem_cache_alloc etc above, print a state in-line as we're just
 * "pausing" whatever state we were in before. We'll come back to it, so
 * preserver it in `self->state` until then.
 */
fbt::biowait:entry / self->id && self->in_default_physio / {
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", 13, timestamp - START, self->id);
}

fbt::biowait:return / self->id && self->in_default_physio / {
	/*
	 * The biowait is done, so we are back to whatever it was we were doing before waiting.
	 */
	printf("{\"state\": %d, \"time\": \"%d\", \"entity\": \"%x\"}\n", self->state, timestamp - START, self->id);
}

tick-1s {
	exit(0);
}
