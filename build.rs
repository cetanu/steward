use rayon::prelude::*;
use regex::Regex;
use reqwest::blocking::get;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use zip::ZipArchive;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Package<'a> {
    url: &'a str,
    directory: &'a str,
    namespaces: std::collections::HashMap<&'a str, &'a str>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Which Envoy protos to use
    let envoy_version = "1.25.0";
    let envoy_url =
        format!("https://github.com/envoyproxy/envoy/archive/refs/tags/v{envoy_version}.zip");
    let envoy_directory = format!("envoy-{envoy_version}/api");

    // Protobuf trees to download
    let packages = vec![
        Package {
            url: &envoy_url,
            namespaces: vec![("envoy", "envoy")]
                .into_iter()
                .collect(),
            directory: &envoy_directory,
        },
        Package {
            url: "https://github.com/googleapis/googleapis/archive/master.zip",
            namespaces: vec![("google", "google")]
                .into_iter()
                .collect(),
            directory: "googleapis-master",
        },
        Package{
            url:"https://github.com/envoyproxy/protoc-gen-validate/archive/main.zip",
            namespaces: vec![("validate", "validate")].into_iter().collect(),
            directory:"protoc-gen-validate-main",
        },
        Package{
            url:"https://github.com/census-instrumentation/opencensus-proto/archive/refs/tags/v0.2.0.zip",
            namespaces: vec![("opencensus", "opencensus")].into_iter().collect(),
            directory:"opencensus-proto-0.2.0/src",
        },
        Package{
            url:"https://github.com/prometheus/client_model/archive/refs/tags/v0.2.0.zip",
            namespaces: vec![(".", "prometheus")].into_iter().collect(),
            directory:"client_model-0.2.0",
        },
        Package{
            url:"https://github.com/cncf/xds/archive/refs/heads/main.zip",
            namespaces: vec![("xds", "xds"), ("udpa", "udpa")].into_iter().collect(),
            directory:"xds-main",
        },
    ];

    // Move into build directory to keep project clean
    let build_directory = Path::new("proto");
    if !build_directory.exists() {
        println!("Creating proto directory");
        fs::create_dir(build_directory).expect("Unable to create proto directory");
    }
    std::env::set_current_dir(build_directory).expect("Unable to change to proto directory");

    // Download, extract, and clean up
    download_protobufs(packages);

    // Compile protocol buffers
    tonic_build::configure()
        .build_client(false)
        .compile(&["envoy/service/ratelimit/v3/rls.proto"], &["."])?;
    Ok(())
}

fn download_protobufs(packages: Vec<Package>) {
    let pattern = r"github\.com/[a-zA-Z0-9-_]+/(?P<name>[a-zA-Z0-9-_]+)";
    let repo_name_pattern = Regex::new(pattern).unwrap();

    packages.par_iter().for_each(|package| {
        let package_name: String = repo_name_pattern
            .captures_iter(package.url)
            .take(1)
            .map(|item| item["name"].to_string())
            .collect();
        let name = Path::new(&package_name);
        let keep_name = format!("_{package_name}.keep");
        let keep = Path::new(&keep_name);
        let proto_root = Path::new(&package.directory);
        let archive_name = format!("{package_name}.zip");
        let archive = Path::new(&archive_name);

        println!("cargo:rerun-if-changed={}", keep.display());
        if keep.exists() {
            println!(
                "{:?} exists; remove to redownload {:?}",
                keep.canonicalize().unwrap(),
                name
            );
        } else {
            if !archive.exists() {
                println!("Downloading {package_name} protocol buffers from Github");
                let mut zf = File::create(archive).expect("Unable to create zip file");
                let content = get(package.url)
                    .expect("Failed to fetch package URL")
                    .bytes()
                    .expect("Failed to read package URL bytes");
                zf.write_all(&content)
                    .expect("Unable to write fetched content to zip file");
            }

            println!("Extracting {package_name} archive");
            let mut zip = ZipArchive::new(File::open(archive).expect("Unable to open archive"))
                .expect("Unable to read zip archive");
            for i in 0..zip.len() {
                let mut file = zip.by_index(i).expect("Unable to access zip file by index");
                if file.name().ends_with(".proto") {
                    // println!("Extracting {}", file.name());
                    let outpath = file.mangled_name();
                    if file.name().ends_with('/') {
                        fs::create_dir_all(&outpath).expect("Unable to create directory");
                    } else {
                        if let Some(parent) = outpath.parent() {
                            if !parent.exists() {
                                fs::create_dir_all(parent)
                                    .expect("Unable to create parent directory");
                            }
                        }
                        let mut outfile =
                            File::create(&outpath).expect("Unable to create output file");
                        std::io::copy(&mut file, &mut outfile)
                            .expect("Unable to copy from zip to output file");
                    }
                }
            }

            // Move subfolder/namespace into proper directory
            // so that proto-compiler can find them as relative paths
            for (namespace, destination) in &package.namespaces {
                let namespace_path = proto_root.join(namespace);
                let destination_path = Path::new(destination);

                println!(
                    "Moving {} to {}",
                    namespace_path.display(),
                    destination_path.display()
                );

                if destination_path.exists() {
                    fs::remove_dir_all(destination_path)
                        .expect("Unable to remove destination directory");
                }
                fs::create_dir_all(destination_path)
                    .expect("Unable to create destination directory");

                for entry in fs::read_dir(namespace_path).unwrap() {
                    let entry = entry.unwrap();
                    let entry_path = entry.path();
                    let dest_path = destination_path.join(entry_path.file_name().unwrap());

                    if entry_path.is_dir() {
                        println!(
                            "Moving inner {} to {}",
                            entry_path.display(),
                            destination_path.display()
                        );
                        fs_extra::dir::copy(
                            entry_path,
                            destination_path,
                            &fs_extra::dir::CopyOptions::new(),
                        )
                        .unwrap();
                    } else {
                        println!(
                            "Moving inner {} to {}",
                            entry_path.display(),
                            dest_path.display()
                        );
                        fs::copy(entry_path, dest_path).unwrap();
                    }
                }
            }

            // Clean-up downloaded + extracted files
            println!("Removing {}", proto_root.display());
            fs::remove_dir_all(proto_root).unwrap_or_default();
            fs::remove_file(archive).unwrap_or_default();

            // Prevent re-downloads
            let start = SystemTime::now();
            let timestamp = start
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards");
            File::create(keep).expect("Unable to create keep file");
            fs::write(keep, format!("Last downloaded: {timestamp:?}"))
                .expect("Unable to write keep file");
        }
    });
}
