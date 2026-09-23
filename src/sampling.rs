use std::{
    io,
    sync::{
        Arc,
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

/// 当前每秒采样：超过 2.5 秒就视为中断，CPU/网络重建基线，曲线也断开。
pub const MAX_SAMPLE_GAP: Duration = Duration::from_millis(2500);

/// 一次采样的全部结果。每项各自可能失败，互不影响。
#[derive(Debug)]
pub struct Sample {
    /// 单调时钟：不受系统时间调整影响，只用来算间隔
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

/// 后台采样线程的句柄。stop() 或离开作用域时，通知线程退出并等它结束。
pub struct SamplerThread {
    /// 停止信号就是“丢弃发送端”：工作线程的 recv_timeout 会立刻返回 Disconnected。
    /// 通道里从来不真正发送数据。
    stop_tx: Option<mpsc::Sender<()>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl SamplerThread {
    /// on_sample 在采样线程上被调用，所以要求 Send + 'static：
    /// 它会被移动到另一个线程，并且可能比当前函数活得更久。
    pub fn spawn(
        interval: Duration,
        network_luid: Option<u64>,
        disk: Option<DiskDevice>,
        mut on_sample: impl FnMut(Sample) + Send + 'static,
    ) -> io::Result<Self> {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();

        let handle = thread::Builder::new()
            .name("sampler".into()) // 调试器和 panic 信息里能看到这个名字
            .spawn(move || {
                // CpuSampler 归这个线程独有，不需要锁
                let mut cpu_sampler = CpuSampler::new();
                let mut cpu_frequency_sampler = pdh::CpuFrequencySampler::new();
                let mut network_sampler = NetworkSampler::default();
                let mut battery: Option<(Instant, Arc<BatterySnapshot>)> = None;
                let mut nvme: Option<(Instant, Arc<Result<SmartHealth, String>>)> = None;
                let mut pdh_sampler = pdh::Sampler::new();
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
                    let battery_now = Instant::now();
                    if battery
                        .as_ref()
                        .is_none_or(|(at, _)| battery_now.duration_since(*at) >= battery::INTERVAL)
                    {
                        let snapshot = battery::collect_battery();
                        let at = Instant::now();
                        battery = Some((at, Arc::new(snapshot)));
                    }
                    if let Some(device) = disk.as_ref()
                        && nvme
                            .as_ref()
                            .is_none_or(|(at, _)| at.elapsed() >= nvme::INTERVAL)
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

                    // 用“等停止信号，最多等 interval”代替 sleep：
                    // 停止时不必等满一秒，立即醒来。
                    match stop_rx.recv_timeout(interval) {
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

    /// 显式停止。真正的工作在 Drop 里，这里只是让意图更清楚。
    pub fn stop(self) {
        drop(self);
    }
}

impl Drop for SamplerThread {
    fn drop(&mut self) {
        // 顺序很重要：先发信号，再 join。反过来会永远等下去。
        drop(self.stop_tx.take());
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            eprintln!("采样线程发生了 panic");
        }
    }
}
