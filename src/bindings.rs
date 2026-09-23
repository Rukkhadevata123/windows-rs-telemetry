// 私有生成代码。生成器使用 Win32 原名（MEMORYSTATUSEX、dwLength 等），关掉命名风格警告。
// 三份绑定的生成模式不同，见 build.rs。
#![allow(
    dead_code,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    clippy::upper_case_acronyms
)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// PDH 的 C 风格句柄 API。
pub(crate) mod pdh {
    include!(concat!(env!("OUT_DIR"), "/pdh.rs"));
}

/// DXGI 的 COM 接口，只用于枚举物理适配器的 LUID。
pub(crate) mod dxgi {
    include!(concat!(env!("OUT_DIR"), "/dxgi.rs"));
}
