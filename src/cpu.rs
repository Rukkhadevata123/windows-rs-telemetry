use crate::bindings::{FILETIME, GetSystemTimes};
use crate::sampling::MAX_SAMPLE_GAP;
use std::time::Instant;

/// 开机以来的累计 CPU 时间（所有逻辑处理器相加），单位 100 纳秒。
/// 注意 kernel 已经包含 idle：空闲时间在 Windows 里记作内核态时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CpuTimes {
    pub idle: u64,
    pub kernel: u64,
    pub user: u64,
}

/// FILETIME 把一个 64 位数拆成高低两个 u32，这里拼回去。
fn filetime_to_u64(ft: FILETIME) -> u64 {
    (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
}

/// 采集层：只负责调用 API，不做任何计算。
pub fn read_cpu_times() -> std::io::Result<CpuTimes> {
    let mut idle = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();

    // SAFETY：三个指针都指向本函数栈上的有效 FILETIME，只在本次调用期间使用。
    let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(CpuTimes {
        idle: filetime_to_u64(idle),
        kernel: filetime_to_u64(kernel),
        user: filetime_to_u64(user),
    })
}

/// 一次计算的结果。“还没有值”是独立状态，不用 0% 冒充。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CpuUsage {
    /// 第一次采样，或刚刚重建基线，要等下一次采样才有速率
    Pending,
    /// 两次采样之间的平均利用率，0.0–1.0
    Busy(f64),
}

/// 计算层：有状态，记住上一次的累计值，把“累计量”变成“区间内的比例”。
/// 这里不碰任何 Windows API，所以可以用手造的数据测试。
#[derive(Debug, Default)]
pub struct CpuSampler {
    last: Option<(CpuTimes, Instant)>,
}

impl CpuSampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.last = None;
    }

    pub fn update(&mut self, now: CpuTimes, at: Instant) -> CpuUsage {
        // 无论结果如何，这次读数都成为下一次的基线
        let Some((prev, prev_at)) = self.last.replace((now, at)) else {
            return CpuUsage::Pending;
        };

        let elapsed = at.saturating_duration_since(prev_at);
        if elapsed.is_zero() || elapsed > MAX_SAMPLE_GAP {
            // 暂停/恢复后的第一份读数只作基线，不把长区间平均值当成当前利用率。
            return CpuUsage::Pending;
        }

        // checked_sub：计数回退（理论上不该发生，但不信任外部数据）时得到 None，
        // 这时丢弃这个区间、以当前值重建基线，而不是算出一个巨大的假值。
        let (Some(d_idle), Some(d_kernel), Some(d_user)) = (
            now.idle.checked_sub(prev.idle),
            now.kernel.checked_sub(prev.kernel),
            now.user.checked_sub(prev.user),
        ) else {
            return CpuUsage::Pending;
        };

        // 分母是 ΔKernel + ΔUser，不要再加 ΔIdle：它已经包含在 ΔKernel 里。
        let total = d_kernel + d_user;
        if total == 0 || d_idle > total {
            // 零增量（两次读得太近）或数据自相矛盾：这个区间没法解释
            return CpuUsage::Pending;
        }

        CpuUsage::Busy(1.0 - d_idle as f64 / total as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(idle: u64, kernel: u64, user: u64) -> CpuTimes {
        CpuTimes { idle, kernel, user }
    }

    /// 从一个固定基线出发，喂入增量，返回第二次的结果
    fn usage_after(d_idle: u64, d_kernel: u64, d_user: u64) -> CpuUsage {
        let mut s = CpuSampler::new();
        let at = Instant::now();
        s.update(t(1000, 5000, 3000), at);
        s.update(
            t(1000 + d_idle, 5000 + d_kernel, 3000 + d_user),
            at + Duration::from_secs(1),
        )
    }

    #[test]
    fn first_sample_is_pending() {
        assert_eq!(
            CpuSampler::new().update(t(1, 2, 3), Instant::now()),
            CpuUsage::Pending
        );
    }

    #[test]
    fn fully_idle() {
        // 100 单位全是空闲：kernel 增加 100，其中 idle 也是 100
        assert_eq!(usage_after(100, 100, 0), CpuUsage::Busy(0.0));
    }

    #[test]
    fn fully_busy() {
        assert_eq!(usage_after(0, 60, 40), CpuUsage::Busy(1.0));
    }

    #[test]
    fn half_busy() {
        // 总共 100：内核 80（其中空闲 50）+ 用户 20 → 忙 50
        assert_eq!(usage_after(50, 80, 20), CpuUsage::Busy(0.5));
    }

    #[test]
    fn zero_delta_is_pending() {
        assert_eq!(usage_after(0, 0, 0), CpuUsage::Pending);
    }

    #[test]
    fn counter_going_backwards_rebaselines() {
        let mut s = CpuSampler::new();
        let at = Instant::now();
        s.update(t(1000, 5000, 3000), at);
        // 回退：丢弃这个区间
        assert_eq!(
            s.update(t(900, 5000, 3000), at + Duration::from_secs(1)),
            CpuUsage::Pending
        );
        // 以回退后的值为新基线，下一次正常计算，没有尖峰
        assert_eq!(
            s.update(t(950, 5100, 3000), at + Duration::from_secs(2)),
            CpuUsage::Busy(0.5)
        );
    }

    #[test]
    fn long_gap_and_read_failure_require_a_fresh_baseline() {
        let at = Instant::now();
        let mut s = CpuSampler::new();
        s.update(t(0, 0, 0), at);
        assert_eq!(
            s.update(t(100, 200, 100), at + Duration::from_secs(30)),
            CpuUsage::Pending
        );
        assert_eq!(
            s.update(t(150, 280, 120), at + Duration::from_secs(31)),
            CpuUsage::Busy(0.5)
        );
        s.reset(); // API 读取失败也丢弃基线。
        assert_eq!(
            s.update(t(200, 360, 140), at + Duration::from_secs(32)),
            CpuUsage::Pending
        );
    }
}
