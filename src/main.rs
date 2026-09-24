#![cfg_attr(not(feature = "console"), windows_subsystem = "windows")]

mod chart;
mod ui;

use std::{cell::RefCell, error::Error, rc::Rc, sync::Arc};

use windows_rs_telemetry::{
    config::Config,
    history::History,
    sampling::{INTERVAL, Latest, Sample, SamplerThread},
    sources,
};
use windows_window::Window;

fn main() -> Result<(), Box<dyn Error>> {
    if let Some(config) = sources::handle_command_line("windows-rs-telemetry")? {
        run_window(config)?;
    }
    Ok(())
}

fn run_window(config: Config) -> Result<(), Box<dyn Error>> {
    let sources = sources::select(&config)?;
    let latest: Arc<Latest<Sample>> = Arc::new(Latest::default());
    // Rc/RefCell 留在 UI 线程；信箱只在取放样本时持锁。
    let view = Rc::new(RefCell::new(ui::View {
        history: History::for_sources(&sources),
        ..ui::View::default()
    }));
    let ui_view = Rc::clone(&view);
    let ui_latest = Arc::clone(&latest);
    let (window_width, window_height) = ui::initial_window_size();
    let window = Window::new("系统监控 — CPU / 内存 / 网络 / 电池 / NVMe / 磁盘 / GPU")
        .size(window_width, window_height)
        .on_message(move |hwnd, msg, _wparam, lparam| match msg {
            ui::WM_APP_SAMPLE => {
                if let Some(sample) = ui_latest.take() {
                    ui_view.borrow_mut().history.push(sample);
                    ui::request_redraw(hwnd);
                }
                Some(0)
            }
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
            ui::WM_DPICHANGED => {
                // SAFETY：只有系统的 WM_DPICHANGED 分支才解释 lparam。
                unsafe { ui::apply_dpi_rect(hwnd, lparam) };
                Some(0)
            }
            // 窗口类带 CS_HREDRAW | CS_VREDRAW，尺寸变化会自动整窗重绘。
            ui::WM_ERASEBKGND => Some(1),
            ui::WM_CLOSE => {
                windows_window::quit();
                Some(0)
            }
            _ => None,
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
