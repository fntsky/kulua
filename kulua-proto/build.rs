//! 从 `proto/direct.proto` 生成 Rust 代码到 OUT_DIR，再在 src/generated.rs 里 include。
//!
//! 需要 `protoc`（PROTOC 环境变量或 PATH）。Windows 上用户已装 protoc（anaconda 自带）；
//! Linux 上可用 `protobuf-compiler` 或自行下载。

fn main() {
    // 重新生成条件：.proto 或本文件变化
    println!("cargo:rerun-if-changed=../proto/direct.proto");
    println!("cargo:rerun-if-changed=build.rs");

    let proto_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../proto");
    let proto_file = proto_dir.join("direct.proto");
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let mut config = prost_build::Config::new();
    config.out_dir(&out_dir);
    config
        .compile_protos(&[proto_file.as_os_str().to_os_string()], &[proto_dir])
        .expect("proto 编译失败（需要 PATH 或 PROTOC 环境变量指向 protoc）");
}
