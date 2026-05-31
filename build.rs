#[cfg(feature = "payload-contract-benchmarks")]
use std::{env, fs, path::Path};

fn main() {
    println!("cargo:rerun-if-changed=benches/proto/bench_payload.proto");
    generate_bench_proto();
}

#[cfg(feature = "payload-contract-benchmarks")]
fn generate_bench_proto() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set");
    let proto_dir = Path::new(&manifest_dir).join("benches/proto");
    let proto_file = proto_dir.join("bench_payload.proto");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR is set");

    protobuf_codegen::Codegen::new()
        .protoc()
        .protoc_path(&protoc_bin_vendored::protoc_bin_path().expect("vendored protoc path"))
        .include(&proto_dir)
        .inputs([proto_file])
        .out_dir(&out_dir)
        .run_from_script();
    sanitize_generated_proto(Path::new(&out_dir).join("bench_payload.rs"));
}

#[cfg(feature = "payload-contract-benchmarks")]
fn sanitize_generated_proto(path: impl AsRef<Path>) {
    let path = path.as_ref();
    let content =
        fs::read_to_string(path).expect("generated benchmark protobuf should be readable");
    let mut sanitized = String::with_capacity(content.len());
    for line in content.lines() {
        if line.starts_with("#![") {
            continue;
        }
        if let Some(comment) = line.strip_prefix("//!") {
            sanitized.push_str("//");
            sanitized.push_str(comment);
        } else {
            sanitized.push_str(line);
        }
        sanitized.push('\n');
    }
    fs::write(path, sanitized).expect("generated benchmark protobuf should be writable");
}

#[cfg(not(feature = "payload-contract-benchmarks"))]
fn generate_bench_proto() {}
