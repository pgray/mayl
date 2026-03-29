fn main() {
    tonic_build::configure()
        .type_attribute(".", "#[allow(clippy::enum_variant_names)]")
        .compile_protos(&["proto/bridge.proto"], &["proto"])
        .unwrap();
}
