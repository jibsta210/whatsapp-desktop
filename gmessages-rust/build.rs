// Build script for gmessages-rust.
//
// By default this is a no-op: the generated `gmproto.rs` is checked into
// `src/` so a normal build doesn't require `protoc`.
//
// To regenerate from the .proto files (after copying fresh ones from
// mautrix-gmessages), run:
//
//     GENERATE_PROTO=1 cargo build -p gmessages-rust
//
// This requires `protoc` to be installed on the system.

fn main() -> std::io::Result<()> {
    if std::env::var("GENERATE_PROTO").is_err() {
        println!("cargo:rerun-if-changed=build.rs");
        return Ok(());
    }

    println!("cargo:rerun-if-changed=proto/");
    println!("cargo:warning=GENERATE_PROTO is set, regenerating gmproto definitions...");

    let proto_files = [
        "proto/authentication.proto",
        "proto/client.proto",
        "proto/config.proto",
        "proto/conversations.proto",
        "proto/events.proto",
        "proto/rpc.proto",
        "proto/settings.proto",
        "proto/ukey.proto",
        "proto/util.proto",
        "proto/vendor/pblite.proto",
    ];

    let mut config = prost_build::Config::new();
    config.type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]");
    config.out_dir("src/gmproto/");

    // Also dump a FileDescriptorSet next to the generated code so prost-reflect
    // can walk fields (needed for the PBLite encoder/decoder, which needs to
    // consult the [(pblite.pblite_binary)] field option).
    config.file_descriptor_set_path("src/gmproto/file_descriptor_set.bin");

    // Hook prost-reflect-build so each generated message gets a
    // `ReflectMessage` impl (we need this to fetch FieldDescriptors at
    // runtime for the PBLite encoder).
    prost_reflect_build::Builder::new()
        .file_descriptor_set_bytes("crate::PBLITE_FILE_DESCRIPTOR_SET_BYTES")
        .compile_protos_with_config(config, &proto_files, &["proto/", "proto/vendor/"])?;
    Ok(())
}
