// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::convert::{TryFrom, TryInto};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use propolis_standalone_config::SnapshotTag;

#[derive(Subcommand)]
enum Field {
    Global,
    Device {
        #[clap(value_parser)]
        matching_name: Option<String>,
    },
}

#[derive(clap::Parser)]
/// Propolis command-line frontend for running a VM.
struct Args {
    #[clap(subcommand)]
    field: Field,

    #[clap(value_parser)]
    input_file: String,
}

fn find_region(fp: &mut File, target: SnapshotTag) -> anyhow::Result<u64> {
    let mut buf = [0u8; 9];
    loop {
        fp.read_exact(&mut buf)?;
        let tag = SnapshotTag::try_from(buf[0])?;
        // The tokio writer uses BE, apparently
        let len = u64::from_be_bytes(buf[1..].try_into().unwrap());
        if tag == target {
            return Ok(len);
        }
        // Skip past the non-matching data
        fp.seek(SeekFrom::Current(len as i64))?;
    }
}

fn print_globals(mut fp: File, len: u64) -> anyhow::Result<()> {
    let mut buf = vec![0u8; len as usize];
    fp.read_exact(&mut buf[..])?;

    let data: serde_json::Value = serde_json::from_slice(&buf)?;

    println!("Global data:");
    println!("{:?}", data);

    Ok(())
}

#[derive(Deserialize)]
struct InstPayload {
    kind: String,
    version: u32,
    data: Vec<u8>,
}
#[derive(Serialize)]
struct PrettyPayload {
    kind: String,
    version: u32,
    data: serde_json::Value,
}
impl TryFrom<&InstPayload> for PrettyPayload {
    type Error = serde_json::error::Error;

    fn try_from(value: &InstPayload) -> Result<Self, Self::Error> {
        let data = serde_json::from_slice::<serde_json::Value>(&value.data)?;
        Ok(Self { kind: value.kind.clone(), version: value.version, data })
    }
}
#[derive(Deserialize)]
struct InstDevice {
    instance_name: String,
    payload: Vec<InstPayload>,
}

fn print_devs(
    mut fp: File,
    len: u64,
    matches: Option<String>,
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; len as usize];
    fp.read_exact(&mut buf[..])?;

    let data: Vec<InstDevice> = serde_json::from_slice(&buf)?;
    println!("Devices:");
    for InstDevice { instance_name, payload } in data.iter() {
        if let Some(filter) = matches.as_ref() {
            if !instance_name.contains(filter) {
                continue;
            }
        }
        println!("{}", instance_name);

        // Parse and pretty-ify the data
        for payload in payload.iter() {
            match PrettyPayload::try_from(payload) {
                Err(e) => {
                    eprintln!("Failed to parse payload for device: {}", e);
                }
                Ok(data) => {
                    let pretty = serde_json::to_string_pretty(&data)?;
                    println!("{}", pretty);
                }
            }
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let Args { field, input_file } = Args::parse();

    let mut fp = File::open(input_file).expect("Failed to open input");

    match field {
        Field::Global => {
            let len = find_region(&mut fp, SnapshotTag::Global)?;
            print_globals(fp, len)?;
        }
        Field::Device { matching_name } => {
            let len = find_region(&mut fp, SnapshotTag::Device)?;
            print_devs(fp, len, matching_name)?;
        }
    }

    Ok(())
}
