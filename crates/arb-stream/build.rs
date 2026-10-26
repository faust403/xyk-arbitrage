//! Compile shredstream.proto via tonic-build. Only invoked when the
//! `shredstream` feature is enabled — guards the cfg-attr below so that
//! the default build (e.g. Windows dev path) doesn't try to find protoc.
fn main() {
    #[cfg(feature = "shredstream")]
    {
        let proto = "proto/shredstream.proto";
        println!("cargo:rerun-if-changed={proto}");
        if let Err(e) = tonic_build::configure()
            .build_server(false)
            .compile(&[proto], &["proto"])
        {
            // Don't fail the build outright — without protoc the feature
            // simply won't be available at runtime. Print so it's visible.
            println!("cargo:warning=tonic_build failed: {e} (shredstream client unavailable)");
        }
    }
}
