# Windows telemetry

一个 Windows 桌面遥测程序，提供两个独立窗口入口：Win32 + Canvas 显示最近 60 秒的 CPU、内存和 WLAN 曲线；WinUI 3 + Reactor 显示自动刷新的指标卡片。两个入口共用采样、数据模型和设备选择代码。

## 构建与运行

需要 Windows、Rust 工具链，以及已缓存或可下载的 `windows-rs` 0.100 依赖。WinUI 入口还需要本机可用的 Windows App Runtime。

```powershell
cargo run --locked --bin windows-rs-telemetry
cargo run --locked --features reactor --bin telemetry-reactor
```

已缓存全部依赖时可在命令中加 `--offline`。使用 `cargo build --release --locked` 构建 Win32 exe；使用 `cargo build --release --locked --features reactor` 同时构建两个 exe。

两个图形入口默认均使用 Windows 图形子系统，双击相应 exe 时直接打开图形窗口。需要在终端查看帮助、设备列表或诊断输出时，添加 `console` feature 构建并运行，例如：

```powershell
cargo run --locked --features console --bin windows-rs-telemetry -- --help
cargo run --locked --features console --bin windows-rs-telemetry -- --list-disks
cargo run --locked --features "reactor,console" --bin telemetry-reactor -- --help
```

两个入口接受相同的选项：

```text
--list-network          列出网络接口
--list-disks            列出 NVMe 设备
--interface 名称        选择物理 WLAN 接口
--disk 路径             选择 NVMe 物理磁盘，例如 \\.\PhysicalDrive0
--help                  显示帮助
```

程序按设备类别独立选择：每类只有一个候选时自动选择，也可用参数指定。枚举失败或候选不唯一时，其他指标仍可显示；指定无效设备会报错。设备编号可能变化，先用列表命令确认当前路径。

## 读数含义

- CPU 使用率来自 `GetSystemTimes` 的相邻样本差分。CPU 频率由 PDH `Processor Information(_Total)` 的基准频率和性能百分比估算，表示整机聚合读数。
- 内存显示 Windows 可见物理内存的已用量和总量。
- WLAN 吞吐量按选中接口累计字节数的相邻采样差分计算，单位 B/s，界面以 KiB/s 或 MiB/s 显示。
- 磁盘读写速率来自 PDH `PhysicalDisk(_Total)`，汇总所有物理磁盘。GPU 百分比取最忙的单个进程/引擎实例利用率。
- NVMe SMART 每 30 秒进行只读查询，显示复合温度、可用备用空间百分比、备用阈值、已用寿命估计及警告位。
- 电池状态与电池侧充放电功率每 3 秒更新，正值表示充电，负值表示放电。

各指标独立报告错误，过期读数会显示为过期。Win32 窗口初始为 1400×1480 DIP，按主屏 DPI 换算成物理像素并限制在工作区内；中等高度按比例绘制曲线，极小窗口显示文字摘要。WinUI 页面支持滚动。程序在后台每秒采样，关闭窗口时等待采样线程退出。

## 验证

```powershell
cargo fmt --all -- --check
cargo test --locked --offline --all-features
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
```
