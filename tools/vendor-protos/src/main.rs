use reqwest::blocking::Client;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const ENTRY: &str = "envoy/service/ratelimit/v3/rls.proto";
const SOURCES: &[(&str, &str)] = &[
    (
        "envoy/",
        "https://raw.githubusercontent.com/envoyproxy/envoy/v1.39.0/api/",
    ),
    (
        "google/protobuf/",
        "https://raw.githubusercontent.com/protocolbuffers/protobuf/v3.21.12/src/",
    ),
    (
        "google/",
        "https://raw.githubusercontent.com/googleapis/googleapis/master/",
    ),
    (
        "validate/",
        "https://raw.githubusercontent.com/bufbuild/protoc-gen-validate/v1.3.3/",
    ),
    (
        "opencensus/",
        "https://raw.githubusercontent.com/census-instrumentation/opencensus-proto/v0.2.0/src/",
    ),
    ("xds/", "https://raw.githubusercontent.com/cncf/xds/main/"),
    ("udpa/", "https://raw.githubusercontent.com/cncf/xds/main/"),
    (
        "prometheus/",
        "https://raw.githubusercontent.com/prometheus/client_model/v0.2.0/",
    ),
];

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let vendor = root.join("proto/vendor");
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent("steward-proto-vendor")
        .build()?;

    let mut pending = vec![ENTRY.to_owned()];
    let mut protos = HashMap::<String, Vec<u8>>::new();
    while let Some(proto) = pending.pop() {
        if protos.contains_key(&proto) {
            continue;
        }
        validate_proto_path(&proto)?;
        let url = source_url(&proto)?;
        let contents = client
            .get(url)
            .send()?
            .error_for_status()?
            .bytes()?
            .to_vec();
        let text = std::str::from_utf8(&contents)?;
        pending.extend(
            imports(text)
                .into_iter()
                .filter(|import| !protos.contains_key(import)),
        );
        protos.insert(proto, contents);
    }

    // Fetch the full dependency tree before changing the checked-in files.
    if vendor.exists() {
        for entry in walk_proto_files(&vendor)? {
            let relative = entry.strip_prefix(&vendor)?.to_string_lossy().into_owned();
            if !protos.contains_key(&relative) {
                fs::remove_file(entry)?;
            }
        }
    }
    for (proto, contents) in &protos {
        let destination = vendor.join(proto);
        fs::create_dir_all(destination.parent().expect("proto has a parent"))?;
        fs::write(destination, contents)?;
    }

    let bytes: usize = protos.values().map(Vec::len).sum();
    println!("Vendored {} protobuf files ({bytes} bytes)", protos.len());
    Ok(())
}

fn source_url(proto: &str) -> Result<String, Box<dyn Error>> {
    SOURCES
        .iter()
        .find_map(|(prefix, base)| {
            proto
                .strip_prefix(prefix)
                .map(|path| format!("{base}{path}"))
        })
        .ok_or_else(|| format!("no upstream source configured for {proto}").into())
}

fn imports(proto: &str) -> Vec<String> {
    proto
        .lines()
        .filter_map(|line| {
            let import = line.trim().strip_prefix("import ")?.trim_start();
            let import = import
                .strip_prefix("public ")
                .or_else(|| import.strip_prefix("weak "))
                .unwrap_or(import)
                .trim_start();
            let start = import.find('"')? + 1;
            let end = import[start..].find('"')? + start;
            Some(import[start..end].to_owned())
        })
        .collect()
}

fn validate_proto_path(proto: &str) -> Result<(), Box<dyn Error>> {
    if Path::new(proto)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("invalid proto import path: {proto}").into());
    }
    Ok(())
}

fn walk_proto_files(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() {
                directories.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "proto")
            {
                files.push(path);
            }
        }
    }
    Ok(files)
}
