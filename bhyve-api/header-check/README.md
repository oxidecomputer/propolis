# bhyve-api Header Check

In order to facilitate accurate reproduction of the bhyve kernel interfaces
from their respective headers, this crate uses `ctest2` in the same manner as
[rust-libc](https://github.com/rust-lang/libc/).

## Usage

Building and executing the test requires a set of illumos-gate sources
corresponding to the version of interfaces authoried in bhyve-api.  Due to a
tangle in those headers, building and executing the test must itself be done on
a moderately recent illumos system, although that may change in the future.

From the `header-check` directory, execute `cargo test` with the `GATE_SRC`
environment variable pointing towards the afformentioned illumos-gate source
tree:

```
$ GATE_SRC=/path/to/my/illumos-gate cargo test
    Finished test [unoptimized + debuginfo] target(s) in 0.02s
     Running test/main.rs (target/debug/deps/main-2be1b9ed23245f3a)
RUNNING ALL TESTS
PASSED 578 tests
```
