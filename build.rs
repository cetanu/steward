fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure().build_client(false).compile(
        &[
            // "proto/envoy/extensions/common/ratelimit/v3/ratelimit.proto",
            "proto/envoy/service/ratelimit/v3/rls.proto",
        ],
        &["proto/"],
    )?;
    Ok(())
}
