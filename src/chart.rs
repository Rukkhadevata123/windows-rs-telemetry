//! Canvas 只读历史模型；尺寸是 DIP，交换链尺寸是物理像素。
use std::{ffi::c_void, time::Instant};
use windows_canvas::{
    Brush, ColorF, DrawingSession, Ellipse, GpuDevice, Rect, Result, SwapChain, TextFormat, Vector2,
};

use windows_rs_telemetry::history::{History, MAX_GAP};
use windows_rs_telemetry::network::format_rate;

pub struct Renderer {
    // 交换链先于设备释放。
    chain: SwapChain,
    _device: GpuDevice,
    fonts: Fonts,
    dpi: u32,
}

pub struct Fonts {
    title: TextFormat,
    body: TextFormat,
    small: TextFormat,
}

impl Fonts {
    pub fn new() -> Result<Self> {
        Ok(Self {
            title: TextFormat::new_bold("Microsoft YaHei UI", 22.0)?,
            body: TextFormat::new("Microsoft YaHei UI", 14.0)?,
            small: TextFormat::new("Microsoft YaHei UI", 11.0)?,
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
    ) -> Result<bool> {
        if self.chain.width() != width || self.chain.height() != height {
            self.chain.resize(width, height)?;
        }
        if self.dpi != dpi {
            self.chain.set_dpi(dpi as f32, dpi as f32);
            self.dpi = dpi;
        }
        {
            let session = self.chain.begin_draw()?;
            draw(
                &session,
                &self.fonts,
                width as f32 * 96.0 / dpi as f32,
                height as f32 * 96.0 / dpi as f32,
                history,
                now,
            )?;
        } // Drop 调用 EndDraw；结束绘制后才能 Present。
        self.chain.present() // false 是设备丢失，不是成功呈现。
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
    session.clear(ColorF::rgb(0.035, 0.055, 0.09));
    let text = session.create_solid_brush(ColorF::rgb(0.89, 0.93, 0.98))?;
    let muted = session.create_solid_brush(ColorF::rgb(0.53, 0.62, 0.73))?;
    let grid = session.create_solid_brush(ColorF::rgb(0.16, 0.22, 0.30))?;
    let cpu = session.create_solid_brush(ColorF::rgb(0.22, 0.78, 0.96))?;
    let ram = session.create_solid_brush(ColorF::rgb(0.48, 0.86, 0.65))?;
    let download_brush = session.create_solid_brush(ColorF::rgb(0.70, 0.56, 1.0))?;
    let upload_brush = session.create_solid_brush(ColorF::rgb(1.0, 0.66, 0.30))?;
    let heading = "系统监控 / CPU + RAM + WLAN + 电池 + NVMe + PDH";
    session.draw_text(
        heading,
        &fonts.title,
        &Rect::from_xywh(24.0, 18.0, width - 48.0, 34.0),
        &text,
    );
    if width < 500.0 || height < 600.0 {
        session.draw_text(
            "放大窗口以查看最近 60 秒曲线",
            &fonts.body,
            &Rect::from_xywh(24.0, 65.0, (width - 48.0).max(1.0), 60.0),
            &muted,
        );
        return Ok(());
    }
    session.draw_text(
        "曲线：最近 60 秒 / 每秒采样  ·  电池 3 秒  ·  NVMe 30 秒  ·  PDH 约 1 秒",
        &fonts.small,
        &Rect::from_xywh(24.0, 54.0, width - 48.0, 20.0),
        &muted,
    );

    // 电池慢速刷新，只显示快照，不在 60 秒曲线上伪造平滑变化。
    let battery_stale = history.battery_at.is_some_and(|at| {
        now.saturating_duration_since(at) > windows_rs_telemetry::battery::STALE_AFTER
    });
    let basic = if battery_stale {
        "电池数据已过期，等待新样本"
    } else {
        &history.battery_label
    };
    let detail = if battery_stale {
        ""
    } else {
        &history.battery_detail
    };
    session.draw_text(
        &format!("电池  {basic}"),
        &fonts.body,
        &Rect::from_xywh(24.0, 84.0, width - 48.0, 24.0),
        &ram,
    );
    session.draw_text(
        detail,
        &fonts.body,
        &Rect::from_xywh(24.0, 112.0, width - 48.0, 24.0),
        &text,
    );
    session.draw_text(
        "功率：+ 充电 / − 放电  ·  电池侧功率  ·  续航为估算值",
        &fonts.small,
        &Rect::from_xywh(24.0, 140.0, width - 48.0, 20.0),
        &muted,
    );
    let nvme_stale = history.nvme_at.is_some_and(|at| {
        now.saturating_duration_since(at) > windows_rs_telemetry::nvme::STALE_AFTER
    });
    let nvme_label = if nvme_stale {
        "数据已过期，等待新样本"
    } else {
        &history.nvme_label
    };
    session.draw_text(
        &format!("NVMe / {}  {nvme_label}", history.nvme_name),
        &fonts.body,
        &Rect::from_xywh(24.0, 168.0, width - 48.0, 24.0),
        &text,
    );
    if !nvme_stale {
        let age = history
            .nvme_at
            .map(|at| format!("  ·  {} 秒前", now.saturating_duration_since(at).as_secs()))
            .unwrap_or_default();
        session.draw_text(
            &format!("{}{}", history.nvme_detail, age),
            &fonts.small,
            &Rect::from_xywh(24.0, 192.0, width - 48.0, 20.0),
            &muted,
        );
    }
    let pdh_stale = history
        .pdh_at
        .is_some_and(|at| now.saturating_duration_since(at) > MAX_GAP);
    let disk_label = if pdh_stale {
        "数据已过期"
    } else {
        &history.disk_label
    };
    let gpu_label = if pdh_stale {
        "数据已过期"
    } else {
        &history.gpu_label
    };
    session.draw_text(
        &format!("物理磁盘 / _Total  {disk_label}"),
        &fonts.body,
        &Rect::from_xywh(24.0, 220.0, width - 48.0, 24.0),
        &text,
    );
    session.draw_text(
        &format!("GPU / 最忙单引擎实例  {gpu_label}"),
        &fonts.body,
        &Rect::from_xywh(24.0, 248.0, width - 48.0, 24.0),
        &text,
    );
    session.draw_text(
        "磁盘：PhysicalDisk 总读写速率  ·  GPU：单个进程/引擎实例，不是整卡利用率",
        &fonts.small,
        &Rect::from_xywh(24.0, 275.0, width - 48.0, 20.0),
        &muted,
    );
    let panel_top = 306.0;
    let panel_height = (height - panel_top - 28.0) / 3.0;
    let stale = history
        .points
        .back()
        .is_none_or(|p| now.saturating_duration_since(p.at) > MAX_GAP);
    for (index, name, label, color, series) in [
        (
            0,
            "CPU",
            &history.cpu_label,
            &cpu,
            history.series(now, |p| p.cpu),
        ),
        (
            1,
            "RAM",
            &history.ram_label,
            &ram,
            history.series(now, |p| p.ram),
        ),
    ] {
        let top = panel_top + index as f32 * panel_height;
        let label = if stale && !history.points.is_empty() {
            "数据已过期，等待新样本"
        } else if history.points.is_empty() {
            "等待采样"
        } else {
            label.as_str()
        };
        session.draw_text(
            &format!("{name}   {label}"),
            &fonts.body,
            &Rect::from_xywh(24.0, top, width - 48.0, 26.0),
            color,
        );
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
    let top = panel_top + 2.0 * panel_height;
    let stale_network = history
        .points
        .back()
        .is_none_or(|p| now.saturating_duration_since(p.network_at) > MAX_GAP);
    let status = if stale_network && !history.points.is_empty() {
        "数据已过期"
    } else {
        &history.network_status
    };
    session.draw_text(
        &format!("网络 / {}  {status}", history.network_name),
        &fonts.body,
        &Rect::from_xywh(24.0, top, width - 48.0, 24.0),
        &text,
    );
    let latest = history.points.back().filter(|_| !stale_network);
    for (index, name, value, brush) in [
        (0, "下载", latest.and_then(|p| p.download), &download_brush),
        (1, "上传", latest.and_then(|p| p.upload), &upload_brush),
    ] {
        let label = value.map(format_rate).unwrap_or_else(|| "—".into());
        session.draw_text(
            &format!("{name}  {label}"),
            &fonts.small,
            &Rect::from_xywh(
                24.0 + index as f32 * (width - 48.0) / 2.0,
                top + 25.0,
                (width - 48.0) / 2.0,
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
    session.draw_text(
        "RAM：物理内存占比  ·  网络：仅选中接口，纵轴随历史峰值调整",
        &fonts.small,
        &Rect::from_xywh(24.0, height - 24.0, width - 48.0, 20.0),
        &muted,
    );
    Ok(())
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
            &Rect::from_xywh(24.0, y - 8.0, 84.0, 18.0),
            muted,
        );
    }
    for seconds in [60, 45, 30, 15, 0] {
        let x = plot.right - plot.width() * seconds as f32 / 60.0;
        session.draw_line(point(x, plot.top), point(x, plot.bottom), grid, 1.0);
        let label = if seconds == 0 {
            "现在".into()
        } else {
            format!("-{seconds}s")
        };
        session.draw_text(
            &label,
            &fonts.small,
            &Rect::from_xywh(x - 16.0, plot.bottom + 5.0, 38.0, 20.0),
            muted,
        );
    }
}

/// 最低 1 KiB/s，以 2 的幂取整；上下行使用同一个最近 60 秒的峰值。
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
        let fonts = Fonts::new()?;
        let now = Instant::now();
        let history = render_history(now);
        for (width, height, scale) in [(900, 640, 1.0_f32), (320, 240, 1.0), (1800, 1280, 2.0)] {
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
            // 可选保存真实渲染的 BMP，便于人工检查布局，不影响普通测试。
            if let Some(dir) = std::env::var_os("TELEMETRY_PREVIEW_DIR") {
                let path =
                    std::path::PathBuf::from(dir).join(format!("canvas-{width}x{height}.bmp"));
                let mut bmp = Vec::new();
                bmp.extend(b"BM");
                bmp.extend((54 + pixels.len() as u32).to_le_bytes());
                bmp.extend([0_u8; 4]);
                bmp.extend(54_u32.to_le_bytes());
                bmp.extend(40_u32.to_le_bytes());
                bmp.extend((width as i32).to_le_bytes());
                bmp.extend((-(height as i32)).to_le_bytes());
                bmp.extend(1_u16.to_le_bytes());
                bmp.extend(32_u16.to_le_bytes());
                bmp.extend([0_u8; 24]);
                bmp.extend(pixels);
                std::fs::write(path, bmp).unwrap();
            }
        }
        Ok(())
    }
}
