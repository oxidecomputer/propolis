# Pheidippides (PHD): the Propolis test runner

[Pheidippides](https://en.wikipedia.org/wiki/Pheidippides), or "PHD" for short,
is a freestanding test framework and runner for testing Propolis.

PHD's test framework aims to make it easy to test that Propolis provides correct
abstractions to guest software. PHD's helpers let test authors concisely launch
VMs and interact with the guest OS via the guest serial console.

PHD is very much a work in progress. PHD issues in this repo bear the `testing`
label.

## Requirements

PHD launches "real" Propolis server instances in freestanding processes on the
system that hosts the PHD runner. The runner machine needs to have a prebuilt
Propolis server binary and needs to meet all the system requirements described
in the main Propolis repo (e.g. the runner must run on a Helios system that's
sufficiently modern to run the Propolis server of interest).

## Quickstart

To get started running PHD tests:

1. Build the `propolis-server` target (e.g. with `cargo build --bin
   propolis-server`).
1. From the `phd-tests` directory, run `./quickstart.sh $PROPOLIS_PATH`, where
   `$PROPOLIS_PATH` is the path to the server binary built in the previous step.

That's it! PHD will obtain a guest OS image and a bootrom and run its tests
against them.

## Building & executing the runner

To build:

`cargo build -p phd-runner`

PHD requires the unwinding of stacks in order to properly catch assertions in
test cases, so building with a profile which sets `panic = "abort"` is not
supported.  This precludes the use of the `release` or `dev-abort` profiles.

To run:

`pfexec cargo run -p phd-runner -- [OPTIONS]`

Running under pfexec is required to allow PHD to ensure the host system is
correctly configured to run live migration tests.

## Runtime options

The runner takes one of two subcommands:

- `run` actually runs a set of tests.
- `list` lists the tests that would be run given the values of other parameters.

In `run` mode, the following sub-arguments are required:

- `--propolis-server-cmd $PROPOLIS_PATH` supplies the path to the
  `propolis-server` binary to execute.
- `--tmp-directory $TMPDIR` supplies a temporary directory to which the runner
  writes propolis-server log files and other temporary files generated during
  tests.
- `--artifact-toml-path` supplies the path to a TOML file defining the set of
  artifacts--guest OS and firmware images--to use for the current run. This
  file's format is described below. The `artifacts.toml` file in the repo
  contains the list of artifacts that are currently used in regular PHD test
  runs and that are meant to be compatible with PHD's guest adapters.

Other options are described in the runner's help text (`cargo run -- --help`).

### Specifying artifacts

The runner requires a TOML file that specifies the guest OS and firmware images
that are available for a test run to use. It has the following format:

```toml
# An array of URIs from which to try to fetch artifacts with the "remote_server"
# source type. The runner appends "/filename" to each of these URIs to generate
# a download URI for each such artifact.
remote_server_uris = ["http://foo.com", "http://bar.net"]

# Every artifact has a named entry in the "artifacts" table. The runner's
# `default_guest_artifact` and `default_bootrom_artifact` parameters name the
# guest OS and bootrom artifacts that will be used for a given test run.
#
# Every artifact has a kind, which is one of `guest_os`, `bootrom`, or
# `propolis_server`.
#
# Every artifact also has a source, which is one of `remote_server`,
# `local_path`, or `buildomat`.
#
# The following entry specifies a guest OS named "alpine" that searches the
# remote URI list for files named "alpine.iso":
[artifacts.alpine]
filename = "alpine.iso"

# Bootrom and Propolis server artifacts can put a `kind = "foo"` entry inline,
# but guest OSes need to use the structured data syntax to specify the guest OS
# adapter to use when booting a guest from this artifact.
[artifacts.alpine.kind]
guest_os = "alpine"

# Remote artifacts are required to specify an expected SHA256 digest as a
# string.
[artifacts.alpine.source.remote_server]
sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

# The following entry specifies a debug bootrom pulled from Buildomat. Buildomat
# outputs are associated with a single repo and a commit therein; the jobs that
# create them also specify a 'series' that identifies the task that created the
# collateral.
[artifacts.bootrom]
filename = "OVMF_CODE.fd"
kind = "bootrom"

[artifacts.bootrom.source.buildomat]
repo = "oxidecomputer/edk2"
series = "image_debug"
commit = "commit_sha"
sha256 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"

# This entry specifies a local directory in which an artifact can be found.
# SHA256 digests are optional for local artifacts. This allows you to create
# an entry for a local artifact that changes frequently (e.g. a Propolis build)
# without having to edit the digest every time it changes.
[artifacts.propolis]
filename = "propolis-server"
kind = "propolis_server"

[artifacts.propolis.source.local_path]
path = "/home/oxide/propolis/target/debug"
# sha256 = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
```

## Guest OS support

Different guest OS images may have different feature sets and login
requirements. The PHD framework abstracts these differences out guest OS
adapters that implement the `GuestOs` trait, whose methods supply PHD with
guest-specific information like the sequence of commands needed to log on or the
expected guest command prompt.

The full list of supported OSes is defined in the framework's
[guest OS module](framework/src/guest_os/mod.rs). Each guest OS artifact in the
artifact TOML (see above) must have a `kind` that corresponds to a variant of
the `GuestOsKind` enum in this module.

Some guest OSes are presumed to use password-based login credentials. These are
encoded into the logon sequences for each adapter and reproduced below:

| Guest adapter       | Username        | Password     |
|---------------------|-----------------|--------------|
| Alpine Linux        | `root`          |              |
| Debian 11 (nocloud) | `root`          |              |
| Ubuntu 20.04        | `ubuntu`        | `1!Passw0rd` |
| Windows Server 2022 | `Administrator` | `0xide#1Fan` |

If you add a custom image to your artifact file, you must make sure either to
configure the image to accept the credentials its adapter supplies or to change
the adapter to provide the correct credentials.

## Authoring tests

PHD's test cases live in the `tests` crate. To write a new test, add a function
of the form `fn my_test(ctx: &Framework)` and tag it with the
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

Every test gets a `phd_testcase::Framework` that contains helper methods for
constructing VMs and their execution environments. See the module documentation
for more information.

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
