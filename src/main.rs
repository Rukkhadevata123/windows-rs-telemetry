mod chart;
mod ui;

use std::{cell::RefCell, error::Error, io, rc::Rc, sync::Arc};

use windows_rs_telemetry::{
    config::{self, Command, Config},
    network, nvme,
    sampling::{INTERVAL, Latest, Sample, SamplerThread},
    sources,
};
use windows_window::Window;

fn main() -> Result<(), Box<dyn Error>> {
    let command = Command::parse(std::env::args().skip(1)).map_err(io::Error::other)?;
    match command {
        Command::Help => println!("{}", config::help("windows-rs-telemetry")),
        Command::ListNetwork => network::print_interfaces()?,
        Command::ListDisks => {
            for disk in nvme::list_disks()? {
                println!("{}  {}", disk.path, disk.name);
            }
        }
        Command::Run(config) => run_window(config)?,
    }
    Ok(())
}

fn run_window(config: Config) -> Result<(), Box<dyn Error>> {
    let sources = sources::select(&config)?;
    let latest: Arc<Latest<Sample>> = Arc::new(Latest::default());
    // Rc/RefCell 留在 UI 线程；信箱只在取放样本时持锁。
    let view = Rc::new(RefCell::new(ui::View::new()));
    {
        let mut state = view.borrow_mut();
        state.history.network_name = sources.network_name;
        state.history.nvme_name = sources.disk_name;
    }
    let ui_view = Rc::clone(&view);
    let ui_latest = Arc::clone(&latest);
    let (window_width, window_height) = initial_window_size();
    let window = Window::new("系统监控 — CPU / 内存 / WLAN / 电池 / NVMe / 磁盘 / GPU")
        .size(window_width, window_height)
        .on_message(move |hwnd, msg, _wparam, lparam| {
            if msg == ui::WM_APP_SAMPLE {
                if let Some(sample) = ui_latest.take() {
                    ui_view.borrow_mut().history.push(sample);
                    ui::request_redraw(hwnd);
                }
                return Some(0);
            }
            match msg as i32 {
                ui::WM_PAINT => {
                    // SAFETY：WM_PAINT 回调提供当前 UI 线程仍然有效的 HWND。
                    let result = unsafe { ui_view.borrow_mut().paint(hwnd) };
                    if let Err(error) = result {
                        eprintln!("Canvas 绘制失败：{error}");
                        ui_view.borrow_mut().error = Some(error);
                        windows_window::quit();
                    }
                    Some(0)
                }
                ui::WM_SIZE => {
                    ui::request_redraw(hwnd);
                    None
                }
                ui::DPI_CHANGED => {
                    // SAFETY：只有系统的 WM_DPICHANGED 分支才解释 lparam。
                    unsafe { ui::apply_dpi_rect(hwnd, lparam) };
                    Some(0)
                }
                ui::WM_ERASEBKGND => Some(1),
                ui::CLOSE => {
                    windows_window::quit();
                    Some(0)
                }
                _ => None,
            }
        })
        .create()?;

    let waker = ui::UiWaker::new(window.hwnd());
    let sampler = SamplerThread::spawn(
        INTERVAL,
        sources.network_luid,
        sources.disk,
        move |sample| {
            let was_empty = latest.publish(sample);
            if was_empty && !waker.wake() {
                // 投递失败时清空信箱，下一轮仍可重试。
                latest.take();
            }
        },
    )?;

    ui::request_redraw(window.hwnd());
    windows_window::run();
    sampler.stop();
    let mut state = view.borrow_mut();
    state.renderer = None;
    let error = state.error.take();
    drop(state);
    drop(window);
    if let Some(error) = error {
        return Err(error.into());
    }
    Ok(())
}

fn initial_window_size() -> (i32, i32) {
    use windows_rs_telemetry::bindings::{RECT, SPI_GETWORKAREA, SystemParametersInfoW};
    let mut work = RECT::default();
    // SPI_GETWORKAREA 按物理像素返回主显示器工作区；窗口尺寸也是外框物理像素。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA as u32,
            0,
            (&mut work as *mut RECT).cast(),
            0,
        )
    };
    if ok == 0 {
        return (1400, 1480);
    }
    let width = (work.right - work.left).max(1);
    let height = (work.bottom - work.top).max(1);
    (
        1400.min(width.saturating_sub(32).max(1)),
        1480.min(height.saturating_sub(32).max(1)),
    )
}
