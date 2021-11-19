use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum ErrorCode {
    Bad,
    IncompatibleVersion,
    DeviceShapeMismatch,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub struct Error(pub ErrorCode, pub String);

pub type Status = std::result::Result<(), Error>;

pub const OK: Status = Ok(());
