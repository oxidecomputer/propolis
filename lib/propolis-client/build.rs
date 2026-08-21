// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    env,
    fs::{self, File},
    path::Path,
};

fn main() {
    let src = "../../openapi/propolis-server/propolis-server-latest.json";
    println!("cargo:rerun-if-changed={}", src);
    let file = File::open(src).unwrap();
    let spec = serde_json::from_reader(file).unwrap();
    let mut generator = progenitor::Generator::new(
        progenitor::GenerationSettings::default()
            .with_interface(progenitor::InterfaceStyle::Builder)
            .with_tag(progenitor::TagStyle::Separate)
            .with_replacement(
                "PciPath",
                "propolis_api_types_versions::latest::instance_spec::PciPath",
                [].into_iter(),
            )
            .with_replacement(
                "ReplacementComponent",
                "propolis_api_types_versions::latest::instance::ReplacementComponent",
                [].into_iter(),
            )
            .with_replacement(
                "InstanceSpec",
                "propolis_api_types_versions::latest::instance_spec::InstanceSpec",
                [].into_iter(),
            )
            .with_replacement(
                "InstanceSpecStatus",
                "propolis_api_types_versions::latest::instance_spec::InstanceSpecStatus",
                [].into_iter(),
            )
            .with_replacement(
                "InstanceProperties",
                "propolis_api_types_versions::latest::instance::InstanceProperties",
                [].into_iter(),
            )
            .with_replacement(
                "InstanceMetadata",
                "propolis_api_types_versions::latest::instance::InstanceMetadata",
                [].into_iter(),
            )
            .with_replacement(
                "InstanceSpecGetResponse",
                "propolis_api_types_versions::latest::instance_spec::InstanceSpecGetResponse",
                [].into_iter(),
            )
            .with_replacement(
                "Component",
                "propolis_api_types_versions::latest::instance_spec::Component",
                [].into_iter(),
            )
            .with_replacement(
                "SmbiosType1Input",
                "propolis_api_types_versions::latest::instance_spec::SmbiosType1Input",
                [].into_iter(),
            )
            .with_replacement(
                "VersionedInstanceSpec",
                "propolis_api_types_versions::latest::instance_spec::VersionedInstanceSpec",
                [].into_iter(),
            )
            .with_replacement(
                "CpuidEntry",
                "propolis_api_types_versions::latest::components::board::CpuidEntry",
                [].into_iter(),
            )
            .with_patch("BootSettings", progenitor::TypePatch::default().with_derive("Default"))
            .with_patch("CpuidEntry", progenitor::TypePatch::default().with_derive("PartialEq").with_derive("Eq").with_derive("Copy"))
            .with_patch("InstanceMetadata", progenitor::TypePatch::default().with_derive("PartialEq"))
            .with_patch("SpecKey", progenitor::TypePatch::default().with_derive("PartialEq").with_derive("Eq").with_derive("Ord").with_derive("PartialOrd").with_derive("Hash")),
    );

    let tokens = generator.generate_tokens(&spec).unwrap();
    let ast = syn::parse2(tokens).unwrap();
    let content = prettyplease::unparse(&ast);

    let mut out_file = Path::new(&env::var("OUT_DIR").unwrap()).to_path_buf();
    out_file.push("codegen.rs");

    fs::write(out_file, content).unwrap();
}
