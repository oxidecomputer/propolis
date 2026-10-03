// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

fn main() -> anyhow::Result<()> {
    let git2 = vergen_git2::Git2::builder()
        .branch(true)
        .commit_count(true)
        .dirty(true)
        .sha(true)
        .build();
    vergen_git2::Emitter::default().add_instructions(&git2)?.emit()?;
    // Apologies for the broad rerun-if-changed, but stick with me: build.rs
    // *must* rerun when source files are changed so that a version string's
    // VERGEN_GIT_DIRTY is accurate. If we only rerun-on-changed for changes in
    // the bin crates, that almost works, but means a source-only change in
    // a crate in this tree that a bin depends on directly (say,
    // propolis-api-types) wouldn't cause this build.rs to re-evaluate and get
    // the right dirty bit in the version string.
    //
    // We could do the VERGEN_GIT_DIRTY work in the binary crates too, and plumb
    // that back to propolis-lib version's string, but we'd want that to be
    // broad to cover crates too. This takes the other approach: rerun
    // propolis-lib build.rs if any files here have changed, which will cause
    // propolis dependents to rebuild, and get updated version strings. This
    // comes at the cost of recompiling the propolis-lib crate more often than
    // strictly necessary during development, but that crate on its own is
    // *much* faster to build than any of the bins being linked afterwards!
    //
    // This also means we can keep all of the versioning fiddliness here instead
    // of subjecting every (e.g. both) user to thinking about this stuff.
    println!("cargo:rerun-if-changed=../lib");
    println!("cargo:rerun-if-changed=../bin");
    println!("cargo:rerun-if-changed=../crates");

    Ok(())
}
