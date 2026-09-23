//! 普通权限的电池快照。功率直接由系统报告，不对容量做差分。
use std::{mem::size_of, ptr::null, time::Duration};

use crate::bindings as n;

pub const INTERVAL: Duration = Duration::from_secs(3);
pub const STALE_AFTER: Duration = Duration::from_secs(7);

// Windows BOOLEAN 是字节；生成的 bool 字段不能直接承接系统写入的任意字节。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BatteryStateRaw {
    ac_online: u8,
    battery_present: u8,
    charging: u8,
    discharging: u8,
    spare: [u8; 3],
    tag: u8,
    max_capacity: u32,
    remaining_capacity: u32,
    rate: u32,
    estimated_time: u32,
    default_alert1: u32,
    default_alert2: u32,
}

const _: () = {
    assert!(size_of::<BatteryStateRaw>() == size_of::<n::SYSTEM_BATTERY_STATE>());
    assert!(
        std::mem::offset_of!(BatteryStateRaw, rate)
            == std::mem::offset_of!(n::SYSTEM_BATTERY_STATE, Rate)
    );
};

#[derive(Debug)]
pub struct PowerStatus {
    pub ac_online: Option<bool>,
    pub present: Option<bool>,
    pub charging: Option<bool>,
    pub percent: Option<u8>,
    pub saver: Option<bool>,
    pub remaining_seconds: Option<u32>,
}

#[derive(Debug)]
pub struct BatteryFlow {
    pub present: bool,
    pub charging: bool,
    pub discharging: bool,
    /// 正为充电，负为放电；零/未报告和未知哨兵值保留为 None。
    pub milliwatts: Option<i32>,
}

#[derive(Debug)]
pub struct BatterySnapshot {
    // 两个 API 独立报告失败，功率失败不抹掉基础电量。
    pub power: Result<PowerStatus, String>,
    pub flow: Result<BatteryFlow, String>,
}

fn flag(value: u8) -> Option<bool> {
    match value {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn decode_power(raw: n::SYSTEM_POWER_STATUS) -> PowerStatus {
    // 必须先处理 255（未知），再判断位；否则会把未知当成“无电池/充电”。
    let present = (raw.BatteryFlag != 255).then_some(raw.BatteryFlag & 128 == 0);
    let ac_online = flag(raw.ACLineStatus);
    PowerStatus {
        ac_online,
        present,
        charging: (present == Some(true)).then_some(raw.BatteryFlag & 8 != 0),
        percent: (present != Some(false) && raw.BatteryLifePercent <= 100)
            .then_some(raw.BatteryLifePercent),
        saver: flag(raw.SystemStatusFlag),
        remaining_seconds: (present != Some(false)
            && ac_online == Some(false)
            && raw.BatteryLifeTime != u32::MAX)
            .then_some(raw.BatteryLifeTime),
    }
}

fn decode_flow(raw: BatteryStateRaw) -> BatteryFlow {
    // Windows 声明为 DWORD，但文档要求按 LONG 解读；as i32 保留位模式。
    let rate = raw.rate as i32;
    let present = raw.battery_present != 0;
    let charging = raw.charging != 0;
    let discharging = raw.discharging != 0;
    let direction_matches =
        (charging && !discharging && rate > 0) || (discharging && !charging && rate < 0);
    BatteryFlow {
        present,
        charging,
        discharging,
        milliwatts: (present && rate != i32::MIN && direction_matches).then_some(rate),
    }
}

pub fn collect_battery() -> BatterySnapshot {
    let mut power = n::SYSTEM_POWER_STATUS::default();
    // SAFETY：传入与 API 类型匹配的可写本地结构体，只在成功后解释输出。
    let power = if unsafe { n::GetSystemPowerStatus(&mut power) } != 0 {
        Ok(decode_power(power))
    } else {
        Err(std::io::Error::last_os_error().to_string())
    };
    let mut flow = BatteryStateRaw::default();
    // SAFETY：SystemBatteryState 是只读查询，输入为空；输出指针和大小匹配。
    let status = unsafe {
        n::CallNtPowerInformation(
            n::SystemBatteryState,
            null(),
            0,
            (&mut flow as *mut BatteryStateRaw).cast(),
            size_of::<BatteryStateRaw>() as u32,
        )
    };
    // 此 API 返回 NTSTATUS；不能用 GetLastError 解释它。
    let flow = if status == 0 {
        Ok(decode_flow(flow))
    } else {
        Err(format!("NTSTATUS 0x{:08X}", status as u32))
    };
    BatterySnapshot { power, flow }
}

impl BatterySnapshot {
    /// 保留插电未充电、没有电池和未知三种区别。
    pub fn labels(&self) -> (String, String) {
        let basic = match &self.power {
            Err(e) => format!("电源状态读取失败：{e}"),
            Ok(p) => {
                let source = match p.ac_online {
                    Some(true) => "外接电源",
                    Some(false) => "电池供电",
                    None => "供电未知",
                };
                let level = if p.present == Some(false) {
                    "无系统电池".into()
                } else {
                    p.percent
                        .map_or_else(|| "电量未知".into(), |v| format!("{v}%"))
                };
                let charging = match p.charging {
                    Some(true) => "充电中",
                    Some(false) => "未充电",
                    None => "充电状态未知",
                };
                let saver = match p.saver {
                    Some(true) => "省电开启",
                    Some(false) => "省电关闭",
                    None => "省电状态未知",
                };
                format!("{level}  ·  {source}  ·  {charging}  ·  {saver}")
            }
        };
        let flow = match &self.flow {
            Err(e) => format!("功率读取失败：{e}"),
            Ok(f) if !f.present => "无系统电池".into(),
            Ok(f) => {
                let state = match (f.charging, f.discharging) {
                    (true, false) => "充电",
                    (false, true) => "放电",
                    (false, false) => "未充放电",
                    (true, true) => "状态不一致",
                };
                f.milliwatts.map_or_else(
                    || format!("{state} · 功率未报告"),
                    |v| format!("{state} {:+.2} W", f64::from(v) / 1000.0),
                )
            }
        };
        let remaining = self
            .power
            .as_ref()
            .ok()
            .and_then(|p| p.remaining_seconds)
            .map_or_else(
                || "剩余续航 —".into(),
                |s| format!("预计续航 {}h {:02}m", s / 3600, s % 3600 / 60),
            );
        (basic, format!("电池侧：{flow}  ·  {remaining}"))
    }
}

#[cfg(test)]
pub(crate) fn demo_snapshot() -> BatterySnapshot {
    BatterySnapshot {
        power: Ok(PowerStatus {
            ac_online: Some(false),
            present: Some(true),
            charging: Some(false),
            percent: Some(76),
            saver: Some(false),
            remaining_seconds: Some(18000),
        }),
        flow: Ok(BatteryFlow {
            present: true,
            charging: false,
            discharging: true,
            milliwatts: Some(-12340),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_flags_are_not_no_battery_or_charging() {
        let p = decode_power(n::SYSTEM_POWER_STATUS {
            ACLineStatus: 255,
            BatteryFlag: 255,
            BatteryLifePercent: 255,
            BatteryLifeTime: u32::MAX,
            ..Default::default()
        });
        assert_eq!(
            (p.present, p.charging, p.percent, p.remaining_seconds),
            (None, None, None, None)
        );
        let p = decode_power(n::SYSTEM_POWER_STATUS {
            ACLineStatus: 1,
            BatteryFlag: 9,
            BatteryLifePercent: 94,
            BatteryLifeTime: 100,
            ..Default::default()
        });
        assert_eq!(p.charging, Some(true));
        assert_eq!(p.remaining_seconds, None); // 接电也不把秒数当成充满倒计时。
    }

    #[test]
    fn signed_power_and_unknown_rate_do_not_become_giant_wattages() {
        let mut raw = BatteryStateRaw {
            battery_present: 1,
            discharging: 1,
            rate: (-14350_i32) as u32,
            ..Default::default()
        };
        assert_eq!(decode_flow(raw).milliwatts, Some(-14350));
        raw.rate = 0x80000000;
        assert_eq!(decode_flow(raw).milliwatts, None);
        raw.rate = 23000;
        raw.discharging = 0;
        raw.charging = 1;
        assert_eq!(decode_flow(raw).milliwatts, Some(23000));
        raw.battery_present = 0;
        assert_eq!(decode_flow(raw).milliwatts, None);
    }

    #[test]
    fn non_boolean_spare_bytes_and_boolean_values_are_safe() {
        let raw = BatteryStateRaw {
            battery_present: 255,
            discharging: 128,
            spare: [255, 128, 2],
            rate: (-12000_i32) as u32,
            ..Default::default()
        };
        let flow = decode_flow(raw);
        assert!(flow.present && flow.discharging);
        assert_eq!(flow.milliwatts, Some(-12000));
    }
}
