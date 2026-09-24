//! Canvas 只读历史模型；尺寸是 DIP，交换链尺寸是物理像素。
use std::{ffi::c_void, time::Instant};
use windows_canvas::{
    Brush, ColorF, DrawingSession, Ellipse, GpuDevice, Matrix3x2, Rect, Result, SwapChain,
    TextFormat, Vector2,
};

use windows_rs_telemetry::format::rate as format_rate;
use windows_rs_telemetry::history::{HISTORY_SPAN, History, PENDING_LABEL, unless_stale};
use windows_rs_telemetry::sampling::{INTERVAL, MAX_SAMPLE_GAP, is_stale};
use windows_rs_telemetry::{battery, nvme};

/// 一帧的呈现结果；设备丢失时调用方重建 Renderer。
pub enum Frame {
    Presented,
    DeviceLost,
}

pub struct Renderer {
    // 交换链先于设备释放。
    chain: SwapChain,
    _device: GpuDevice,
    fonts: Fonts,
    scaled_fonts: Option<(f32, Fonts)>,
    dpi: u32,
}

pub struct Fonts {
    title: TextFormat,
    body: TextFormat,
    small: TextFormat,
}

const MARGIN: f32 = 24.0;
const PANEL_TOP: f32 = 306.0;
const FOOTER_SPACE: f32 = 28.0;
const MIN_PLOT_HEIGHT: f32 = 60.0;
const NETWORK_PLOT_INSET: f32 = 84.0;
const MIN_HEIGHT: f32 = PANEL_TOP + FOOTER_SPACE + 3.0 * (NETWORK_PLOT_INSET + MIN_PLOT_HEIGHT);
/// 低于这个宽或高就改画文字摘要。
const COMPACT_BELOW: f32 = 500.0;
/// 时间轴刻度把 HISTORY_SPAN 等分成几段。
const TIME_AXIS_STEPS: u32 = 4;

/// 中等高度时整张布局按比例缩小；None 表示原尺寸或文字摘要。
fn layout_scale(width: f32, height: f32) -> Option<f32> {
    (width >= COMPACT_BELOW && (COMPACT_BELOW..MIN_HEIGHT).contains(&height))
        .then(|| height / MIN_HEIGHT)
}

impl Fonts {
    pub fn new() -> Result<Self> {
        Self::for_layout_scale(1.0)
    }

    /// 字号先除以布局缩放抵消变换，再乘 scale.sqrt()：文字只缩小一部分，
    /// 否则行距随布局变小而字号不变，文字会互相重叠。
    fn for_layout_scale(scale: f32) -> Result<Self> {
        let factor = scale.sqrt() / scale;
        Ok(Self {
            title: TextFormat::new_bold("Microsoft YaHei UI", 22.0 * factor)?,
            body: TextFormat::new("Microsoft YaHei UI", 14.0 * factor)?,
            small: TextFormat::new("Microsoft YaHei UI", 11.0 * factor)?,
        })
    }
}

impl Renderer {
    /// # Safety
    ///
    /// 调用方保证 HWND 在 Renderer 被释放之前保持有效；只在 UI 线程调用。
    pub unsafe fn new(hwnd: *mut c_void, width: u32, height: u32, dpi: u32) -> Result<Self> {
        let device = GpuDevice::new_or_warp()?;
        // SAFETY：由本函数调用方保证窗口寿命；关闭流程先释放 Renderer，再销毁窗口。
        let mut chain = unsafe { device.create_swap_chain_for_hwnd(hwnd, width, height)? };
        chain.set_dpi(dpi as f32, dpi as f32);
        Ok(Self {
            chain,
            _device: device,
            fonts: Fonts::new()?,
            scaled_fonts: None,
            dpi,
        })
    }

    pub fn paint(
        &mut self,
        width: u32,
        height: u32,
        dpi: u32,
        history: &History,
        now: Instant,
    ) -> Result<Frame> {
        if self.chain.width() != width || self.chain.height() != height {
            self.chain.resize(width, height)?;
        }
        if self.dpi != dpi {
            self.chain.set_dpi(dpi as f32, dpi as f32);
            self.dpi = dpi;
        }
        let dip_width = width as f32 * 96.0 / dpi as f32;
        let dip_height = height as f32 * 96.0 / dpi as f32;
        match layout_scale(dip_width, dip_height) {
            Some(scale)
                if self
                    .scaled_fonts
                    .as_ref()
                    .is_none_or(|(cached, _)| (cached - scale).abs() > 0.01) =>
            {
                self.scaled_fonts = Some((scale, Fonts::for_layout_scale(scale)?));
            }
            Some(_) => {}
            None => self.scaled_fonts = None,
        }
        {
            let session = self.chain.begin_draw()?;
            let fonts = self
                .scaled_fonts
                .as_ref()
                .map_or(&self.fonts, |(_, fonts)| fonts);
            draw(&session, fonts, dip_width, dip_height, history, now)?;
        } // Drop 调用 EndDraw；结束绘制后才能 Present。
        Ok(if self.chain.present()? {
            Frame::Presented
        } else {
            Frame::DeviceLost
        })
    }
}

fn point(x: f32, y: f32) -> Vector2 {
    Vector2 { x, y }
}

/// 与 HWND 无关，可使用历史样本验证绘图。
pub fn draw(
    session: &DrawingSession<'_>,
    fonts: &Fonts,
    width: f32,
    height: f32,
    history: &History,
    now: Instant,
) -> Result<()> {
    if width < COMPACT_BELOW || height < COMPACT_BELOW {
        return draw_compact(session, fonts, width, height, history, now);
    }
    if let Some(scale) = layout_scale(width, height) {
        let previous = session.transform();
        session.set_transform(&Matrix3x2 {
            m11: previous.m11 * scale,
            m12: previous.m12 * scale,
            m21: previous.m21 * scale,
            m22: previous.m22 * scale,
            m31: previous.m31,
            m32: previous.m32,
        });
        let result = draw_full(session, fonts, width / scale, MIN_HEIGHT, history, now);
        session.set_transform(&previous);
        return result;
    }
    draw_full(session, fonts, width, height, history, now)
}

fn draw_compact(
    session: &DrawingSession<'_>,
    fonts: &Fonts,
    width: f32,
    height: f32,
    history: &History,
    now: Instant,
) -> Result<()> {
    session.clear(ColorF::rgb(0.035, 0.055, 0.09));
    let text = session.create_solid_brush(ColorF::rgb(0.89, 0.93, 0.98))?;
    let muted = session.create_solid_brush(ColorF::rgb(0.53, 0.62, 0.73))?;
    session.draw_text(
        "系统监控",
        &fonts.title,
        &Rect::from_xywh(20.0, 14.0, (width - 40.0).max(1.0), 36.0),
        &text,
    );
    let latest = history.points.back();
    let sample_stale = is_stale(history.last_sample_at(), now, MAX_SAMPLE_GAP);
    let network_stale =
        latest.is_some_and(|point| is_stale(Some(point.network_at), now, MAX_SAMPLE_GAP));
    let network = latest.and_then(|point| Some((point.download?, point.upload?)));
    let network_label = network.map_or_else(
        || history.network_status.clone(),
        |(down, up)| format!("↓ {}  ↑ {}", format_rate(down), format_rate(up)),
    );
    let pdh_stale = is_stale(history.pdh_at, now, MAX_SAMPLE_GAP);
    let nvme_stale = is_stale(history.nvme_at, now, nvme::STALE_AFTER);
    let battery_stale = is_stale(history.battery_at, now, battery::STALE_AFTER);
    let lines = [
        format!("CPU  {}", unless_stale(&history.cpu_label(), sample_stale)),
        format!("内存  {}", unless_stale(&history.ram_label(), sample_stale)),
        format!("WLAN  {}", unless_stale(&network_label, network_stale)),
        format!("磁盘  {}", unless_stale(&history.disk_label, pdh_stale)),
        format!("GPU  {}", unless_stale(&history.gpu_label, pdh_stale)),
        format!("NVMe  {}", unless_stale(&history.nvme_label, nvme_stale)),
        format!(
            "电池  {}",
            unless_stale(&history.battery_label, battery_stale)
        ),
    ];
    let row_height = ((height - 65.0) / lines.len() as f32).clamp(24.0, 40.0);
    for (index, line) in lines.iter().enumerate() {
        let y = 62.0 + index as f32 * row_height;
        if y >= height {
            break;
        }
        session.draw_text(
            line,
            &fonts.body,
            &Rect::from_xywh(
                20.0,
                y,
                (width - 40.0).max(1.0),
                (row_height - 4.0).max(1.0),
            ),
            if index % 2 == 0 { &text } else { &muted },
        );
    }
    Ok(())
}

fn draw_full(
    session: &DrawingSession<'_>,
    fonts: &Fonts,
    width: f32,
    height: f32,
    history: &History,
    now: Instant,
) -> Result<()> {
    session.clear(ColorF::rgb(0.035, 0.055, 0.09));
    let text = session.create_solid_brush(ColorF::rgb(0.89, 0.93, 0.98))?;
    let muted = session.create_solid_brush(ColorF::rgb(0.53, 0.62, 0.73))?;
    let grid = session.create_solid_brush(ColorF::rgb(0.16, 0.22, 0.30))?;
    let cpu = session.create_solid_brush(ColorF::rgb(0.22, 0.78, 0.96))?;
    let ram = session.create_solid_brush(ColorF::rgb(0.48, 0.86, 0.65))?;
    let download_brush = session.create_solid_brush(ColorF::rgb(0.70, 0.56, 1.0))?;
    let upload_brush = session.create_solid_brush(ColorF::rgb(1.0, 0.66, 0.30))?;
    let content_width = width - 2.0 * MARGIN;
    // 占满内容宽度的一行文字。
    let line = |label: &str, font: &TextFormat, top: f32, height: f32, brush: &Brush| {
        session.draw_text(
            label,
            font,
            &Rect::from_xywh(MARGIN, top, content_width, height),
            brush,
        );
    };
    line("系统监控", &fonts.title, 18.0, 34.0, &text);
    line(&subtitle(), &fonts.small, 54.0, 20.0, &muted);

    // 电池慢速刷新，只显示快照，不在 60 秒曲线上伪造平滑变化。
    let battery_stale = is_stale(history.battery_at, now, battery::STALE_AFTER);
    let (basic, detail) = if battery_stale {
        ("电池数据已过期，等待新样本", "")
    } else {
        (
            history.battery_label.as_str(),
            history.battery_detail.as_str(),
        )
    };
    line(&format!("电池  {basic}"), &fonts.body, 84.0, 24.0, &ram);
    line(detail, &fonts.body, 112.0, 24.0, &text);
    line(
        "功率：+ 充电 / − 放电  ·  电池侧功率  ·  续航为估算值",
        &fonts.small,
        140.0,
        20.0,
        &muted,
    );
    let nvme_stale = is_stale(history.nvme_at, now, nvme::STALE_AFTER);
    let nvme_label = if nvme_stale {
        "数据已过期，等待新样本"
    } else {
        &history.nvme_label
    };
    line(
        &format!("NVMe / {}  {nvme_label}", history.nvme_name),
        &fonts.body,
        168.0,
        24.0,
        &text,
    );
    if !nvme_stale {
        let age = history
            .nvme_at
            .map(|at| format!("  ·  {} 秒前", now.saturating_duration_since(at).as_secs()))
            .unwrap_or_default();
        line(
            &format!("{}{}", history.nvme_detail, age),
            &fonts.small,
            192.0,
            20.0,
            &muted,
        );
    }
    let pdh_stale = is_stale(history.pdh_at, now, MAX_SAMPLE_GAP);
    line(
        &format!(
            "物理磁盘 / _Total  {}",
            unless_stale(&history.disk_label, pdh_stale)
        ),
        &fonts.body,
        220.0,
        24.0,
        &text,
    );
    line(
        &format!(
            "GPU / 最忙单引擎实例  {}",
            unless_stale(&history.gpu_label, pdh_stale)
        ),
        &fonts.body,
        248.0,
        24.0,
        &text,
    );
    line(
        "磁盘：PhysicalDisk 总读写速率  ·  GPU：单个进程/引擎实例，不是整卡利用率",
        &fonts.small,
        275.0,
        20.0,
        &muted,
    );
    let panel_height = (height - PANEL_TOP - FOOTER_SPACE) / 3.0;
    let stale = is_stale(history.last_sample_at(), now, MAX_SAMPLE_GAP);
    for (index, name, label, color, series) in [
        (
            0,
            "CPU",
            history.cpu_label(),
            &cpu,
            history.series(now, |p| p.cpu),
        ),
        (
            1,
            "RAM",
            history.ram_label(),
            &ram,
            history.series(now, |p| p.ram),
        ),
    ] {
        let top = PANEL_TOP + index as f32 * panel_height;
        let label = if history.points.is_empty() {
            PENDING_LABEL
        } else if stale {
            "数据已过期，等待新样本"
        } else {
            &label
        };
        line(&format!("{name}   {label}"), &fonts.body, top, 26.0, color);
        let plot = Rect::new(110.0, top + 32.0, width - 28.0, top + panel_height - 32.0);
        draw_axes(
            session,
            fonts,
            plot,
            ["0%".into(), "50%".into(), "100%".into()],
            (&grid, &muted),
        );
        draw_series(session, &series, plot, color, 1.0);
    }

    // 上传和下载共用一个尺度，历史数据仍是 B/s，不预先归一化。
    let download = history.network_series(now, |p| p.download);
    let upload = history.network_series(now, |p| p.upload);
    let maximum = rate_ceiling(&download, &upload);
    let top = PANEL_TOP + 2.0 * panel_height;
    let latest = history.points.back();
    let stale_network = latest.is_some_and(|p| is_stale(Some(p.network_at), now, MAX_SAMPLE_GAP));
    line(
        &format!(
            "网络 / {}  {}",
            history.network_name,
            unless_stale(&history.network_status, stale_network)
        ),
        &fonts.body,
        top,
        24.0,
        &text,
    );
    let latest = latest.filter(|_| !stale_network);
    let half_width = content_width / 2.0;
    for (index, name, value, brush) in [
        (0, "下载", latest.and_then(|p| p.download), &download_brush),
        (1, "上传", latest.and_then(|p| p.upload), &upload_brush),
    ] {
        let label = value.map(format_rate).unwrap_or_else(|| "—".into());
        session.draw_text(
            &format!("{name}  {label}"),
            &fonts.small,
            &Rect::from_xywh(
                MARGIN + index as f32 * half_width,
                top + 25.0,
                half_width,
                20.0,
            ),
            brush,
        );
    }
    let plot = Rect::new(110.0, top + 52.0, width - 28.0, top + panel_height - 32.0);
    draw_axes(
        session,
        fonts,
        plot,
        [
            format_rate(0.0),
            format_rate(maximum as f64 / 2.0),
            format_rate(maximum as f64),
        ],
        (&grid, &muted),
    );
    draw_series(session, &download, plot, &download_brush, maximum);
    draw_series(session, &upload, plot, &upload_brush, maximum);
    line(
        "RAM：物理内存占比  ·  网络：仅选中接口，纵轴随历史峰值调整",
        &fonts.small,
        height - 24.0,
        20.0,
        &muted,
    );
    Ok(())
}

/// 各数据源的刷新周期都来自对应常量，改间隔时文案不会过时。
fn subtitle() -> String {
    format!(
        "曲线：最近 {} 秒 / 每 {} 秒采样  ·  电池 {} 秒  ·  NVMe {} 秒  ·  磁盘/GPU 约 {} 秒",
        HISTORY_SPAN.as_secs_f64(),
        INTERVAL.as_secs_f64(),
        battery::INTERVAL.as_secs_f64(),
        nvme::INTERVAL.as_secs_f64(),
        INTERVAL.as_secs_f64(),
    )
}

fn draw_axes(
    session: &DrawingSession<'_>,
    fonts: &Fonts,
    plot: Rect,
    labels: [String; 3],
    brushes: (&Brush, &Brush),
) {
    let (grid, muted) = brushes;
    for (index, label) in labels.iter().enumerate() {
        let y = plot.bottom - plot.height() * index as f32 / 2.0;
        session.draw_line(point(plot.left, y), point(plot.right, y), grid, 1.0);
        session.draw_text(
            label,
            &fonts.small,
            &Rect::from_xywh(MARGIN, y - 8.0, 84.0, 18.0),
            muted,
        );
    }
    for step in (0..=TIME_AXIS_STEPS).rev() {
        let fraction = step as f32 / TIME_AXIS_STEPS as f32;
        let x = plot.right - plot.width() * fraction;
        session.draw_line(point(x, plot.top), point(x, plot.bottom), grid, 1.0);
        let label = if step == 0 {
            "现在".into()
        } else {
            format!("-{:.0}s", HISTORY_SPAN.as_secs_f32() * fraction)
        };
        session.draw_text(
            &label,
            &fonts.small,
            &Rect::from_xywh(x - 16.0, plot.bottom + 5.0, 38.0, 20.0),
            muted,
        );
    }
}

/// 最低 1 KiB/s，以 2 的幂取整；上下行共用 HISTORY_SPAN 内的同一个峰值。
fn rate_ceiling(download: &[Option<(f32, f32)>], upload: &[Option<(f32, f32)>]) -> f32 {
    let peak = download
        .iter()
        .chain(upload)
        .flatten()
        .fold(1024.0_f32, |peak, (_, value)| peak.max(*value));
    peak.log2().ceil().exp2()
}

fn draw_series(
    session: &DrawingSession<'_>,
    series: &[Option<(f32, f32)>],
    plot: Rect,
    brush: &Brush,
    maximum: f32,
) {
    let mut previous = None;
    for value in series {
        let current = value.map(|(x, y)| {
            point(
                plot.left + x * plot.width(),
                plot.bottom - (y / maximum) * plot.height(),
            )
        });
        if let Some(p) = current {
            if let Some(prev) = previous {
                session.draw_line(prev, p, brush, 2.0);
            }
            // 单个有效样本也可见，不需要伪造前一个值来连线。
            session.fill_ellipse(&Ellipse::circle(p, 1.8), brush);
        }
        previous = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use windows_rs_telemetry::{
        cpu::CpuUsage, memory::MemorySnapshot, network::NetworkUsage, sampling::Sample,
    };

    fn render_history(now: Instant) -> History {
        let mut history = History::default();
        for index in 0..=60 {
            let at = now - Duration::from_secs(60 - index);
            history.push(Sample {
                taken_at: at,
                cpu: Ok(CpuUsage::Busy(0.35 + index as f64 * 0.005)),
                cpu_mhz: Ok(Some(2500.0)),
                memory: Ok(MemorySnapshot {
                    total_bytes: 32_u64 << 30,
                    available_bytes: (16_u64 << 30) - index * (1_u64 << 27),
                }),
                network_at: at,
                network: Ok(NetworkUsage::Transfer {
                    download_bytes_per_sec: (1.0 + index as f64 * 0.01) * 1024.0 * 1024.0,
                    upload_bytes_per_sec: (0.4 + index as f64 * 0.005) * 1024.0 * 1024.0,
                }),
                battery: None,
                nvme: None,
                pdh: None,
            });
        }
        history
    }

    #[test]
    fn network_scale_covers_both_directions_and_has_a_nonzero_floor() {
        assert_eq!(rate_ceiling(&[], &[]), 1024.0);
        assert_eq!(rate_ceiling(&[Some((1.0, 1536.0))], &[None]), 2048.0);
        assert_eq!(
            rate_ceiling(
                &[Some((1.0, 1536.0))],
                &[Some((1.0, 3.0 * 1024.0 * 1024.0))]
            ),
            4.0 * 1024.0 * 1024.0
        );
    }

    #[test]
    fn renders_to_warp_at_normal_compact_and_high_dpi_sizes() -> Result<()> {
        let device = GpuDevice::new_warp()?;
        let now = Instant::now();
        let history = render_history(now);
        for (width, height, scale) in [
            (900, 800, 1.0_f32),
            (900, 640, 1.0),
            (900, 500, 1.0),
            (450, 400, 1.0),
            (320, 240, 1.0),
            (1800, 1280, 2.0),
            (1800, 1600, 2.0),
        ] {
            let fonts = match layout_scale(width as f32 / scale, height as f32 / scale) {
                Some(layout) => Fonts::for_layout_scale(layout)?,
                None => Fonts::new()?,
            };
            let target = device.create_render_target(width, height)?;
            target.draw(|session| {
                session.set_transform(&windows_canvas::Matrix3x2 {
                    m11: scale,
                    m12: 0.0,
                    m21: 0.0,
                    m22: scale,
                    m31: 0.0,
                    m32: 0.0,
                });
                draw(
                    session,
                    &fonts,
                    width as f32 / scale,
                    height as f32 / scale,
                    &history,
                    now,
                )
            })?;
            let pixels = target.read_pixels()?;
            // 实际绘制了非背景像素；大尺寸还应同时包含蓝色 CPU 和绿色 RAM。
            let blue = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[0] > 150 && p[1] > 130 && p[2] < 150)
                .count();
            let green = pixels
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|p| p[1] > 170 && i16::from(p[1]) > i16::from(p[2]) + 20 && p[0] < 200)
                .count();
            if width >= 900 {
                assert!(blue > 100 && green > 100);
                // 在网络图的绘图区内找两种线色，避免把图例文字当成曲线。
                let network_pixels = pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| index / width as usize > (height * 3 / 4) as usize)
                    .map(|(_, pixel)| pixel)
                    .collect::<Vec<_>>();
                let purple = network_pixels
                    .iter()
                    .filter(|p| p[0] > 210 && p[2] > 130 && p[2] < 210 && p[1] < 170)
                    .count();
                let orange = network_pixels
                    .iter()
                    .filter(|p| p[2] > 220 && p[1] > 120 && p[1] < 200 && p[0] < 130)
                    .count();
                assert!(
                    purple > 30 && orange > 30,
                    "network curves: purple={purple}, orange={orange}"
                );
            }
        }
        Ok(())
    }
}
