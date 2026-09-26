use std::time::Instant;

use crate::chart::{Frame, Renderer};
use windows_rs_telemetry::bindings::{
    self as win, BeginPaint, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, EndPaint, GetClientRect,
    GetDpiForSystem, GetDpiForWindow, HWND, InvalidateRect, IsIconic, PAINTSTRUCT, PostMessageW,
    RECT, SPI_GETWORKAREA, SWP_NOACTIVATE, SWP_NOZORDER, SetProcessDpiAwarenessContext,
    SetWindowPos, SystemParametersInfoW,
};
use windows_rs_telemetry::history::History;

// 元数据把 WM_* 标成 i32，窗口过程收到的是 u32，这里统一转换。
pub const WM_PAINT: u32 = win::WM_PAINT as u32;
pub const WM_ERASEBKGND: u32 = win::WM_ERASEBKGND as u32;
pub const WM_DPICHANGED: u32 = win::WM_DPICHANGED as u32;
pub const WM_CLOSE: u32 = win::WM_CLOSE as u32;
/// 采样线程通知 UI 线程“信箱里有新样本”。
pub const WM_APP_SAMPLE: u32 = win::WM_APP as u32;

/// 初始窗口尺寸，单位 DIP；实际按系统 DPI 换算，并限制在主屏工作区内。
const INITIAL_SIZE: (i32, i32) = (1400, 1480);
const WORK_AREA_MARGIN: i32 = 32;

/// 采样线程通过 PostMessageW 向窗口投递 WM_APP_SAMPLE。
/// HWND 裸指针类型不实现 Send；这里跨线程传递句柄值，投递时再转换为 HWND。
pub struct UiWaker {
    hwnd: usize,
}

impl UiWaker {
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd: hwnd as usize,
        }
    }

    /// 窗口已经销毁时投递失败并返回 false；退出过程中这是正常情况。
    pub fn wake(&self) -> bool {
        // SAFETY：PostMessageW 可以在任意线程调用；hwnd 无效时它只会返回 0。
        unsafe { PostMessageW(self.hwnd as HWND, WM_APP_SAMPLE, 0, 0) != 0 }
    }
}

/// UI 线程独占历史数据；重建 GPU 资源时保留历史数据。
#[derive(Default)]
pub struct View {
    pub history: History,
    pub renderer: Option<Renderer>,
    pub error: Option<windows_result::Error>,
}

impl View {
    /// # Safety
    ///
    /// `hwnd` 必须是当前 UI 线程仍然有效的窗口句柄；在 WM_PAINT 回调中调用。
    pub unsafe fn paint(&mut self, hwnd: HWND) -> windows_canvas::Result<()> {
        // 即使最小化或绘制失败，也必须确认更新区域，避免 WM_PAINT 忙循环。
        let mut paint = PAINTSTRUCT::default();
        // SAFETY：由本窗口 WM_PAINT 回调调用，paint 在配对的 Begin/EndPaint 期间有效。
        unsafe { BeginPaint(hwnd, &mut paint) };
        let result = self.draw_frame(hwnd);
        // SAFETY：与上面的 BeginPaint 配对，paint 未被修改。
        unsafe { EndPaint(hwnd, &paint) };
        result
    }

    fn draw_frame(&mut self, hwnd: HWND) -> windows_canvas::Result<()> {
        let mut rect = RECT::default();
        // SAFETY：WM_PAINT 回调期间 hwnd 有效，并在当前窗口线程使用。
        if unsafe { IsIconic(hwnd) } != 0 {
            return Ok(());
        }
        // SAFETY：同一有效 hwnd；rect 是本地可写输出缓冲区。
        if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
            return Err(windows_result::Error::from_thread());
        }
        let width = (rect.right - rect.left).max(0) as u32;
        let height = (rect.bottom - rect.top).max(0) as u32;
        if width == 0 || height == 0 {
            return Ok(());
        }
        // SAFETY：hwnd 在本次 WM_PAINT 回调内有效。
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let now = Instant::now();
        self.history.prune(now);
        // 设备丢失时最多重建一次。
        for attempt in 0..2 {
            let renderer = match &mut self.renderer {
                Some(renderer) => renderer,
                // SAFETY：WM_CLOSE 被应用拦截，退出时先释放 renderer，再销毁窗口。
                slot => slot.insert(unsafe { Renderer::new(hwnd, width, height, dpi)? }),
            };
            match renderer.paint(width, height, dpi, &self.history, now) {
                Ok(Frame::Presented) => return Ok(()),
                Ok(Frame::DeviceLost) => {}
                Err(e) if windows_canvas::is_device_lost(e.code()) => {}
                Err(e) => return Err(e),
            }
            self.renderer = None;
            if attempt == 0 {
                eprintln!("图形设备丢失，重建 Canvas；历史数据保留");
            }
        }
        Err(windows_canvas::device_lost_error())
    }
}

/// 只标记更新区域，由消息循环合并重绘；最小化时继续采样但不绘图。
pub fn request_redraw(hwnd: HWND) {
    // SAFETY：只查询 hwnd 的窗口状态；句柄失效时 Win32 返回失败。
    if unsafe { IsIconic(hwnd) } == 0 {
        // SAFETY：传入的是窗口句柄值；Win32 校验失效句柄并返回失败。
        unsafe { InvalidateRect(hwnd, std::ptr::null(), 0) };
    }
}

/// # Safety
///
/// 只传入系统 WM_DPICHANGED 的 lparam：它在本次回调内指向建议窗口矩形；
/// `hwnd` 必须是该回调的有效窗口句柄。
pub unsafe fn apply_dpi_rect(hwnd: HWND, lparam: isize) {
    // SAFETY：调用方保证 lparam 来源；先复制矩形，随后 SetWindowPos 可能重入。
    let rect = unsafe { *(lparam as *const RECT) };
    // SAFETY：hwnd 在本次回调内有效；矩形已复制到本地。
    unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            (SWP_NOZORDER | SWP_NOACTIVATE) as u32,
        );
    }
    request_redraw(hwnd);
}

/// 返回外框物理像素尺寸，供 `WindowBuilder::size` 使用。
///
/// windows-window 在 `create()` 里才设置 DPI 感知；在那之前调用系统 API
/// 会拿到按 96 DPI 虚拟化的工作区。这里提前设置同一个感知级别，
/// 之后 windows-window 的重复设置会失败并被它忽略。
pub fn initial_window_size() -> (i32, i32) {
    // SAFETY：只修改本进程的 DPI 感知级别，此时还没有创建任何窗口。
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    // SAFETY：无参数；进程已声明 DPI 感知，返回主屏 DPI。
    let dpi = unsafe { GetDpiForSystem() }.max(96) as i32;
    let (width, height) = (INITIAL_SIZE.0 * dpi / 96, INITIAL_SIZE.1 * dpi / 96);
    let mut work = RECT::default();
    // SAFETY：SPI_GETWORKAREA 向 work 写入一个 RECT。
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA as u32, 0, (&raw mut work).cast(), 0) };
    if ok == 0 {
        return (width, height);
    }
    let fit = |size: i32, available: i32| size.min((available - WORK_AREA_MARGIN).max(1));
    (
        fit(width, work.right - work.left),
        fit(height, work.bottom - work.top),
    )
}
