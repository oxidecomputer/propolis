extern crate bhyve_api_sys;
extern crate libc;

use bhyve_api_sys::*;
use libc::*;

include!(concat!(env!("OUT_DIR"), "/main.rs"));
