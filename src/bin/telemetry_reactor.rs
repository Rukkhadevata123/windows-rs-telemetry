//! 使用 WinUI 控件展示遥测快照。
use std::{
    error::Error,
    io,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use windows_reactor::*;
use windows_rs_telemetry::{
    battery,
    config::{self, Command},
    cpu::CpuUsage,
    history::History,
    network::{self, format_rate},
    nvme,
    sampling::{INTERVAL, Latest, MAX_SAMPLE_GAP, Sample, SamplerThread, is_stale},
    sources::{self, Sources},
};

const CANCEL_CHECK: Duration = Duration::from_millis(100);
const BACKGROUND: Color = Color::rgb(10, 18, 32);
const CARD: Color = Color::rgb(22, 36, 56);
const HERO: Color = Color::rgb(19, 49, 70);
const STROKE: Color = Color::rgb(47, 67, 88);
const TEXT: Color = Color::rgb(241, 247, 253);
const MUTED: Color = Color::rgb(167, 188, 207);
const CYAN: Color = Color::rgb(105, 222, 238);
const GREEN: Color = Color::rgb(151, 229, 179);
const VIOLET: Color = Color::rgb(203, 186, 255);
const AMBER: Color = Color::rgb(255, 201, 133);

enum Message {
    Sample(Box<Sample>),
    Tick,
    SamplerStopped,
    DeliveryRejected,
}

enum Metric {
    Pending,
    Value(String),
    Failed(String),
}

impl Metric {
    fn label(&self, stale: bool) -> String {
        if stale {
            return "数据已过期".into();
        }
        match self {
            Self::Pending => "预热中".into(),
            Self::Value(value) => value.clone(),
            Self::Failed(error) => format!("读取失败：{error}"),
        }
    }
}

struct Gauges {
    cpu: Metric,
    frequency: Metric,
    memory: Metric,
    cpu_percent: Option<f64>,
    memory_percent: Option<f64>,
    memory_detail: String,
}

impl Gauges {
    fn from_sample(sample: &Sample) -> Self {
        let (cpu, cpu_percent) = match &sample.cpu {
            Ok(CpuUsage::Busy(ratio)) if ratio.is_finite() && (0.0..=1.0).contains(ratio) => (
                Metric::Value(format!("{:.1}%", ratio * 100.0)),
                Some(ratio * 100.0),
            ),
            Ok(CpuUsage::Pending) => (Metric::Pending, None),
            Ok(_) => (Metric::Failed("无效利用率".into()), None),
            Err(error) => (Metric::Failed(error.to_string()), None),
        };
        let frequency = match &sample.cpu_mhz {
            Ok(Some(mhz)) if mhz.is_finite() && *mhz > 0.0 => {
                Metric::Value(format!("{:.2} GHz", mhz / 1000.0))
            }
            Ok(Some(_)) => Metric::Failed("无效频率".into()),
            Ok(None) => Metric::Pending,
            Err(error) => Metric::Failed(error.clone()),
        };
        let (memory, memory_percent, memory_detail) = match &sample.memory {
            Ok(snapshot)
                if snapshot.total_bytes > 0 && snapshot.available_bytes <= snapshot.total_bytes =>
            {
                let gib = (1_u64 << 30) as f64;
                let percent = snapshot.used_bytes() as f64 / snapshot.total_bytes as f64 * 100.0;
                (
                    Metric::Value(format!("{percent:.1}%")),
                    Some(percent),
                    format!(
                        "已用 {:.2} / {:.2} GiB（Windows 可见）",
                        snapshot.used_bytes() as f64 / gib,
                        snapshot.total_bytes as f64 / gib,
                    ),
                )
            }
            Ok(_) => (Metric::Failed("容量无效".into()), None, String::new()),
            Err(error) => (Metric::Failed(error.to_string()), None, String::new()),
        };
        Self {
            cpu,
            frequency,
            memory,
            cpu_percent,
            memory_percent,
            memory_detail,
        }
    }
}

fn start_sampler(
    selected: &Sources,
) -> io::Result<(SamplerThread, Receiver<()>, Arc<Latest<Sample>>)> {
    // 单槽信箱保存最新样本；通知通道也只有一个位置。
    let latest = Arc::new(Latest::default());
    let writer = Arc::clone(&latest);
    let (tx, rx) = mpsc::sync_channel(1);
    let sampler = SamplerThread::spawn(
        INTERVAL,
        selected.network_luid,
        selected.disk.clone(),
        move |sample| {
            writer.publish(sample);
            let _ = tx.try_send(());
        },
    )?;
    Ok((sampler, rx, latest))
}

fn schedule_sample(
    context: &ComponentContext<TelemetryPage>,
    receiver: Arc<Mutex<Receiver<()>>>,
    latest: Arc<Latest<Sample>>,
) {
    context.spawn_background_with_rejection(
        move |cancel| {
            // 最多占用线程池工作线程一秒；超时消息也会刷新样本年龄。
            for _ in 0..10 {
                if cancel.is_cancelled() {
                    return Message::SamplerStopped;
                }
                match receiver.lock().unwrap().recv_timeout(CANCEL_CHECK) {
                    Ok(()) => {
                        if let Some(sample) = latest.take() {
                            return Message::Sample(Box::new(sample));
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Message::SamplerStopped,
                }
            }
            Message::Tick
        },
        Message::DeliveryRejected,
    );
}

fn fresh(label: &str, stale: bool) -> String {
    if stale {
        "数据已过期".into()
    } else {
        label.into()
    }
}

fn info_card(title: &'static str, value: String, detail: String, accent: Color) -> View {
    Border::new()
        .background(CARD)
        .border_brush(STROKE)
        .border_thickness(1.0)
        .corner_radius(16.0)
        .padding(18.0)
        .content(
            StackPanel::new().spacing(8.0).children((
                TextBlock::new()
                    .text(title)
                    .font_size(12.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(accent),
                TextBlock::new()
                    .text(value)
                    .font_size(18.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(TEXT)
                    .text_wrapping(TextWrapping::Wrap),
                TextBlock::new()
                    .text(detail)
                    .font_size(12.0)
                    .foreground(MUTED)
                    .text_wrapping(TextWrapping::Wrap),
            )),
        )
}

fn gauge_card(
    title: &'static str,
    value: String,
    detail: String,
    percent: Option<f64>,
    accent: Color,
) -> View {
    let bar: View = percent.map_or_else(View::empty, |value| {
        ProgressBar::new()
            .minimum(0.0)
            .maximum(100.0)
            .value(value.clamp(0.0, 100.0))
            .into()
    });
    Border::new()
        .background(CARD)
        .border_brush(STROKE)
        .border_thickness(1.0)
        .corner_radius(18.0)
        .padding(20.0)
        .content(
            StackPanel::new().spacing(10.0).children((
                TextBlock::new()
                    .text(title)
                    .font_size(13.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(accent),
                TextBlock::new()
                    .text(value)
                    .font_size(34.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(TEXT)
                    .text_wrapping(TextWrapping::Wrap),
                bar,
                TextBlock::new()
                    .text(detail)
                    .font_size(12.0)
                    .foreground(MUTED)
                    .text_wrapping(TextWrapping::Wrap),
            )),
        )
}

struct TelemetryPage {
    sampler: Option<SamplerThread>,
    receiver: Option<Arc<Mutex<Receiver<()>>>>,
    latest: Arc<Latest<Sample>>,
    history: History,
    gauges: Option<Gauges>,
    last_sample_at: Option<Instant>,
    error: Option<String>,
}

impl Component for TelemetryPage {
    type Input = Sources;
    type Message = Message;

    fn create(input: &Sources, context: &ComponentContext<Self>) -> Self {
        match start_sampler(input) {
            Ok((sampler, receiver, latest)) => {
                let receiver = Arc::new(Mutex::new(receiver));
                schedule_sample(context, Arc::clone(&receiver), Arc::clone(&latest));
                let history = History {
                    network_name: input.network_name.clone(),
                    nvme_name: input.disk_name.clone(),
                    ..History::default()
                };
                Self {
                    sampler: Some(sampler),
                    receiver: Some(receiver),
                    latest,
                    history,
                    gauges: None,
                    last_sample_at: None,
                    error: None,
                }
            }
            Err(error) => Self {
                sampler: None,
                receiver: None,
                latest: Arc::new(Latest::default()),
                history: History::default(),
                gauges: None,
                last_sample_at: None,
                error: Some(format!("采样线程启动失败：{error}")),
            },
        }
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        match message {
            Message::Sample(sample) => {
                let gauges = Gauges::from_sample(&sample);
                self.last_sample_at = Some(sample.taken_at);
                self.history.push(*sample);
                self.gauges = Some(gauges);
                if let Some(receiver) = &self.receiver {
                    schedule_sample(context, Arc::clone(receiver), Arc::clone(&self.latest));
                }
            }
            Message::Tick => {
                if let Some(receiver) = &self.receiver {
                    schedule_sample(context, Arc::clone(receiver), Arc::clone(&self.latest));
                }
            }
            Message::SamplerStopped => {
                self.receiver = None;
                self.sampler = None;
                self.error = Some("采样线程已停止".into());
            }
            Message::DeliveryRejected => {
                self.receiver = None;
                self.sampler = None;
                self.error = Some("后台样本投递失败".into());
            }
        }
    }

    fn view(&self, _input: &Sources, context: &mut ViewContext<Self>) -> View {
        context.window_title("Telemetry · WinUI");
        let now = Instant::now();
        let age = self
            .last_sample_at
            .map(|at| now.saturating_duration_since(at));
        let stopped = self.error.is_some();
        let stale = stopped || is_stale(self.last_sample_at, now, MAX_SAMPLE_GAP);
        let status = if let Some(error) = &self.error {
            error.clone()
        } else if let Some(age) = age {
            if stale {
                format!("数据已过期：上次采样在 {:.1} 秒前", age.as_secs_f64())
            } else {
                format!("自动刷新中 · 上次采样在 {:.1} 秒前", age.as_secs_f64())
            }
        } else {
            "预热中 · 等待首份样本".into()
        };
        let label = |metric: fn(&Gauges) -> &Metric| {
            self.gauges
                .as_ref()
                .map_or_else(|| "预热中".into(), |gauges| metric(gauges).label(stale))
        };
        let cpu_percent = self
            .gauges
            .as_ref()
            .and_then(|gauges| gauges.cpu_percent)
            .filter(|_| !stale);
        let memory_percent = self
            .gauges
            .as_ref()
            .and_then(|gauges| gauges.memory_percent)
            .filter(|_| !stale);
        let memory_detail = if stale {
            "等待最新内存快照".into()
        } else {
            self.gauges.as_ref().map_or_else(
                || "Windows 可见物理内存".into(),
                |gauges| gauges.memory_detail.clone(),
            )
        };
        let network_at = self.history.points.back().map(|point| point.network_at);
        let network_value = if stopped || is_stale(network_at, now, MAX_SAMPLE_GAP) {
            "数据已过期".into()
        } else if let Some(point) = self.history.points.back() {
            match (point.download, point.upload) {
                (Some(down), Some(up)) => {
                    format!("↓ {}    ↑ {}", format_rate(down), format_rate(up))
                }
                _ => self.history.network_status.clone(),
            }
        } else {
            "等待采样".into()
        };
        let battery_stale = stopped || is_stale(self.history.battery_at, now, battery::STALE_AFTER);
        let nvme_stale = stopped || is_stale(self.history.nvme_at, now, nvme::STALE_AFTER);
        let pdh_stale = stopped || is_stale(self.history.pdh_at, now, MAX_SAMPLE_GAP);
        let battery_value = fresh(&self.history.battery_label, battery_stale);
        let nvme_value = fresh(&self.history.nvme_label, nvme_stale);
        let disk_value = fresh(&self.history.disk_label, pdh_stale);
        let gpu_value = fresh(&self.history.gpu_label, pdh_stale);
        let nvme_detail = if nvme_stale {
            format!("{} · 等待新快照", self.history.nvme_name)
        } else {
            format!("{} · {}", self.history.nvme_name, self.history.nvme_detail)
        };
        let battery_detail = if battery_stale {
            "等待新快照".into()
        } else {
            self.history.battery_detail.clone()
        };

        Border::new().background(BACKGROUND).content(
            ScrollViewer::new()
                .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled)
                .vertical_scroll_bar_visibility(ScrollBarVisibility::Auto)
                .content(
                    Border::new().padding(24.0).content(
                        StackPanel::new().spacing(16.0).children((
                            Border::new()
                                .background(HERO)
                                .corner_radius(20.0)
                                .padding(22.0)
                                .content(
                                    StackPanel::new().spacing(8.0).children((
                                        TextBlock::new()
                                            .text("WINDOWS • LIVE TELEMETRY")
                                            .font_size(11.0)
                                            .font_weight(FontWeight::SEMI_BOLD)
                                            .foreground(CYAN),
                                        TextBlock::new()
                                            .text("系统状态")
                                            .font_size(32.0)
                                            .font_weight(FontWeight::SEMI_BOLD)
                                            .foreground(TEXT),
                                        TextBlock::new()
                                            .text(status)
                                            .font_size(13.0)
                                            .foreground(MUTED)
                                            .text_wrapping(TextWrapping::Wrap),
                                    )),
                                ),
                            TextBlock::new()
                                .text("核心指标")
                                .font_size(18.0)
                                .font_weight(FontWeight::SEMI_BOLD)
                                .foreground(TEXT),
                            gauge_card(
                                "CPU / 总利用率",
                                label(|gauges| &gauges.cpu),
                                "全部逻辑处理器 · 最近一次采样区间".into(),
                                cpu_percent,
                                CYAN,
                            ),
                            gauge_card(
                                "RAM / 物理内存",
                                label(|gauges| &gauges.memory),
                                memory_detail,
                                memory_percent,
                                GREEN,
                            ),
                            info_card(
                                "CPU / 估算频率",
                                label(|gauges| &gauges.frequency),
                                "PDH _Total 估算 · 非单核瞬时时钟".into(),
                                VIOLET,
                            ),
                            TextBlock::new()
                                .text("设备活动")
                                .font_size(18.0)
                                .font_weight(FontWeight::SEMI_BOLD)
                                .foreground(TEXT),
                            info_card(
                                "WLAN / 吞吐",
                                network_value,
                                format!("选中接口：{} · 字节每秒", self.history.network_name),
                                CYAN,
                            ),
                            info_card(
                                "物理磁盘 / 总吞吐",
                                disk_value,
                                "PhysicalDisk _Total · 读写速率".into(),
                                AMBER,
                            ),
                            info_card(
                                "GPU / 活动引擎",
                                gpu_value,
                                "最忙的单个进程/引擎实例 · 非整卡利用率".into(),
                                VIOLET,
                            ),
                            info_card("NVMe / SMART", nvme_value, nvme_detail, GREEN),
                            info_card("电池 / 供电", battery_value, battery_detail, AMBER),
                            TextBlock::new()
                                .text("数据来自 Windows 系统接口；不同指标的刷新周期各自独立。")
                                .font_size(11.0)
                                .foreground(MUTED)
                                .text_wrapping(TextWrapping::Wrap),
                        )),
                    ),
                ),
        )
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let command = Command::parse(std::env::args().skip(1)).map_err(io::Error::other)?;
    match command {
        Command::Help => println!("{}", config::help("telemetry-reactor")),
        Command::ListNetwork => network::print_interfaces()?,
        Command::ListDisks => {
            for disk in nvme::list_disks()? {
                println!("{}  {}", disk.path, disk.name);
            }
        }
        Command::Run(config) => {
            let selected = sources::select(&config)?;
            App::run_component::<TelemetryPage>(selected)?;
        }
    }
    Ok(())
}
