fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");

    tonic_prost_build::configure()
        .build_client(false)
        .disable_comments(["."])
        .boxed(".envoy.config.core.v3.AsyncDataSource.specifier.remote")
        .compile_protos(&["proto/envoy/service/ratelimit/v3/rls.proto"], &["proto"])?;

    Ok(())
}
