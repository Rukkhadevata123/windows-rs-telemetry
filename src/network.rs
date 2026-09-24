//! 复制网络接口快照后，立即释放 Windows 分配的表。
use std::{io, ptr, time::Instant};

use crate::bindings::{
    FreeMibTable, GetIfTable2, IF_OPER_STATUS, IF_TYPE_IEEE80211, IfOperStatusDormant,
    IfOperStatusDown, IfOperStatusLowerLayerDown, IfOperStatusNotPresent, IfOperStatusTesting,
    IfOperStatusUnknown, IfOperStatusUp, MIB_IF_ROW2, MIB_IF_TABLE2,
};
use crate::sampling::MAX_SAMPLE_GAP;

#[derive(Debug)]
pub struct InterfaceSnapshot {
    /// LUID 用于跨采样周期关联接口；接口 index 可能变化。
    pub luid: u64,
    pub index: u32,
    pub name: String,
    pub description: String,
    pub is_hardware: bool,
    pub is_wifi: bool,
    pub oper_status: IF_OPER_STATUS,
    pub received_bytes: u64,
    pub sent_bytes: u64,
}

/// 唯一拥有 Windows 分配的系统表，Drop 时用 FreeMibTable 释放。
struct InterfaceTable(*mut MIB_IF_TABLE2);

impl Drop for InterfaceTable {
    fn drop(&mut self) {
        // SAFETY：只包装 GetIfTable2 成功返回的表，未复制所有权，恰好释放一次。
        unsafe { FreeMibTable(self.0.cast()) };
    }
}

pub fn collect_interfaces() -> io::Result<Vec<InterfaceSnapshot>> {
    let mut raw = ptr::null_mut();
    // SAFETY：传入有效输出指针；Windows 分配表并把地址写入 raw。
    let status = unsafe { GetIfTable2(&mut raw) };
    if status != 0 {
        // GetIfTable2 直接返回错误码，用返回值构造 io::Error。
        return Err(io::Error::from_raw_os_error(status));
    }
    let table = InterfaceTable(raw);
    let mut interfaces = Vec::new();
    // SAFETY：成功返回的表包含 NumEntries 行；按生成结构体定位首行并跨过对齐填充。
    // Table 的 [ROW; 1] 是 C 可变长尾数组的占位；实际行数由 NumEntries 决定。
    unsafe {
        let first = ptr::addr_of!((*table.0).Table).cast::<MIB_IF_ROW2>();
        for index in 0..(*table.0).NumEntries as usize {
            let row = first.add(index);
            // bindgen 0.100 把这组 8 个 BOOLEAN 位域生成为 bool，但原始字节可能 > 1。
            // 只读取字节及其他有效字段，不构造整行的 Rust 引用或读取那个 bool。
            let flags = ptr::addr_of!((*row).InterfaceAndOperStatusFlags)
                .cast::<u8>()
                .read();
            interfaces.push(InterfaceSnapshot {
                luid: (*row).InterfaceLuid.Value,
                index: (*row).InterfaceIndex,
                name: utf16(&(*row).Alias),
                description: utf16(&(*row).Description),
                is_hardware: flags & 1 != 0, // HardwareInterface 是最低位。
                is_wifi: (*row).Type == IF_TYPE_IEEE80211 as u32,
                oper_status: (*row).OperStatus,
                received_bytes: (*row).InOctets,
                sent_bytes: (*row).OutOctets,
            });
        }
    }
    // 返回的 Vec/String/u64 都属于 Rust；table 在此离开作用域，释放系统表。
    Ok(interfaces)
}

#[expect(non_upper_case_globals, reason = "模式匹配使用 Win32 原名的状态常量")]
fn status_label(state: IF_OPER_STATUS) -> &'static str {
    match state {
        IfOperStatusUp => "Up（可传输）",
        IfOperStatusDown => "Down（不可传输）",
        IfOperStatusTesting => "Testing（测试中）",
        IfOperStatusUnknown => "Unknown（未知）",
        IfOperStatusDormant => "Dormant（等待连接）",
        IfOperStatusNotPresent => "NotPresent（设备不在场）",
        IfOperStatusLowerLayerDown => "LowerLayerDown（下层断开）",
        _ => "未知状态",
    }
}

fn utf16(value: &[u16]) -> String {
    let end = value.iter().position(|&c| c == 0).unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

pub fn print_interfaces() -> io::Result<()> {
    let interfaces = collect_interfaces()?;
    println!("网络接口快照：累计字节，不是实时速度\n");
    for interface in &interfaces {
        println!("{} — {}", interface.name, interface.description);
        println!(
            "  LUID={:#018x}  index={}  {}  硬件={}  WLAN={}",
            interface.luid,
            interface.index,
            status_label(interface.oper_status),
            interface.is_hardware,
            interface.is_wifi
        );
        println!(
            "  接收={} B  发送={} B",
            interface.received_bytes, interface.sent_bytes
        );
    }
    if let Some(interface) = select_interface(&interfaces, None)? {
        println!(
            "\n本次选择：{}（{}），LUID={:#018x}",
            interface.name, interface.description, interface.luid
        );
    } else {
        println!("\n没有唯一的物理 WLAN 接口；可用 --interface 名称 明确选择。");
    }
    Ok(())
}

pub fn select_interface<'a>(
    interfaces: &'a [InterfaceSnapshot],
    requested_name: Option<&str>,
) -> io::Result<Option<&'a InterfaceSnapshot>> {
    let candidates: Vec<_> = interfaces
        .iter()
        .filter(|i| i.is_hardware && i.is_wifi)
        .collect();
    let selected = if let Some(name) = requested_name {
        let normalized = name.to_lowercase();
        Some(
            candidates
                .iter()
                .copied()
                .find(|i| i.name.to_lowercase() == normalized)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("未找到名为 {name:?} 的物理 WLAN 接口"),
                    )
                })?,
        )
    } else if candidates.len() == 1 {
        Some(candidates[0])
    } else {
        None
    };
    Ok(selected)
}

#[derive(Debug, PartialEq)]
pub enum NetworkUsage {
    NotSelected,
    Pending,
    Disconnected,
    Unavailable,
    Transfer {
        download_bytes_per_sec: f64,
        upload_bytes_per_sec: f64,
    },
}

struct Counters {
    at: Instant,
    received: u64,
    sent: u64,
}

/// 只负责一张选定接口的差分。读取系统表和显示结果留给调用者。
#[derive(Default)]
pub struct NetworkSampler {
    last: Option<Counters>,
}

impl NetworkSampler {
    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn update(&mut self, interface: Option<&InterfaceSnapshot>, at: Instant) -> NetworkUsage {
        let Some(interface) = interface else {
            self.reset();
            return NetworkUsage::Unavailable;
        };
        if interface.oper_status != IfOperStatusUp {
            self.reset();
            return NetworkUsage::Disconnected;
        }
        let now = Counters {
            at,
            received: interface.received_bytes,
            sent: interface.sent_bytes,
        };
        let Some(prev) = self.last.replace(now) else {
            return NetworkUsage::Pending;
        };
        let elapsed = at.saturating_duration_since(prev.at);
        if elapsed.is_zero() || elapsed > MAX_SAMPLE_GAP {
            return NetworkUsage::Pending;
        }
        let (Some(received), Some(sent)) = (
            interface.received_bytes.checked_sub(prev.received),
            interface.sent_bytes.checked_sub(prev.sent),
        ) else {
            return NetworkUsage::Pending;
        };
        NetworkUsage::Transfer {
            download_bytes_per_sec: received as f64 / elapsed.as_secs_f64(),
            upload_bytes_per_sec: sent as f64 / elapsed.as_secs_f64(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn interface(name: &str, luid: u64, hardware: bool) -> InterfaceSnapshot {
        InterfaceSnapshot {
            luid,
            index: luid as u32,
            name: name.into(),
            description: name.into(),
            is_hardware: hardware,
            is_wifi: true,
            oper_status: IfOperStatusUp,
            received_bytes: 0,
            sent_bytes: 0,
        }
    }

    #[test]
    fn selection_requires_unique_physical_wifi_unless_named() {
        let interfaces = [
            interface("WLAN", 1, true),
            interface("Wi-Fi 2", 2, true),
            interface("Virtual", 3, false),
        ];
        assert!(select_interface(&interfaces, None).unwrap().is_none());
        assert_eq!(
            select_interface(&interfaces, Some("wlan"))
                .unwrap()
                .unwrap()
                .luid,
            1
        );
        assert!(select_interface(&interfaces, Some("virtual")).is_err());
        assert!(select_interface(&interfaces[..1], None).unwrap().is_some());
        assert!(select_interface(&[], None).unwrap().is_none());
    }

    #[test]
    fn rate_uses_elapsed_time_and_reconnection_rewarms() {
        let at = Instant::now();
        let mut interface = InterfaceSnapshot {
            luid: 1,
            index: 1,
            name: "test".into(),
            description: "test".into(),
            is_hardware: true,
            is_wifi: true,
            oper_status: IfOperStatusUp,
            received_bytes: 100,
            sent_bytes: 200,
        };
        let mut sampler = NetworkSampler::default();
        assert_eq!(sampler.update(Some(&interface), at), NetworkUsage::Pending);
        interface.received_bytes += 3072;
        interface.sent_bytes += 1536;
        assert_eq!(
            sampler.update(Some(&interface), at + Duration::from_millis(1500)),
            NetworkUsage::Transfer {
                download_bytes_per_sec: 2048.0,
                upload_bytes_per_sec: 1024.0
            }
        );
        interface.oper_status = IfOperStatusDown;
        assert_eq!(
            sampler.update(Some(&interface), at + Duration::from_secs(2)),
            NetworkUsage::Disconnected
        );
        interface.oper_status = IfOperStatusUp;
        assert_eq!(
            sampler.update(Some(&interface), at + Duration::from_secs(3)),
            NetworkUsage::Pending
        );
    }

    #[test]
    fn counter_reset_rewarms_instead_of_wrapping() {
        let at = Instant::now();
        let mut interface = interface("WLAN", 1, true);
        interface.received_bytes = 10_000;
        interface.sent_bytes = 10_000;
        let mut sampler = NetworkSampler::default();
        sampler.update(Some(&interface), at);
        // 驱动重置使计数回退时，重建差分基线。
        interface.received_bytes = 100;
        let second = at + Duration::from_secs(1);
        assert_eq!(
            sampler.update(Some(&interface), second),
            NetworkUsage::Pending
        );
        interface.received_bytes += 2048;
        interface.sent_bytes += 1024;
        assert_eq!(
            sampler.update(Some(&interface), second + Duration::from_secs(1)),
            NetworkUsage::Transfer {
                download_bytes_per_sec: 2048.0,
                upload_bytes_per_sec: 1024.0
            }
        );
    }
}
