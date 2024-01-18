// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
use anyhow::Context;
use std::time::Duration;

#[derive(Debug)]
pub(super) struct DownloadConfig {
    pub(super) timeout: Duration,
    pub(super) buildomat_backoff: backoff::ExponentialBackoff,
    pub(super) remote_server_uris: Vec<String>,
}

impl DownloadConfig {
    pub(super) fn download_buildomat_uri(
        &self,
        uri: &str,
    ) -> anyhow::Result<bytes::Bytes> {
        info!(timeout = ?self.timeout, "Downloading '{uri}' from Buildomat");
        let client = reqwest::blocking::ClientBuilder::new()
            .timeout(self.timeout)
            .build()?;
        let try_download = || {
            let request = client
                .get(uri)
                .build()
                // failing to build the request is a permanent (non-retryable)
                // error, because any retries will use the same URI and request
                // configuration, so they'd fail as well.
                .map_err(|e| backoff::Error::permanent(e.into()))?;

            let response = client
                .execute(request)
                .map_err(|e| backoff::Error::transient(e.into()))?;
            if !response.status().is_success() {
                // when downloading a file from buildomat, we currently retry
                // all errors, since buildomat returns 500s when an artifact
                // doesn't exist. hopefully, this will be fixed upstream soon.
                let err = anyhow::anyhow!(
                    "Downloading {uri} returned HTTP error {}",
                    response.status()
                );
                return Err(backoff::Error::transient(err));
            }
            Ok(response)
        };

        let log_retry = |error, wait| {
            info!(%error, "Downloading '{uri}' failed, trying again in {wait:?}...");
        };

        let bytes = backoff::retry_notify(
            self.buildomat_backoff.clone(),
            try_download,
            log_retry,
        )
        .map_err(|e| match e {
            backoff::Error::Permanent(e) => e,
            backoff::Error::Transient { err, .. } => err,
        })
        .with_context(|| format!("Failed to download '{uri}'"))?
        .bytes()?;

        Ok(bytes)
    }
}
