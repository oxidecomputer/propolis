// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

fn main() -> anyhow::Result<()> {
    // See lib/propolis `fn version` for why we need this in bin crates rather
    // than the library.
    let git2 = vergen_git2::Git2::builder()
        .dirty(true)
        .build();
    vergen_git2::Emitter::default().add_instructions(&git2)?.emit()?;

    Ok(())
}
