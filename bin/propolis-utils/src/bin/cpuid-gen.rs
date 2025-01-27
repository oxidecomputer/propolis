// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::str::FromStr;

use clap::{Parser, ValueEnum};
use cpuid_utils::CpuidSet;

fn print_text(results: &CpuidSet) {
    for (key, value) in results.iter() {
        let header = match key.subleaf {
            None => {
                format!("eax:{:x}\t\t", key.leaf)
            }
            Some(subleaf) => {
                format!("eax:{:x} ecx:{:x}", key.leaf, subleaf)
            }
        };

        println!(
            "{} ->\t{:x} {:x} {:x} {:x}",
            header, value.eax, value.ebx, value.ecx, value.edx
        );
    }
}
fn print_toml(results: &CpuidSet) {
    println!("[cpuid]");
    for (key, value) in results.iter() {
        let key_name = match key.subleaf {
            None => format!("{:x}", key.leaf),
            Some(subleaf) => format!("{:x}-{:x}", key.leaf, subleaf),
        };
        println!(
            "\"{}\" = [0x{:x}, 0x{:x}, 0x{:x}, 0x{:x}]",
            key_name, value.eax, value.ebx, value.ecx, value.edx
        );
    }
}

fn print_json(results: CpuidSet) {
    let cpuid = results.into_instance_spec_cpuid();
    println!("{}", serde_json::to_string_pretty(&cpuid).unwrap());
}

#[derive(Default, Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    /// Print a human-readable plain-text representation.
    #[default]
    Text,

    /// Print TOML suitable for use in a propolis-standalone config file.
    Toml,

    /// Print JSON suitable for inclusion in a propolis-server instance spec.
    Json,
}

impl FromStr for OutputFormat {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "text" => Self::Text,
            "toml" => Self::Toml,
            "json" => Self::Json,
            _ => {
                return Err(
                    "invalid output format, must be text, toml, or json",
                )
            }
        })
    }
}

#[derive(clap::Parser)]
struct Opts {
    /// Elide all-zero entries from results
    #[clap(short)]
    zero_elide: bool,

    /// Emit output in the specified format
    #[clap(short, long, value_enum, default_value = "text")]
    format: OutputFormat,

    /// Query CPU directly, rather that via bhyve masking
    #[clap(short)]
    raw_query: bool,
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    let source = if opts.raw_query {
        cpuid_utils::host::CpuidSource::HostCpu
    } else {
        cpuid_utils::host::CpuidSource::BhyveDefault
    };

    let mut results = cpuid_utils::host::query_complete(source)?;
    if opts.zero_elide {
        results.retain(|_id, val| !val.all_zero());
    }

    match opts.format {
        OutputFormat::Text => print_text(&results),
        OutputFormat::Toml => print_toml(&results),
        OutputFormat::Json => print_json(results),
    }

    Ok(())
}
