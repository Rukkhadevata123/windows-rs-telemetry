mod chart;
mod ui;

use std::{
    cell::RefCell,
    error::Error,
    io,
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use windows_rs_telemetry::{
    config::{self, Command, Config},
    network, nvme,
    sampling::{Sample, SamplerThread},
    sources,
};
use windows_window::Window;

fn main() -> Result<(), Box<dyn Error>> {
    let command = Command::parse(std::env::args().skip(1)).map_err(io::Error::other)?;
    match command {
        Command::Help => println!("{}", config::help("windows-rs-telemetry")),
        Command::ListNetwork => network::print_interfaces(None)?,
        Command::ListDisks => {
            for disk in nvme::list_disks() {
                println!("{}  {}", disk.path, disk.name);
            }
        }
        Command::Run(config) => run_window(config)?,
    }
    Ok(())
}

fn run_window(config: Config) -> Result<(), Box<dyn Error>> {
    let sources = sources::select(&config)?;
    let latest: Arc<Mutex<Option<Sample>>> = Arc::new(Mutex::new(None));
    // Rc/RefCell 留在 UI 线程；信箱只在取放样本时持锁。
    let view = Rc::new(RefCell::new(ui::View::new()));
    {
        let mut state = view.borrow_mut();
        state.history.network_name = sources.network_name;
        state.history.nvme_name = sources.disk_name;
    }
    let ui_view = Rc::clone(&view);
    let ui_latest = Arc::clone(&latest);
    let window = Window::new("windows-rs telemetry — CPU / RAM / WLAN / Battery / NVMe / PDH")
        .size(1400, 1480)
        .on_message(move |hwnd, msg, _wparam, lparam| {
            if msg == ui::WM_APP_SAMPLE {
                if let Some(sample) = ui_latest.lock().unwrap().take() {
                    ui_view.borrow_mut().history.push(sample);
                    ui::request_redraw(hwnd);
                }
                return Some(0);
            }
            match msg as i32 {
                ui::WM_PAINT => {
                    // SAFETY：WM_PAINT 回调提供当前 UI 线程仍然有效的 HWND。
                    if let Err(error) = unsafe { ui_view.borrow_mut().paint(hwnd) } {
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
        Duration::from_secs(1),
        sources.network_luid,
        sources.disk,
        move |sample| {
            let was_empty = latest.lock().unwrap().replace(sample).is_none();
            if was_empty && !waker.wake() {
                // 投递失败时清空信箱，下一轮仍可重试。
                latest.lock().unwrap().take();
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
