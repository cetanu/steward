use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = Path::new("proto/vendor");
    let out_dir = std::env::var("OUT_DIR")?;

    println!("cargo:rerun-if-changed={}", proto_root.display());

    tonic_prost_build::configure()
        .out_dir(out_dir)
        .build_client(true)
        .build_server(true)
        .disable_comments(["."])
        .boxed(".envoy.config.core.v3.AsyncDataSource.specifier.remote")
        .compile_protos(
            &["proto/vendor/envoy/service/ratelimit/v3/rls.proto"],
            &["proto/vendor"],
        )?;

    Ok(())
}
