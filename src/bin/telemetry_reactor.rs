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
    battery, format,
    history::{History, PENDING_LABEL, STALE_LABEL, unless_stale},
    nvme,
    sampling::{INTERVAL, Latest, MAX_SAMPLE_GAP, Sample, SamplerThread, is_stale},
    sources::{self, Sources},
};

const CANCEL_CHECK: Duration = Duration::from_millis(100);
/// 一次后台任务最多等一个采样周期；超时消息也会刷新样本年龄。
const CHECKS_PER_TICK: u128 = INTERVAL.as_millis() / CANCEL_CHECK.as_millis();
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
            for _ in 0..CHECKS_PER_TICK {
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

fn section_title(text: &'static str) -> TextBlock {
    TextBlock::new()
        .text(text)
        .font_size(18.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .foreground(TEXT)
}

fn card_frame(corner_radius: f64, padding: f64, content: View) -> View {
    Border::new()
        .background(CARD)
        .border_brush(STROKE)
        .border_thickness(1.0)
        .corner_radius(corner_radius)
        .padding(padding)
        .content(content)
}

fn info_card(title: &'static str, value: String, detail: String, accent: Color) -> View {
    card_frame(
        16.0,
        18.0,
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
    card_frame(
        18.0,
        20.0,
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
    error: Option<String>,
}

impl Component for TelemetryPage {
    type Input = Sources;
    type Message = Message;

    fn create(input: &Sources, context: &ComponentContext<Self>) -> Self {
        let history = History::for_sources(input);
        match start_sampler(input) {
            Ok((sampler, receiver, latest)) => {
                let receiver = Arc::new(Mutex::new(receiver));
                schedule_sample(context, Arc::clone(&receiver), Arc::clone(&latest));
                Self {
                    sampler: Some(sampler),
                    receiver: Some(receiver),
                    latest,
                    history,
                    error: None,
                }
            }
            Err(error) => Self {
                sampler: None,
                receiver: None,
                latest: Arc::new(Latest::default()),
                history,
                error: Some(format!("采样线程启动失败：{error}")),
            },
        }
    }

    fn update(&mut self, message: Message, context: &ComponentContext<Self>) {
        match message {
            Message::Sample(sample) => {
                self.history.push(*sample);
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
        let history = &self.history;
        let last_sample_at = history.last_sample_at();
        let stopped = self.error.is_some();
        let stale = stopped || is_stale(last_sample_at, now, MAX_SAMPLE_GAP);
        let status = if let Some(error) = &self.error {
            error.clone()
        } else if let Some(at) = last_sample_at {
            let age = now.saturating_duration_since(at).as_secs_f64();
            if stale {
                format!("{STALE_LABEL}：上次采样在 {age:.1} 秒前")
            } else {
                format!("自动刷新中 · 上次采样在 {age:.1} 秒前")
            }
        } else {
            "预热中 · 等待首份样本".into()
        };
        let fresh = |label: String| unless_stale(&label, stale).to_owned();
        let cpu_value = fresh(history.cpu.label(|ratio| format::percent(*ratio)));
        let memory_value = fresh(history.memory.label(|m| format::percent(m.ratio)));
        let frequency_value = fresh(history.cpu_mhz.label(|mhz| format::ghz(*mhz)));
        let cpu_percent = history
            .cpu
            .value()
            .map(|ratio| ratio * 100.0)
            .filter(|_| !stale);
        let memory_percent = history
            .memory
            .value()
            .map(|m| m.ratio * 100.0)
            .filter(|_| !stale);
        let memory_detail = match history.memory.value() {
            _ if stale => "等待最新内存快照".into(),
            Some(m) => format!(
                "已用 {}（Windows 可见）",
                format::gib_pair(m.used_bytes, m.total_bytes)
            ),
            None => "Windows 可见物理内存".into(),
        };
        let network_point = history.points.back();
        let network_stale = stopped
            || is_stale(
                network_point.map(|point| point.network_at),
                now,
                MAX_SAMPLE_GAP,
            );
        let network_value = match network_point {
            _ if network_stale => STALE_LABEL.into(),
            None => PENDING_LABEL.into(),
            Some(point) => match (point.download, point.upload) {
                (Some(down), Some(up)) => {
                    format!("↓ {}    ↑ {}", format::rate(down), format::rate(up))
                }
                _ => history.network_status.clone(),
            },
        };
        let battery_stale = stopped || is_stale(history.battery_at, now, battery::STALE_AFTER);
        let nvme_stale = stopped || is_stale(history.nvme_at, now, nvme::STALE_AFTER);
        let pdh_stale = stopped || is_stale(history.pdh_at, now, MAX_SAMPLE_GAP);
        let battery_value = unless_stale(&history.battery_label, battery_stale).to_owned();
        let nvme_value = unless_stale(&history.nvme_label, nvme_stale).to_owned();
        let disk_value = unless_stale(&history.disk_label, pdh_stale).to_owned();
        let gpu_value = unless_stale(&history.gpu_label, pdh_stale).to_owned();
        let nvme_detail = if nvme_stale {
            format!("{} · 等待新快照", history.nvme_name)
        } else {
            format!("{} · {}", history.nvme_name, history.nvme_detail)
        };
        let battery_detail = if battery_stale {
            "等待新快照".into()
        } else {
            history.battery_detail.clone()
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
                            section_title("核心指标"),
                            gauge_card(
                                "CPU / 总利用率",
                                cpu_value,
                                "全部逻辑处理器 · 最近一次采样区间".into(),
                                cpu_percent,
                                CYAN,
                            ),
                            gauge_card(
                                "RAM / 物理内存",
                                memory_value,
                                memory_detail,
                                memory_percent,
                                GREEN,
                            ),
                            info_card(
                                "CPU / 估算频率",
                                frequency_value,
                                "PDH _Total 估算 · 非单核瞬时时钟".into(),
                                VIOLET,
                            ),
                            section_title("设备活动"),
                            info_card(
                                "WLAN / 吞吐",
                                network_value,
                                format!("选中接口：{} · 字节每秒", history.network_name),
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
    if let Some(config) = sources::handle_command_line("telemetry-reactor")? {
        let selected = sources::select(&config)?;
        App::run_component::<TelemetryPage>(selected)?;
    }
    Ok(())
}
