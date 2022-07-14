use std::convert::{TryFrom, TryInto};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use clap::{Parser, Subcommand};

use propolis_standalone_shared as shared;
use shared::SnapshotTag;

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

    let data: propolis::vmm::migrate::BhyveVmV1 = serde_json::from_slice(&buf)?;

    println!("Global entries:");
    if !data.arch_entries.is_empty() {
        for ent in data.arch_entries {
            println!("{:08X} = {}", ent.ident, ent.value);
        }
    } else {
        println!("None");
    }

    Ok(())
}

fn print_devs(
    mut fp: File,
    len: u64,
    matches: Option<String>,
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; len as usize];
    fp.read_exact(&mut buf[..])?;

    let data: Vec<(String, Vec<u8>)> = serde_json::from_slice(&buf)?;
    println!("Devices:");
    for (name, dev_json) in data.iter() {
        if let Some(filter) = matches.as_ref() {
            if !name.contains(filter) {
                continue;
            }
        }
        println!("{}", name);
        // Parse and pretty-ify the data
        let parsed: serde_json::Value = serde_json::from_slice(dev_json)?;
        let pretty = serde_json::to_string_pretty(&parsed)?;
        println!("{}", pretty);
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
