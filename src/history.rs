//! 保存时间戳、曲线数据、结构化最新读数和慢速指标的显示标签，不依赖窗口/Canvas。
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use crate::{
    cpu::CpuUsage,
    format,
    network::NetworkUsage,
    sampling::{MAX_SAMPLE_GAP, Sample},
    sources::Sources,
};

pub const HISTORY_SPAN: Duration = Duration::from_secs(60);
const MAX_POINTS: usize = 256;
pub const PENDING_LABEL: &str = "等待采样";
pub const STALE_LABEL: &str = "数据已过期";

/// 过期时统一替换成 STALE_LABEL，不再展示旧值。
pub fn unless_stale(label: &str, stale: bool) -> &str {
    if stale { STALE_LABEL } else { label }
}

/// 最近一次读数。校验只在 push 时做一次，前端只负责格式化。
#[derive(Clone, Debug, PartialEq)]
pub enum Metric<T> {
    Pending,
    Value(T),
    Failed(String),
}

impl<T> Metric<T> {
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }

    pub fn label(&self, format: impl FnOnce(&T) -> String) -> String {
        match self {
            Self::Pending => PENDING_LABEL.into(),
            Self::Value(value) => format(value),
            Self::Failed(error) => format!("读取失败：{error}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemoryUse {
    /// 0..1
    pub ratio: f64,
    pub used_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Copy)]
pub struct Reading {
    pub at: Instant,
    pub cpu: Option<f64>,
    pub ram: Option<f64>,
    pub network_at: Instant,
    /// 网络保留原始 B/s，绘制时才按纵轴上限换算成比例。
    pub download: Option<f64>,
    pub upload: Option<f64>,
}

pub struct History {
    pub points: VecDeque<Reading>,
    /// 0..1 的总利用率。
    pub cpu: Metric<f64>,
    pub cpu_mhz: Metric<f64>,
    pub memory: Metric<MemoryUse>,
    pub network_name: String,
    pub network_status: String,
    pub battery_at: Option<Instant>,
    pub battery_label: String,
    pub battery_detail: String,
    pub nvme_at: Option<Instant>,
    pub nvme_name: String,
    pub nvme_label: String,
    pub nvme_detail: String,
    pub pdh_at: Option<Instant>,
    pub disk_label: String,
    pub gpu_label: String,
}

impl Default for History {
    fn default() -> Self {
        Self {
            points: VecDeque::new(),
            cpu: Metric::Pending,
            cpu_mhz: Metric::Pending,
            memory: Metric::Pending,
            network_name: "未选择 WLAN".into(),
            network_status: PENDING_LABEL.into(),
            battery_at: None,
            battery_label: "等待电池采样".into(),
            battery_detail: String::new(),
            nvme_at: None,
            nvme_name: "未选择 NVMe".into(),
            nvme_label: "可用 --list-disks 查看设备".into(),
            nvme_detail: String::new(),
            pdh_at: None,
            disk_label: "等待基线".into(),
            gpu_label: "等待基线".into(),
        }
    }
}

fn cpu_metric(sample: &Sample) -> Metric<f64> {
    match &sample.cpu {
        Ok(CpuUsage::Busy(ratio)) if ratio.is_finite() && (0.0..=1.0).contains(ratio) => {
            Metric::Value(*ratio)
        }
        Ok(CpuUsage::Pending) => Metric::Pending,
        Ok(CpuUsage::Busy(_)) => Metric::Failed("无效读数".into()),
        Err(error) => Metric::Failed(error.to_string()),
    }
}

fn frequency_metric(sample: &Sample) -> Metric<f64> {
    match &sample.cpu_mhz {
        Ok(Some(mhz)) if mhz.is_finite() && *mhz > 0.0 => Metric::Value(*mhz),
        Ok(Some(_)) => Metric::Failed("无效频率".into()),
        Ok(None) => Metric::Pending,
        Err(error) => Metric::Failed(error.clone()),
    }
}

fn memory_metric(sample: &Sample) -> Metric<MemoryUse> {
    match &sample.memory {
        Ok(m) if m.total_bytes > 0 && m.available_bytes <= m.total_bytes => {
            Metric::Value(MemoryUse {
                ratio: m.used_bytes() as f64 / m.total_bytes as f64,
                used_bytes: m.used_bytes(),
                total_bytes: m.total_bytes,
            })
        }
        Ok(_) => Metric::Failed("无效容量".into()),
        Err(error) => Metric::Failed(error.to_string()),
    }
}

impl History {
    /// 设备名的占位文案只在这里决定。
    pub fn for_sources(sources: &Sources) -> Self {
        let mut history = Self::default();
        if let Some(name) = &sources.network_name {
            history.network_name.clone_from(name);
        }
        if let Some(disk) = &sources.disk {
            history.nvme_name.clone_from(&disk.name);
        }
        history
    }

    pub fn last_sample_at(&self) -> Option<Instant> {
        self.points.back().map(|point| point.at)
    }

    pub fn cpu_label(&self) -> String {
        let frequency = match &self.cpu_mhz {
            Metric::Value(mhz) => format!("估算频率 {}", format::ghz(*mhz)),
            Metric::Pending => "频率等待基线".into(),
            Metric::Failed(error) => format!("频率读取失败：{error}"),
        };
        format!(
            "{}  ·  {frequency}",
            self.cpu.label(|ratio| format::percent(*ratio))
        )
    }

    pub fn ram_label(&self) -> String {
        self.memory.label(|m| {
            format!(
                "{}  ·  {}",
                format::percent(m.ratio),
                format::gib_pair(m.used_bytes, m.total_bytes)
            )
        })
    }

    pub fn push(&mut self, sample: Sample) {
        if self.points.back().is_some_and(|p| p.at >= sample.taken_at) {
            return; // 不把乱序或重复样本插进时间线。
        }
        self.cpu = cpu_metric(&sample);
        self.cpu_mhz = frequency_metric(&sample);
        self.memory = memory_metric(&sample);
        let (status, download, upload) = match sample.network {
            Ok(NetworkUsage::Transfer {
                download_bytes_per_sec,
                upload_bytes_per_sec,
            }) => (
                String::new(),
                Some(download_bytes_per_sec),
                Some(upload_bytes_per_sec),
            ),
            Ok(NetworkUsage::NotSelected) => ("未选择接口".into(), None, None),
            Ok(NetworkUsage::Pending) => ("等待下一次采样".into(), None, None),
            Ok(NetworkUsage::Disconnected) => ("接口未连接".into(), None, None),
            Ok(NetworkUsage::Unavailable) => ("接口不在场".into(), None, None),
            Err(error) => (format!("读取失败：{error}"), None, None),
        };
        self.network_status = status;
        if let Some((at, battery)) = sample.battery
            && self.battery_at.is_none_or(|previous| at > previous)
        {
            self.battery_at = Some(at);
            (self.battery_label, self.battery_detail) = battery.labels();
        }
        if let Some((at, reading)) = sample.nvme
            && self.nvme_at.is_none_or(|previous| at > previous)
        {
            self.nvme_at = Some(at);
            match reading.as_ref() {
                Ok(health) => (self.nvme_label, self.nvme_detail) = health.labels(),
                Err(error) => {
                    self.nvme_label = format!("读取失败：{error}");
                    self.nvme_detail.clear();
                }
            }
        }
        if let Some((at, snapshot)) = sample.pdh {
            self.pdh_at = Some(at);
            self.disk_label = match snapshot.disk {
                Ok(rates) => format!(
                    "读 {}  ·  写 {}",
                    format::rate(rates.read_bps),
                    format::rate(rates.write_bps)
                ),
                Err(error) => format!("读取失败：{error}"),
            };
            self.gpu_label = match snapshot.gpu {
                Ok(Some(engine)) => format!(
                    "{}  ·  {} / PID {}  {:.1}%",
                    engine.adapter, engine.engine_type, engine.pid, engine.percent
                ),
                Ok(None) => "没有活动引擎实例".into(),
                Err(error) => format!("读取失败：{error}"),
            };
        }
        self.points.push_back(Reading {
            at: sample.taken_at,
            cpu: self.cpu.value().copied(),
            ram: self.memory.value().map(|m| m.ratio),
            network_at: sample.network_at,
            download,
            upload,
        });
        self.prune(sample.taken_at);
    }

    pub fn prune(&mut self, now: Instant) {
        while self
            .points
            .front()
            .is_some_and(|p| now.saturating_duration_since(p.at) > HISTORY_SPAN)
            || self.points.len() > MAX_POINTS
        {
            self.points.pop_front();
        }
    }

    /// x 在 0..1；y 保留原始值。None 同时表达缺失值和线段边界。
    pub fn series(
        &self,
        now: Instant,
        metric: fn(&Reading) -> Option<f64>,
    ) -> Vec<Option<(f32, f32)>> {
        self.series_at(now, metric, |p| p.at)
    }

    pub fn network_series(
        &self,
        now: Instant,
        metric: fn(&Reading) -> Option<f64>,
    ) -> Vec<Option<(f32, f32)>> {
        self.series_at(now, metric, |p| p.network_at)
    }

    fn series_at(
        &self,
        now: Instant,
        metric: fn(&Reading) -> Option<f64>,
        time: fn(&Reading) -> Instant,
    ) -> Vec<Option<(f32, f32)>> {
        let mut result = Vec::new();
        let mut previous = None;
        for p in &self.points {
            let at = time(p);
            let age = now.saturating_duration_since(at);
            if age > HISTORY_SPAN {
                continue;
            }
            if previous.is_some_and(|prev| at.saturating_duration_since(prev) > MAX_SAMPLE_GAP) {
                result.push(None);
            }
            result.push(metric(p).map(|v| {
                (
                    1.0 - age.as_secs_f32() / HISTORY_SPAN.as_secs_f32(),
                    v as f32,
                )
            }));
            previous = Some(at);
        }
        result
    }
}

/// 测试样本包含周期性缺失的 CPU 和网络数据，用来检查曲线断线。
#[cfg(test)]
fn demo_sample(at: Instant, second: u64) -> Sample {
    let phase = second as f64 / 7.0;
    let cpu = if (second % 30) >= 12 && (second % 30) <= 14 {
        CpuUsage::Pending
    } else {
        CpuUsage::Busy(0.4 + 0.3 * phase.sin())
    };
    let total_bytes = 32_u64 << 30;
    Sample {
        taken_at: at,
        cpu: Ok(cpu),
        cpu_mhz: Ok(Some(2800.0 + 350.0 * phase.sin())),
        memory: Ok(crate::memory::MemorySnapshot {
            total_bytes,
            available_bytes: (total_bytes as f64 * (0.48 + 0.07 * (phase / 2.0).cos())) as u64,
        }),
        network_at: at,
        battery: Some((at, std::sync::Arc::new(crate::battery::demo_snapshot()))),
        nvme: Some((at, std::sync::Arc::new(Ok(crate::nvme::demo_health())))),
        pdh: Some((
            at,
            crate::pdh::Snapshot {
                disk: Ok(crate::pdh::DiskRates {
                    read_bps: (1.2 + phase.sin()) * 1024.0 * 1024.0,
                    write_bps: 250.0 * 1024.0,
                }),
                gpu: Ok(Some(crate::pdh::EngineReading {
                    adapter: "演示 GPU".into(),
                    pid: 1234,
                    engine_type: "3D".into(),
                    percent: 50.0 + 30.0 * phase.sin(),
                })),
            },
        )),
        network: Ok(if (second % 30) >= 20 && (second % 30) <= 22 {
            NetworkUsage::Disconnected
        } else {
            NetworkUsage::Transfer {
                download_bytes_per_sec: (1.5 + phase.sin()) * 1024.0 * 1024.0,
                upload_bytes_per_sec: (0.3 + 0.2 * (phase * 1.7).cos()) * 1024.0 * 1024.0,
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_keeps_bytes_per_second_its_own_timestamp_and_error_gaps() {
        let start = Instant::now();
        let mut h = History::default();
        let mut sample = demo_sample(start, 0);
        sample.network_at = start + Duration::from_millis(500);
        sample.network = Ok(NetworkUsage::Transfer {
            download_bytes_per_sec: 2048.0,
            upload_bytes_per_sec: 1024.0,
        });
        h.push(sample);
        let mut failed = demo_sample(start + Duration::from_secs(1), 1);
        failed.network = Err(std::io::Error::other("network probe failed"));
        h.push(failed);
        let series = h.network_series(start + Duration::from_secs(1), |p| p.download);
        let (x, rate) = series[0].unwrap();
        assert!((x - (1.0 - 0.5 / 60.0)).abs() < 0.0001);
        assert_eq!(rate, 2048.0);
        assert!(series[1].is_none());
        assert!(h.points[1].cpu.is_some() && h.points[1].ram.is_some());
        assert!(h.network_status.contains("network probe failed"));
    }

    #[test]
    fn timestamps_set_spacing_and_missing_values_break_only_their_metric() {
        let start = Instant::now();
        let mut h = History::default();
        h.push(demo_sample(start, 0));
        h.push(demo_sample(start + Duration::from_secs(1), 12));
        h.push(demo_sample(start + Duration::from_secs(5), 5));
        let now = start + Duration::from_secs(5);
        let cpu = h.series(now, |p| p.cpu);
        assert!((cpu[0].unwrap().0 - 55.0 / 60.0).abs() < 0.0001);
        assert_eq!(cpu[1], None); // 缺失 CPU 不伪造为零。
        assert_eq!(cpu[2], None); // 4 秒的间隔也不能连线。
        assert_eq!(cpu[3].unwrap().0, 1.0);
        assert!(h.series(now, |p| p.ram)[1].is_some());
    }

    #[test]
    fn history_expires_and_is_bounded_even_at_high_frequency() {
        let start = Instant::now();
        let mut h = History::default();
        for i in 0..1000 {
            h.push(demo_sample(start + Duration::from_millis(i), i));
        }
        assert_eq!(h.points.len(), MAX_POINTS);
        h.prune(start + Duration::from_secs(62));
        assert!(h.points.is_empty());
    }

    #[test]
    fn errors_are_gaps_and_old_samples_cannot_replace_current_labels() {
        let now = Instant::now();
        let mut h = History::default();
        let mut sample = demo_sample(now, 0);
        sample.cpu = Err(std::io::Error::other("probe failed"));
        h.push(sample);
        assert!(h.points[0].cpu.is_none());
        assert!(h.points[0].ram.is_some());
        h.push(demo_sample(now - Duration::from_secs(1), 1));
        assert!(h.cpu_label().contains("probe failed"));
        assert_eq!(h.points.len(), 1);
        let mut sample = demo_sample(now + Duration::from_secs(1), 2);
        sample.cpu = Ok(CpuUsage::Busy(1.5));
        h.push(sample);
        assert!(matches!(h.cpu, Metric::Failed(_)));
        assert!(h.points[1].cpu.is_none());
    }
}
