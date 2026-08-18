//! prost-build 在 OUT_DIR/kulua.direct.rs 生成的 protobuf 类型（proto/direct.proto）。
//! 生成逻辑见 kulua-proto/build.rs。类型平铺在 `crate::generated::` 下（无包嵌套）。
#![allow(clippy::all)]
include!(concat!(env!("OUT_DIR"), "/kulua.direct.rs"));
