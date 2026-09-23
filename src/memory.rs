use crate::bindings::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

/// 对外暴露的 Rust 类型：不泄漏任何 Win32 结构体。
#[derive(Debug, Clone, Copy)]
pub struct MemorySnapshot {
    /// Windows 可见的物理内存总量（字节），比 SMBIOS 记录的安装容量小
    pub total_bytes: u64,
    /// 当前可用量（字节），包括备用列表中可立即复用的页
    pub available_bytes: u64,
}

impl MemorySnapshot {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.available_bytes)
    }
}

pub fn collect_memory() -> std::io::Result<MemorySnapshot> {
    // 输入输出结构体：调用方必须先填 dwLength，函数靠它识别结构版本/大小。
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };

    // SAFETY：&mut status 转换成的指针只在本次调用期间使用，并指向一个
    // dwLength 已正确初始化的有效 MEMORYSTATUSEX。
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };

    // 返回值是 BOOL（i32）：非零表示成功。只有失败时才读取线程的“最后错误”，
    // 成功时它的值没有意义。这不是 HRESULT，也不是直接返回的错误码。
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(MemorySnapshot {
        total_bytes: status.ullTotalPhys,
        available_bytes: status.ullAvailPhys,
    })
}
