This is, for now, an aspirational description of the various states a Propolis
instance could travel through over the course of its life, and what events (and
their requisite details) might trigger those state transitions.

Notably absent from this ideation is mention of states or events related to
live migration.  That will be added once more of the system has been
prototyped.

# States

## Start

Propolis is empty.  No VMM resources exist, nor do any emulated device
definitions.  It waits in this state until it receives a valid creation payload.

## Initialize

A creation payload has been received.  In-kernel VMM resources are allocated.
Any devices spelled out by the payload which are emulated in userspace are
instantiated.  Resources backing those devices are attached.  Worker threads
running vCPUs, driving device emulation (like block IO), and event handling are
started.  The vCPU threads themselves are held outside the running state.  The
vCPUs themselves are initialized with the architecturally-defined INIT state.

## Boot

If guest boot was made conditional on certain state changes, such as a console
connection being established, it is here that progress will be blocked until
those requirements are met.

Once all conditions are fulfilled, the vCPU threads are released from their
holds, allowing them to enter running context.  Emulation for all devices
defined in the initial machine manifest is running as well.  The in-guest boot
software (UEFI) begins execution.

## Run

The VM is running its guest workload.

## Quiesce

The VM ceases to run its guest workload.  All vCPU threads are to exit their
run-loops.  All emulated devices are notified of the quiesce, and are to report
when they have completed termination of all pending operations.


## Halt

All emulated devices must already be quiesced before entering halt state, so
little is left to do.  It is in the Halt state that any diagnostic data could
be collected from the instance (if it were to be halted for a fault, say),
prior to its reboot, or destruction.

## Reboot

All emulated devices are notified that the VM is undergoing a reboot.  The
vCPUs are set to their architecturally defined RESET states, with vCPU threads
re-entering their run-loops to wait at a hold point.

Once all device reboot processing is complete, the instance proceeds back to
the Boot state.

## Destroy

Destruction of the VM and all all its associated emulation resources, such as
in-kernel VMM state and guest DRAM, is initiated.  Persistent resources
utilized by the VM, such as the backing for block devices, or vnics provisioned
in the host network stack are not destroyed, but rather just detached from.

Once all necessary destruction and clean-up has occurred, Propolis proceeds
back to Start, or the process exits, depending on its configuration.

# Event Progression

At any given time, a Propolis instance will be in one of the above states.  It
will also have an optional Target State, set by external events, which it will
use to drive through the state graph as defined above.

Example events would include:
- Forced Poweroff: Target State set to Destroy
- Forced Reset: Target State set to Reboot
- Guest triple-fault: Target State set to Reboot
- "Soft" Reset: No Target State change.  ACPI notification only
- "Soft" Poweroff: No Target State change.  ACPI notification only
