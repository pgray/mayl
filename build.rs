fn main() {
    tonic_build::compile_protos("proto/bridge.proto").unwrap();
}
