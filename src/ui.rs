use std::ffi::c_void;
use std::time::Instant;

use crate::chart::Renderer;
pub use windows_rs_telemetry::bindings::{
    WM_CLOSE as CLOSE, WM_DPICHANGED as DPI_CHANGED, WM_ERASEBKGND, WM_PAINT, WM_SIZE,
};
use windows_rs_telemetry::history::History;

use windows_rs_telemetry::bindings::{HWND, PostMessageW, WM_APP};

/// 自定义消息：“有新样本了”。WM_APP 及以上的编号留给应用自己用。
/// 元数据把 WM_* 常量标成 i32，而消息参数是 u32，所以这里转换一次。
pub const WM_APP_SAMPLE: u32 = WM_APP as u32;

/// 采样线程用来提醒 UI 线程的“门铃”。
///
/// HWND 在 Rust 里是裸指针，裸指针一律不是 Send，所以不能直接带进采样线程。
/// HWND 是不透明的系统句柄，不把它当作内存指针解引用；
/// PostMessageW 明确允许从任何线程调用，它只是把消息放进目标窗口线程的队列。
/// 因此这里保存编号本身（usize 天然是 Send），并且只开放 PostMessageW 这一个操作。
/// 窗口与 Canvas 仍然只在 UI 线程使用，关闭时先停止发送者，再销毁窗口。
pub struct UiWaker {
    hwnd: usize,
}

impl UiWaker {
    pub fn new(hwnd: *mut c_void) -> Self {
        Self {
            hwnd: hwnd as usize,
        }
    }

    /// 投递一条 WM_APP_SAMPLE，立即返回，不等 UI 处理。
    /// 窗口已经销毁时投递会失败，返回 false；退出过程中出现这种情况是正常的。
    pub fn wake(&self) -> bool {
        // SAFETY：PostMessageW 可以在任意线程调用；hwnd 无效时它只会返回 0。
        unsafe { PostMessageW(self.hwnd as HWND, WM_APP_SAMPLE, 0, 0) != 0 }
    }
}

/// UI 线程独占；历史保存在 CPU 内存里，重建 GPU 资源不会丢数据。
pub struct View {
    pub history: History,
    pub renderer: Option<Renderer>,
    pub error: Option<windows_result::Error>,
}

impl View {
    pub fn new() -> Self {
        Self {
            history: History::default(),
            renderer: None,
            error: None,
        }
    }

    /// # Safety
    ///
    /// `hwnd` 必须是当前 UI 线程仍然有效的窗口句柄；在 WM_PAINT 回调中调用。
    pub unsafe fn paint(&mut self, hwnd: HWND) -> windows_canvas::Result<()> {
        use windows_rs_telemetry::bindings::*;
        // 即使最小化或绘制失败，也必须确认更新区域，避免 WM_PAINT 忙循环。
        let mut paint = PAINTSTRUCT::default();
        // SAFETY：由本窗口 WM_PAINT 回调调用，paint 在配对的 Begin/EndPaint 期间有效。
        unsafe {
            BeginPaint(hwnd, &mut paint);
        }
        let result = self.draw_frame(hwnd);
        unsafe {
            EndPaint(hwnd, &paint);
        }
        result
    }

    fn draw_frame(&mut self, hwnd: HWND) -> windows_canvas::Result<()> {
        use windows_rs_telemetry::bindings::*;
        let mut rect = RECT::default();
        // SAFETY：窗口线程读取自己的有效 HWND，rect 是本地有效输出缓冲区。
        unsafe {
            if IsIconic(hwnd) != 0 {
                return Ok(());
            }
            if GetClientRect(hwnd, &mut rect) == 0 {
                return Err(windows_result::Error::from_thread());
            }
        }
        let width = (rect.right - rect.left).max(0) as u32;
        let height = (rect.bottom - rect.top).max(0) as u32;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let now = Instant::now();
        self.history.prune(now);
        // 最多重建一次，不在设备反复丢失时无限循环。
        for attempt in 0..2 {
            if self.renderer.is_none() {
                // SAFETY：WM_CLOSE 被应用拦截，退出时先释放 renderer，再销毁窗口。
                self.renderer = Some(unsafe { Renderer::new(hwnd, width, height, dpi)? });
            }
            let result =
                self.renderer
                    .as_mut()
                    .unwrap()
                    .paint(width, height, dpi, &self.history, now);
            match result {
                Ok(true) => {
                    return Ok(());
                }
                Ok(false) => {}
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

// HWND 是传给 Win32 验证的不透明句柄；此函数不解引用它，失效时 API 返回失败。
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn request_redraw(hwnd: HWND) {
    // 只标记更新区域，由消息循环合并重绘；最小化时继续采样但不绘图。
    unsafe {
        if windows_rs_telemetry::bindings::IsIconic(hwnd) == 0 {
            windows_rs_telemetry::bindings::InvalidateRect(hwnd, std::ptr::null(), 0);
        }
    }
}

/// # Safety
///
/// 只传入系统 WM_DPICHANGED 的 lparam：它在本次回调内指向建议窗口矩形；
/// `hwnd` 必须是该回调的有效窗口句柄。
pub unsafe fn apply_dpi_rect(hwnd: HWND, lparam: isize) {
    use windows_rs_telemetry::bindings::*;
    // SAFETY：调用方保证 lparam 来源；先复制矩形，随后 SetWindowPos 可能重入。
    let rect = unsafe { *(lparam as *const RECT) };
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
