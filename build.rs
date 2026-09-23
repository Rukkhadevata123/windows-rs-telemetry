// 构建时从 Windows 元数据生成窄绑定，只包含过滤清单里的 API 及其依赖类型。
// 生成结果位于 Cargo 提供的 OUT_DIR。PDH 使用 C 风格句柄，DXGI 使用 COM 接口，
// 因此分别生成绑定以免混用模式。
fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    windows_bindgen::bindgen([
        "--out",
        &format!("{out}/bindings.rs"),
        // --flat：不按 Windows 命名空间嵌套模块，全部放在同一层
        "--flat",
        // --sys：生成 windows-sys 风格的原始 FFI（裸函数、C 结构体），不带 Result 等高层包装
        "--sys",
        "--filter",
        "GlobalMemoryStatusEx",
        "GetSystemTimes",
        // 基础电池状态及电池侧充放电功率。
        "GetSystemPowerStatus",
        "CallNtPowerInformation",
        "SystemBatteryState",
        "SYSTEM_BATTERY_STATE",
        // 从采样线程通知 Win32 窗口。
        "PostMessageW",
        "WM_APP",
        "WM_DPICHANGED",
        "WM_CLOSE",
        // 请求/确认重绘、读取物理尺寸、处理 DPI 变化。
        "InvalidateRect",
        "BeginPaint",
        "EndPaint",
        "GetClientRect",
        "GetDpiForWindow",
        "SystemParametersInfoW",
        "SPI_GETWORKAREA",
        "IsIconic",
        "SetWindowPos",
        "SWP_NOZORDER",
        "SWP_NOACTIVATE",
        "WM_PAINT",
        "WM_SIZE",
        "WM_ERASEBKGND",
        // 枚举网络接口；系统分配的表必须交回 FreeMibTable。
        "GetIfTable2",
        "FreeMibTable",
        "IF_TYPE_IEEE80211",
        "IfOperStatusUp",
        "IfOperStatusDown",
        "IfOperStatusTesting",
        "IfOperStatusUnknown",
        "IfOperStatusDormant",
        "IfOperStatusNotPresent",
        "IfOperStatusLowerLayerDown",
        // NVMe：先读取设备描述符以识别总线和型号，再查询只读 SMART 日志。
        "CreateFileW",
        "CloseHandle",
        "DeviceIoControl",
        "STORAGE_DEVICE_DESCRIPTOR",
        "STORAGE_PROPERTY_QUERY",
        "STORAGE_PROTOCOL_SPECIFIC_DATA",
        "STORAGE_PROTOCOL_DATA_DESCRIPTOR",
        "StorageDeviceProperty",
        "StorageDeviceProtocolSpecificProperty",
        "PropertyStandardQuery",
        "BusTypeNvme",
        "ProtocolTypeNvme",
        "NVMeDataTypeLogPage",
        "IOCTL_STORAGE_QUERY_PROPERTY",
    ]);
    windows_bindgen::bindgen([
        "--out",
        &format!("{out}/pdh.rs"),
        "--flat",
        "--sys",
        "--filter",
        "PdhOpenQueryW",
        "PdhCloseQuery",
        "PdhAddEnglishCounterW",
        "PdhCollectQueryData",
        "PdhGetFormattedCounterValue",
        "PdhGetFormattedCounterArrayW",
        "PDH_FMT_DOUBLE",
    ]);
    windows_bindgen::bindgen([
        "--out",
        &format!("{out}/dxgi.rs"),
        "--flat",
        "--filter",
        "CreateDXGIFactory1",
        "IDXGIFactory1",
        "IDXGIAdapter1",
    ]);
    println!("cargo:rerun-if-changed=build.rs");
}
