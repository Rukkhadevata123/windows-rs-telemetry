# Windows telemetry

一个 Windows 桌面遥测程序，提供两个独立窗口入口：Win32 + Canvas 显示最近 60 秒的 CPU、内存和 WLAN 曲线；WinUI 3 + Reactor 显示自动刷新的指标卡片。两个入口共用采样、数据模型和设备选择代码。

## 构建与运行

需要 Windows、Rust 工具链，以及已缓存或可下载的 `windows-rs` 0.100 依赖。WinUI 入口还需要本机可用的 Windows App Runtime。

```powershell
cargo run --locked --bin windows-rs-telemetry
cargo run --locked --features reactor --bin telemetry-reactor
```

已缓存全部依赖时可在命令中加 `--offline`。正式构建使用 `cargo build --release --locked`；若需要 WinUI 入口，使用 `cargo build --release --locked --features reactor`。默认构建不包含 Reactor。

默认构建的两个图形 exe 使用 Windows 图形子系统，双击启动时不会弹出控制台窗口。需要在终端查看帮助、设备列表或诊断输出时，添加 `console` feature 构建并运行，例如：

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

没有显式指定设备时，程序只会自动选择唯一的物理 WLAN 或 NVMe。枚举失败或没有唯一设备时，其他指标仍可显示；显式指定无效设备会报错。设备编号可能变化，先用列表命令确认当前路径。

## 读数含义

- CPU 使用率来自 `GetSystemTimes` 的相邻样本差分。频率由 PDH `Processor Information(_Total)` 计数器估算，表示聚合读数，不是单核瞬时时钟。
- 内存是 Windows 可见的物理内存已用量与总量。
- WLAN 速率由选中接口的累计字节差分得到，内部单位为 B/s，界面按 KiB/s 或 MiB/s 显示；连接速率和互联网测速不在此列。
- 磁盘读写速率来自 PDH `PhysicalDisk(_Total)`，代表所有物理磁盘的合计。GPU 百分比是最忙的单个进程/引擎实例，不能当作整卡利用率。
- NVMe SMART 每 30 秒读取一次，包含复合温度、可用备用空间、阈值、已用寿命估计及警告位。备用空间百分比是设备保留的备用容量指标，不是文件系统剩余空间。查询是只读的。
- 电池状态与功率每 3 秒更新。正功率表示电池充电、负功率表示电池放电；它不是整机输入功率。

各指标独立报告错误，过期读数会显示为过期。Win32 窗口初始为 1400×1480 DIP，按主屏 DPI 换算成物理像素并限制在工作区内；中等高度按比例绘制曲线，极小窗口显示文字摘要。WinUI 页面支持滚动。程序在后台每秒采样，关闭窗口时等待采样线程退出。

## 验证

```powershell
cargo fmt --all -- --check
cargo test --locked --offline --all-features
cargo clippy --locked --offline --all-targets --all-features -- -D warnings
```
