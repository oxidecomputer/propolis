// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::File as FsFile;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::chardev::pollers;
use crate::chardev::BlockingSource;

use tokio::fs::File;
use tokio::io::AsyncWriteExt;

struct Inner {
    fp: Option<FsFile>,
}

pub struct BlockingFileOutput {
    poller: Arc<pollers::BlockingSourceBuffer>,
    inner: Mutex<Inner>,
}

const BUF_SIZE: usize = 256;

impl BlockingFileOutput {
    pub fn new(fp: FsFile) -> Arc<Self> {
        let params = pollers::BlockingParams {
            poll_interval: Duration::from_millis(10),
            poll_miss_thresh: 5,
            buf_size: NonZeroUsize::new(BUF_SIZE).unwrap(),
        };
        let poller = pollers::BlockingSourceBuffer::new(params);

        Arc::new(Self { poller, inner: Mutex::new(Inner { fp: Some(fp) }) })
    }

    pub fn attach(&self, source: Arc<dyn BlockingSource>) {
        let mut inner = self.inner.lock().unwrap();
        let fp = inner.fp.take().unwrap();

        self.poller.attach(source.as_ref());

        let poller = Arc::clone(&self.poller);
        let _task = tokio::spawn(async move {
            let afp = File::from_std(fp);
            let _ = Self::run(poller, afp).await;
            todo!("get async task hdl");
        });
    }

    async fn run(poller: Arc<pollers::BlockingSourceBuffer>, mut fp: File) {
        let mut buf = [0u8; BUF_SIZE];
        loop {
            if let Some(n) = poller.read(&mut buf).await {
                fp.write_all(&buf[..n]).await.unwrap();
            }
        }
    }
}
