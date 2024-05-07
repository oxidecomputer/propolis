// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

fn main() -> anyhow::Result<()> {
    // Generate Git version information.
    vergen::EmitBuilder::builder()
        // Don't emit timestamps and build info.
        .idempotent()
        .git_branch()
        .git_sha(true)
        .git_commit_count()
        .emit()?;

    Ok(())
}
