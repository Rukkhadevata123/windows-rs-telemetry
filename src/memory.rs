use crate::bindings::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

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
    // 调用方必须先填 dwLength，函数靠它识别结构版本。
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };

    // SAFETY：指向 dwLength 已初始化的本地 MEMORYSTATUSEX，只在本次调用期间使用。
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };

    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(MemorySnapshot {
        total_bytes: status.ullTotalPhys,
        available_bytes: status.ullAvailPhys,
    })
}
