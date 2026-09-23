use std::{
    io,
    sync::{
        Arc, Mutex,
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    battery::{self, BatterySnapshot},
    cpu::{self, CpuSampler, CpuUsage},
    memory::{self, MemorySnapshot},
    network::{self, NetworkSampler, NetworkUsage},
    nvme::{self, DiskDevice, SmartHealth},
    pdh,
};

pub const INTERVAL: Duration = Duration::from_secs(1);
/// 约 2.5 个采样周期；超过就视为中断，CPU/网络重建基线，曲线也断开。
pub const MAX_SAMPLE_GAP: Duration = Duration::from_millis(2500);

pub fn is_stale(at: Option<Instant>, now: Instant, max_age: Duration) -> bool {
    at.is_some_and(|at| now.saturating_duration_since(at) > max_age)
}

/// 慢速数据源从未读取过，或距上次读取已满一个周期。
fn is_due(last: Option<Instant>, now: Instant, interval: Duration) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= interval)
}

/// 一次采样的全部结果。每项各自可能失败，互不影响。
#[derive(Debug)]
pub struct Sample {
    pub taken_at: Instant,
    pub cpu: io::Result<CpuUsage>,
    /// 与利用率独立的 PDH 估算频率，单位 MHz；None 表示等待基线或没有有效值。
    pub cpu_mhz: Result<Option<f64>, String>,
    pub memory: io::Result<MemorySnapshot>,
    /// 网络独立记录查询完成的时刻，差分和横轴使用同一个时间来源。
    pub network_at: Instant,
    pub network: io::Result<NetworkUsage>,
    /// 慢速电池快照随每份样本携带，信箱合并时不会丢失；时间戳只在重读时更新。
    pub battery: Option<(Instant, Arc<BatterySnapshot>)>,
    /// NVMe 低频快照也随每份样本携带，UI 忙碌合并通知时不会丢更新。
    pub nvme: Option<(Instant, Arc<Result<SmartHealth, String>>)>,
    /// PDH 同一轮结果：磁盘与 GPU 各自可失败；首轮为 None，等待基线。
    pub pdh: Option<(Instant, pdh::Snapshot)>,
}

/// 单槽信箱：消费者落后时保留最新样本。
pub struct Latest<T>(Mutex<Option<T>>);

impl<T> Default for Latest<T> {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl<T> Latest<T> {
    /// 返回信箱先前是否为空，供调用者决定是否发送唤醒通知。
    pub fn publish(&self, value: T) -> bool {
        self.0.lock().unwrap().replace(value).is_none()
    }

    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap().take()
    }
}

/// 后台采样线程的句柄。stop() 或离开作用域时，通知线程退出并等它结束。
pub struct SamplerThread {
    /// 丢弃发送端即停止信号：工作线程的 recv_timeout 立刻返回 Disconnected。
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SamplerThread {
    /// on_sample 在采样线程上调用。
    pub fn spawn(
        interval: Duration,
        network_luid: Option<u64>,
        disk: Option<DiskDevice>,
        mut on_sample: impl FnMut(Sample) + Send + 'static,
    ) -> io::Result<Self> {
        if interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "采样间隔不能为零",
            ));
        }
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let handle = thread::Builder::new()
            .name("sampler".into())
            .spawn(move || {
                // 所有采样器和 PDH 查询都只在这个线程创建和使用。
                let mut cpu_sampler = CpuSampler::new();
                let mut cpu_frequency_sampler = pdh::CpuFrequencySampler::new();
                let mut cpu_frequency_retry_at = Instant::now() + pdh::RETRY_INTERVAL;
                let mut network_sampler = NetworkSampler::default();
                let mut battery: Option<(Instant, Arc<BatterySnapshot>)> = None;
                let mut nvme: Option<(Instant, Arc<Result<SmartHealth, String>>)> = None;
                let mut pdh_sampler = pdh::Sampler::new();
                let mut next_tick = Instant::now();
                loop {
                    let taken_at = Instant::now();
                    let cpu = match cpu::read_cpu_times() {
                        Ok(times) => Ok(cpu_sampler.update(times, taken_at)),
                        Err(error) => {
                            cpu_sampler.reset();
                            Err(error)
                        }
                    };
                    let cpu_mhz = cpu_frequency_sampler
                        .as_mut()
                        .map_err(|error| error.clone())
                        .and_then(pdh::CpuFrequencySampler::poll);
                    if cpu_mhz.is_err() && taken_at >= cpu_frequency_retry_at {
                        cpu_frequency_sampler = pdh::CpuFrequencySampler::new();
                        cpu_frequency_retry_at = taken_at + pdh::RETRY_INTERVAL;
                    }
                    let memory = memory::collect_memory();
                    let interfaces = network_luid.map(|_| network::collect_interfaces());
                    let network_at = Instant::now();
                    let network = match interfaces {
                        None => Ok(NetworkUsage::NotSelected),
                        Some(Ok(interfaces)) => Ok(network_sampler.update(
                            interfaces.iter().find(|i| Some(i.luid) == network_luid),
                            network_at,
                        )),
                        Some(Err(error)) => {
                            network_sampler.reset();
                            Err(error)
                        }
                    };
                    let slow_now = Instant::now();
                    if is_due(
                        battery.as_ref().map(|(at, _)| *at),
                        slow_now,
                        battery::INTERVAL,
                    ) {
                        let snapshot = battery::collect_battery();
                        battery = Some((Instant::now(), Arc::new(snapshot)));
                    }
                    if let Some(device) = disk.as_ref()
                        && is_due(nvme.as_ref().map(|(at, _)| *at), slow_now, nvme::INTERVAL)
                    {
                        let reading = nvme::collect_health(device).map_err(|e| e.to_string());
                        nvme = Some((Instant::now(), Arc::new(reading)));
                    }
                    let pdh = pdh_sampler
                        .poll()
                        .map(|snapshot| (Instant::now(), snapshot));
                    on_sample(Sample {
                        taken_at,
                        cpu,
                        cpu_mhz,
                        memory,
                        network_at,
                        network,
                        battery: battery.clone(),
                        nvme: nvme.clone(),
                        pdh,
                    });

                    next_tick += interval;
                    let delay = next_tick.saturating_duration_since(Instant::now());
                    if delay.is_zero() {
                        next_tick = Instant::now();
                    }
                    match stop_rx.recv_timeout(delay) {
                        Err(RecvTimeoutError::Timeout) => continue,
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;

        Ok(Self {
            stop_tx: Some(stop_tx),
            handle: Some(handle),
        })
    }

    /// 等价于 drop，只是让调用点意图更清楚。
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for SamplerThread {
    fn drop(&mut self) {
        // 先发停止信号再 join。
        drop(self.stop_tx.take());
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            eprintln!("采样线程发生了 panic");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Latest;

    #[test]
    fn latest_mailbox_replaces_unconsumed_value() {
        let latest = Latest::default();
        assert!(latest.publish(1));
        assert!(!latest.publish(2));
        assert_eq!(latest.take(), Some(2));
        assert_eq!(latest.take(), None);
        assert!(latest.publish(3));
    }
}
