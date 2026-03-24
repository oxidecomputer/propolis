# Remaining tasks for 605 propolis-server prototype

## Status

as of 2026-03-24:

* yesterday we discussed putting the attestation server in the instance ensure
  handler. this makes sense because there's no instance to attest until that
  call happens. presumably a boot disk hashing thread gets kicked off there too.
* my next question: what does the interface between attestation and the boot
  digest calculation look like? we need some sort of concurrency primitive.


in no particular order:


## Miscellaneous 

* what to do if the boot digest is not yet calculated, but we get an attestation
  request?

answer: return an error; there is an Option field on the `attest` config structure

* what tokio runtime should the attestation server and boot digest etc tasks run
  on? there exist: an API runtime for external API requests, a vmm runtime for
  handling vmm tasks


* should we make the creation of the attestation server happen via a sled-agent
  api call, instead of an implicit one by creating a vm?

initial thoughts: no, probably not. the advantage to this is that it makes it
marginally easier for oxide technicians to turn the behavior off if something
catastrophic happens. but we have made the product decision to offer attestation
in the product regardless. if we find that we need to make it configurable, or
have a horrible bug that makes us want to turn it off, we'll deal with that
later. (keeping in mind the urgency of shipping this now)


## Boot digest calculation

from ixi in chat:

* how do we properly deal with boot order
* is the following thing out of scope: booting from the boot disk fails, and we
  ended up in a secondary guest OS because edk2 kept trying

thoughts from the call: if there isn't an explicit boot disk provided by the
control plane, we don't have a "boot disk", because ovmf will just try disks in
whatever order. it also doesn't make sense to try a second disk because if the
boot disk failed to boot, we are hosed anyway

the boot disk is part of the propolis spec







### Where does the boot digest hash thread get kicked off?

Current state: the attestation server thread is kicked off by `run_server`,
prior to any inbound API calls

The boot disk digest thread cannot be kicked off until we have a boot disk,
which means it can only begin after `instance_ensure`


## Fetch instance ID from instance properties


## 




