extern crate bhyve_api;
extern crate libc;

use bhyve_api::*;
use libc::*;

include!(concat!(env!("OUT_DIR"), "/main.rs"));
