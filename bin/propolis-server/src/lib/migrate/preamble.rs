use propolis_client::instance_spec::InstanceSpec;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug)]
pub(crate) struct Preamble {
    pub instance_spec: InstanceSpec,
    pub blobs: Vec<Vec<u8>>,
}

impl Preamble {
    pub fn new(instance_spec: InstanceSpec) -> Preamble {
        Preamble { instance_spec, blobs: Vec::new() }
    }
}
