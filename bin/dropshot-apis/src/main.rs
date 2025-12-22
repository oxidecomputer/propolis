// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::process::ExitCode;

use anyhow::Context;
use camino::Utf8PathBuf;
use clap::Parser;
use dropshot_api_manager::{Environment, ManagedApiConfig, ManagedApis};
use dropshot_api_manager_types::{ManagedApiMetadata, Versions};
use propolis_server_api::*;

pub fn environment() -> anyhow::Result<Environment> {
    // The workspace root is two levels up from this crate's directory.
    let workspace_root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let env = Environment::new(
        // This is the command used to run the OpenAPI manager.
        "cargo xtask openapi",
        workspace_root,
        // This is the location within the workspace root where the OpenAPI
        // documents are stored.
        "openapi",
    )?
    .with_default_git_branch("origin/master".to_owned());
    Ok(env)
}

/// The list of APIs managed by the OpenAPI manager.
pub fn all_apis() -> anyhow::Result<ManagedApis> {
    let apis = vec![ManagedApiConfig {
        ident: "propolis-server",
        versions: Versions::Versioned {
            supported_versions: propolis_server_api::supported_versions(),
        },
        title: "Oxide Propolis Server API",
        metadata: ManagedApiMetadata {
            description: Some(
                "API for interacting with the Propolis hypervisor frontend.",
            ),
            contact_url: Some("https://oxide.computer"),
            contact_email: Some("api@oxide.computer"),
            ..Default::default()
        },
        api_description: propolis_server_api_mod::stub_api_description,
    }];

    let apis = ManagedApis::new(apis).context("error creating ManagedApis")?;
    Ok(apis)
}

fn main() -> anyhow::Result<ExitCode> {
    let app = dropshot_api_manager::App::parse();
    let env = environment()?;
    let apis = all_apis()?;

    Ok(app.exec(&env, &apis))
}

#[cfg(test)]
mod test {
    use dropshot_api_manager::test_util::check_apis_up_to_date;

    use super::*;

    // Also recommended: a test which ensures documents are up-to-date. The
    // OpenAPI manager comes with a helper function for this, called
    // `check_apis_up_to_date`.
    #[test]
    fn test_apis_up_to_date() -> anyhow::Result<ExitCode> {
        let env = environment()?;
        let apis = all_apis()?;

        let result = check_apis_up_to_date(&env, &apis)?;
        Ok(result.to_exit_code())
    }
}
