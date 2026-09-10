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
    vergen_git2::Emitter::default()
        .idempotent()
        .add_instructions(&git2)?
        .emit()?;

    Ok(())
}
