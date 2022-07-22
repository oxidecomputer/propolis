# Pheidippides (PHD): the Oxide Propolis test runner

[Pheidippides](https://en.wikipedia.org/wiki/Pheidippides), or "PHD" for short,
is a freestanding test framework and runner for testing the
[Propolis](https://github.com/oxidecomputer/propolis) virtual machine monitor.

The PHD framework aims to make it easy to test that Propolis provides the right
abstractions to guest software. PHD's helpers let test authors concisely launch
VMs and interact with the guest OS via the guest serial console.

PHD is very much a work in progress. Some of the features on its roadmap are
listed in the project's [TODO list](TODO.adoc).

## Requirements

PHD launches "real" Propolis server instances in freestanding processes on the
system that hosts the PHD runner. The runner machine needs to have a prebuilt
Propolis server binary and needs to meet all the system requirements described
in the main Propolis repo (e.g. the runner must run on a Helios system that's
sufficiently modern to run the Propolis server of interest).

## Quickstart

To get started running PHD tests:

1. Obtain a Propolis server binary, e.g. by cloning the [Propolis
   repo](https://github.com/oxidecomputer/propolis) and running `cargo build`.
1. From the `phd-tests` directory, run `./quickstart.sh $PROPOLIS_PATH`, where
   `$PROPOLIS_PATH` is the path to the server binary built in the previous step.

That's it! PHD will obtain a guest OS image and a bootrom and run its tests
against them.

## Building the runner

`cargo [build|run] --bin=phd-runner --profile=phd -- <OPTIONS>`

Note that `--profile=phd` is required to allow the runner to catch assertions
from test cases (the default Propolis profile aborts on panic instead of
unwinding).

## Runtime options

The PHD runner requires the following command-line parameters:

- `--propolis-server-cmd $PROPOLIS_PATH` supplies the path to the
  `propolis-server` binary to execute.
- `--tmp-directory $TMPDIR` supplies a temporary directory to which the runner
  writes propolis-server log files and other temporary files generated during
  tests.
- `--artifact-toml-path` supplies the path to a TOML file defining the set of
  artifacts--guest OS and firmware images--to use for the current run. This
  file's format is described below; [`sample_artifacts.toml`] contains a small
  example to get you started.

Most other options are described in the runner's help text (`cargo run --
--help`).

### Specifying artifacts

The runner requires a TOML file that specifies what guest OS and firmware
artifacts are available in a given test run. This file has the following
entries:

- `local_root`: The path to the local directory in which artifacts are stored.
  PHD requires read/write access to this directory.
- `remote_root`: Optional. The URI of a remote path from which artifacts can be
  downloaded if they're missing from the local path or appear to be corrupted.
- One or more `guest_images` tables, written as `[guest_images.$KEY]`. The
  runner uses the value of its `--default-guest-artifact` parameter to choose a
  guest image to use for tests that don't attach their own disks.

  These tables have the following fields:
  - `guest_os_kind`: Supplies the "kind" of guest OS this is. PHD uses this to
    create an adapter that tells the rest of the framework how to interact with
    this guest OS--how to log in, what command prompt to expect, etc. The list
    of kinds is given by the `framework::guest_os::artifacts::GuestOsKind` enum.

    Note that PHD expects images to conform to the per-OS-kind behaviors encoded
    in its guest OS adapters. That is, if the PHD code's adapter for
    `GuestOsKind::Foo` says that FooOS is expected to have a shell prompt of
    `user@foo$`, and instead it's `user@bar$`, tests using this artifact will
    fail.
  - `metadata.relative_local_path`: The path to this artifact relative to the
    `local_root` in this artifact file.
  - `metadata.expected_digest`: Optional. A string containing the expected
    SHA256 digest of this artifact.

    At the start of each test, PHD will check the integrity of artifacts with
    expected digests against this digest. If the computed and expected digests
    don't match, PHD will either attempt to reacquire the artifact or abort
    testing.

    If not specified, PHD will skip pre-test integrity checks for this artifact.
    Note that this can cause changes to a disk image to persist between test
    cases!
  - `metadata.relative_remote_path`: Optional. The path to this artifact
    relative to the `remote_root` in this artifact file.

    If an artifact is not present on disk or has the wrong SHA256 digest, the
    runner will try to redownload the artifact from this path.

    If this is omitted for an artifact, the runner will abort testing if it
    encounters a scenario where it needs to reacquire the artifact.
- One or more `[bootroms]` tables, written as `[bootroms.$KEY]`. The runner uses
  the value of its `--default-bootrom-artifact` parameter to choose a bootrom to
  use for tests that don't select their own bootrom.

  The fields in these tables are `relative_local_path`, `expected_digest`, and
  `relative_remote_path`, with the same semantics as for guest images (just
  without the `metadata.` prefix).

### ZFS snapshot support

The runner can use ZFS snapshots to restore artifacts to a pristine state
between tests, avoiding the need to redownload them.

To use this functionality, pass the `--zfs-fs-name` parameter to the runner. The
runner will verify that the ZFS filesystem object with the specified name has a
mountpoint that matches the `local_root` specified in the artifact TOML. If it
does, the runner will take a ZFS snapshot after initially verifying its
artifacts, then roll back to that snapshot between tests.

Note: This requires the user executing the runner to have snapshot, rollback,
create, destroy, and mount privileges on the ZFS filesystem.

## Authoring tests

PHD's test cases live in the `tests` crate. To write a new test, add a function
of the form `fn my_test(ctx: &TestContext)` and tag it with the
`#[phd_testcase]` attribute macro. The framework will automatically register the
test into the crate's test inventory for the runner to discover.

### Test outcomes

`#[phd_testcase]` wraps the function body in an immediately-executed closure
that returns an `anyhow::Result<()>` and that returns `Ok(())` if the end of the
function body is reached. This means that

- Tests that reach the end of the function body will automatically pass.
- Tests that want to pass early can return `Ok(())` immediately.
- Tests can propagate errors with the `?` operator. This causes the test to
  fail. Setting `RUST_BACKTRACE=1` will enable backtraces for eligible errors of
  this kind (e.g. errors raised by framework functions).
- Tests can also use the `panic!` or `assert!` macros; the runner will catch
  panics and convert them into test failures.

### Test context

The `TestContext` structure contains a `VmFactory` helper object that tests can
use to construct VMs. See the module documentation for more information.

The tests in `tests/src/smoke.rs` provide some simple examples of using the
factory to customize and launch a new VM.

### Test VMs

The VM factory yields objects of type `TestVm` that provide routines that change
a VM's state and interact with its serial console. The `run_shell_command`
routine attempts to run a command in the guest's serial console and returns the
command's output as a string. See the module documentation for more information.

## Source layout

PHD is arranged into the following crates:

- `framework` contains the bulk of the test framework and has the following
  modules:
  - `artifacts` implements the artifact store, which processes the artifact TOML
    and provides other modules a way to convert from artifact keys to paths on
    disk.
  - `guest_os` defines the `GuestOsKind` enumeration and the `GuestOs` trait.
    Each supported guest OS implements this trait to provide guest-specific
    adapters for high-level test VM operations to use.
  - `serial` provides a task that connects to a guest's serial console, sends
    commands to it, and processes the characters and terminal control sequences
    the guest sends back. It also provides routines that allow VMs to wait for a
    specific string to arrive in the console's back buffer.
  - `test_vm` implements the `TestVm` struct and the `VmFactory` that tests use
    to configure new VMs.
- `testcase_macro` and `testcase` contain the `TestContext` type, the definition
  of the `#[phd_testcase]` macro, and the support code for the test inventory.
  `testcase` re-exports `testcase_macro`, so consumers of these modules only
  need to import `testcase`.
- `tests` contains individual test cases.
- `runner` implements the test runner, its command-line configuration, and its
  test fixtures.
