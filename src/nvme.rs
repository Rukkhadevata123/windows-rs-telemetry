//! 只读 NVMe SMART 查询。设备描述符用于运行时选盘，不保存序列号。
use std::{ffi::c_void, io, mem::size_of, ptr, time::Duration};

use crate::bindings as n;

pub const INTERVAL: Duration = Duration::from_secs(30);
pub const STALE_AFTER: Duration = Duration::from_secs(75);
const SMART_LOG_BYTES: usize = 512;
const BUFFER_BYTES: usize = 1024;
const DEVICE_BUFFER_BYTES: usize = 4096;
const SMART_LOG_PAGE: u32 = 2;
const MAX_DRIVE_INDEX: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskDevice {
    pub path: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy)]
pub struct SmartHealth {
    pub critical_warning: u8,
    pub temperature_c: f64,
    pub available_spare_percent: u8,
    pub spare_threshold_percent: u8,
    pub percentage_used: u8,
}

impl SmartHealth {
    pub fn labels(&self) -> (String, String) {
        (
            format!(
                "温度 {:.1}°C  ·  备用空间 {}%",
                self.temperature_c, self.available_spare_percent
            ),
            format!(
                "已用寿命估计 {}%  ·  备用阈值 {}%  ·  警告 0x{:02X}",
                self.percentage_used, self.spare_threshold_percent, self.critical_warning
            ),
        )
    }
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY：唯一拥有 CreateFileW 成功返回的句柄；释放一次。
        unsafe { n::CloseHandle(self.0) };
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn open(path: &str) -> io::Result<Handle> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    // 零 desired access、共享读写，只用于设备属性查询。
    // SAFETY：路径以 NUL 结尾且在调用期间有效；其余可选指针允许为空。
    let raw = unsafe { n::CreateFileW(wide.as_ptr(), 0, 3, ptr::null(), 3, 0, ptr::null_mut()) };
    if raw as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(Handle(raw))
}

fn read_u32(bytes: &[u8], at: usize) -> io::Result<u32> {
    let raw: [u8; 4] = bytes
        .get(at..at + 4)
        .ok_or_else(|| invalid("truncated device descriptor"))?
        .try_into()
        .expect("four byte slice");
    Ok(u32::from_le_bytes(raw))
}

fn descriptor_string(bytes: &[u8], at: u32) -> io::Result<String> {
    if at == 0 {
        return Ok(String::new());
    }
    let rest = bytes
        .get(at as usize..)
        .ok_or_else(|| invalid("device string offset outside descriptor"))?;
    let end = rest
        .iter()
        .position(|&byte| byte == 0)
        .ok_or_else(|| invalid("unterminated device string"))?;
    Ok(String::from_utf8_lossy(&rest[..end]).trim().to_owned())
}

fn parse_device(bytes: &[u8]) -> io::Result<Option<String>> {
    let minimum = std::mem::offset_of!(n::STORAGE_DEVICE_DESCRIPTOR, RawDeviceProperties);
    if bytes.len() < minimum {
        return Err(invalid("device descriptor shorter than fixed fields"));
    }
    let version = read_u32(bytes, 0)? as usize;
    let size = read_u32(bytes, 4)? as usize;
    if version < minimum || size < minimum || size > bytes.len() {
        return Err(invalid("invalid device descriptor version or size"));
    }
    let bus = read_u32(
        bytes,
        std::mem::offset_of!(n::STORAGE_DEVICE_DESCRIPTOR, BusType),
    )?;
    if bus != n::BusTypeNvme as u32 {
        return Ok(None);
    }
    // 不把生成结构里的 BOOLEAN 直接读成 Rust bool；只读取需要的整数偏移。
    let vendor = read_u32(
        bytes,
        std::mem::offset_of!(n::STORAGE_DEVICE_DESCRIPTOR, VendorIdOffset),
    )?;
    let product = read_u32(
        bytes,
        std::mem::offset_of!(n::STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset),
    )?;
    let bytes = &bytes[..size];
    let name = format!(
        "{} {}",
        descriptor_string(bytes, vendor)?,
        descriptor_string(bytes, product)?
    )
    .trim()
    .to_owned();
    Ok(Some(if name.is_empty() {
        "未命名 NVMe".into()
    } else {
        name
    }))
}

fn query_device(handle: &Handle) -> io::Result<Option<String>> {
    let query = n::STORAGE_PROPERTY_QUERY {
        PropertyId: n::StorageDeviceProperty,
        QueryType: n::PropertyStandardQuery,
        ..Default::default()
    };
    let mut storage = vec![0_u64; DEVICE_BUFFER_BYTES / size_of::<u64>()];
    let mut returned = 0_u32;
    // SAFETY：句柄有效；输入结构完整，输出缓冲区可写且大小匹配。
    let ok = unsafe {
        n::DeviceIoControl(
            handle.0,
            n::IOCTL_STORAGE_QUERY_PROPERTY as u32,
            (&query as *const n::STORAGE_PROPERTY_QUERY).cast(),
            size_of::<n::STORAGE_PROPERTY_QUERY>() as u32,
            storage.as_mut_ptr().cast(),
            DEVICE_BUFFER_BYTES as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if returned as usize > DEVICE_BUFFER_BYTES {
        return Err(invalid("device returned more bytes than output buffer"));
    }
    // SAFETY：前 returned 字节由成功的同步 DeviceIoControl 初始化。
    let bytes = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast(), returned as usize) };
    parse_device(bytes)
}

pub fn list_disks() -> io::Result<Vec<DiskDevice>> {
    let mut disks = Vec::new();
    let mut first_error = None;
    for index in 0..MAX_DRIVE_INDEX {
        let path = format!(r"\\.\PhysicalDrive{index}");
        match open(&path) {
            Ok(handle) => match query_device(&handle) {
                Ok(Some(name)) => disks.push(DiskDevice { path, name }),
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            },
            Err(error) if matches!(error.raw_os_error(), Some(2 | 3)) => {}
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(disks)
    }
}

pub fn select_disk(requested_path: Option<&str>) -> io::Result<Option<DiskDevice>> {
    if let Some(path) = requested_path {
        let lower = path.to_ascii_lowercase();
        let suffix = lower.strip_prefix(r"\\.\physicaldrive").ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "需要 PhysicalDrive 路径")
        })?;
        if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "需要 PhysicalDrive 路径",
            ));
        }
        let handle = open(path)?;
        let name = query_device(&handle)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "指定路径不是 NVMe 盘"))?;
        return Ok(Some(DiskDevice {
            path: path.to_owned(),
            name,
        }));
    }
    let disks = list_disks()?;
    Ok(if disks.len() == 1 {
        disks.into_iter().next()
    } else {
        None
    })
}

fn parse_smart(bytes: &[u8]) -> io::Result<SmartHealth> {
    let descriptor_size = size_of::<n::STORAGE_PROTOCOL_DATA_DESCRIPTOR>();
    if bytes.len() < descriptor_size {
        return Err(invalid("response shorter than protocol descriptor"));
    }
    // SAFETY：长度足够；字节切片不保证对齐，所以用 read_unaligned。
    let descriptor = unsafe {
        ptr::read_unaligned(bytes.as_ptr().cast::<n::STORAGE_PROTOCOL_DATA_DESCRIPTOR>())
    };
    if descriptor.Version as usize != descriptor_size || descriptor.Size as usize != descriptor_size
    {
        return Err(invalid("unexpected protocol descriptor version or size"));
    }
    let protocol = descriptor.ProtocolSpecificData;
    if protocol.ProtocolType != n::ProtocolTypeNvme
        || protocol.DataType != n::NVMeDataTypeLogPage as u32
    {
        return Err(invalid("response is not an NVMe log page"));
    }
    let protocol_start =
        std::mem::offset_of!(n::STORAGE_PROTOCOL_DATA_DESCRIPTOR, ProtocolSpecificData);
    let offset = protocol.ProtocolDataOffset as usize;
    if offset < size_of::<n::STORAGE_PROTOCOL_SPECIFIC_DATA>() {
        return Err(invalid("protocol data overlaps descriptor"));
    }
    let start = protocol_start
        .checked_add(offset)
        .ok_or_else(|| invalid("protocol data offset overflow"))?;
    let end = start
        .checked_add(protocol.ProtocolDataLength as usize)
        .ok_or_else(|| invalid("protocol data length overflow"))?;
    if (protocol.ProtocolDataLength as usize) < SMART_LOG_BYTES || end > bytes.len() {
        return Err(invalid("SMART log missing or truncated"));
    }
    let log = &bytes[start..start + SMART_LOG_BYTES];
    let kelvin = u16::from_le_bytes([log[1], log[2]]);
    if kelvin == 0 {
        return Err(invalid("SMART composite temperature is not reported"));
    }
    Ok(SmartHealth {
        critical_warning: log[0],
        temperature_c: f64::from(kelvin) - 273.15,
        available_spare_percent: log[3],
        spare_threshold_percent: log[4],
        percentage_used: log[5],
    })
}

pub fn collect_health(device: &DiskDevice) -> io::Result<SmartHealth> {
    let handle = open(&device.path)?;
    let mut storage = vec![0_u64; BUFFER_BYTES / size_of::<u64>()];
    let buffer = storage.as_mut_ptr().cast::<u8>();
    let query = buffer.cast::<n::STORAGE_PROPERTY_QUERY>();
    // SAFETY：缓冲区对齐且大于两个结构体；只请求 NVMe SMART 日志页。
    unsafe {
        (*query).PropertyId = n::StorageDeviceProtocolSpecificProperty;
        (*query).QueryType = n::PropertyStandardQuery;
        let specific = buffer
            .add(std::mem::offset_of!(
                n::STORAGE_PROPERTY_QUERY,
                AdditionalParameters
            ))
            .cast::<n::STORAGE_PROTOCOL_SPECIFIC_DATA>();
        *specific = n::STORAGE_PROTOCOL_SPECIFIC_DATA {
            ProtocolType: n::ProtocolTypeNvme,
            DataType: n::NVMeDataTypeLogPage as u32,
            ProtocolDataRequestValue: SMART_LOG_PAGE,
            ProtocolDataOffset: size_of::<n::STORAGE_PROTOCOL_SPECIFIC_DATA>() as u32,
            ProtocolDataLength: SMART_LOG_BYTES as u32,
            ..Default::default()
        };
    }
    let mut returned = 0_u32;
    // SAFETY：输入/输出均指向完整的可写缓冲区，调用同步完成。
    let ok = unsafe {
        n::DeviceIoControl(
            handle.0,
            n::IOCTL_STORAGE_QUERY_PROPERTY as u32,
            buffer.cast(),
            BUFFER_BYTES as u32,
            buffer.cast(),
            BUFFER_BYTES as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if returned as usize > BUFFER_BYTES {
        return Err(invalid("driver reported more bytes than output buffer"));
    }
    // SAFETY：成功调用初始化 returned 字节。
    let bytes = unsafe { std::slice::from_raw_parts(buffer, returned as usize) };
    parse_smart(bytes)
}

#[cfg(test)]
pub(crate) fn demo_health() -> SmartHealth {
    SmartHealth {
        critical_warning: 0,
        temperature_c: 39.85,
        available_spare_percent: 100,
        spare_threshold_percent: 5,
        percentage_used: 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_parser_checks_returned_length_and_offsets() {
        let mut bytes = vec![0_u8; BUFFER_BYTES];
        let descriptor = n::STORAGE_PROTOCOL_DATA_DESCRIPTOR {
            Version: size_of::<n::STORAGE_PROTOCOL_DATA_DESCRIPTOR>() as u32,
            Size: size_of::<n::STORAGE_PROTOCOL_DATA_DESCRIPTOR>() as u32,
            ProtocolSpecificData: n::STORAGE_PROTOCOL_SPECIFIC_DATA {
                ProtocolType: n::ProtocolTypeNvme,
                DataType: n::NVMeDataTypeLogPage as u32,
                ProtocolDataOffset: size_of::<n::STORAGE_PROTOCOL_SPECIFIC_DATA>() as u32,
                ProtocolDataLength: SMART_LOG_BYTES as u32,
                ..Default::default()
            },
        };
        // SAFETY：目标切片长度足够；写入时不假定 u8 向量的对齐。
        unsafe {
            ptr::write_unaligned(
                bytes
                    .as_mut_ptr()
                    .cast::<n::STORAGE_PROTOCOL_DATA_DESCRIPTOR>(),
                descriptor,
            );
        }
        let start = std::mem::offset_of!(n::STORAGE_PROTOCOL_DATA_DESCRIPTOR, ProtocolSpecificData)
            + size_of::<n::STORAGE_PROTOCOL_SPECIFIC_DATA>();
        bytes[start + 1..start + 3].copy_from_slice(&310_u16.to_le_bytes());
        bytes[start + 3] = 100;
        bytes[start + 4] = 5;
        bytes[start + 5] = 2;
        assert!((parse_smart(&bytes).unwrap().temperature_c - 36.85).abs() < 0.001);
        assert!(parse_smart(&bytes[..100]).is_err());
        let mut broken = descriptor;
        broken.ProtocolSpecificData.ProtocolDataOffset = 900;
        unsafe {
            ptr::write_unaligned(
                bytes
                    .as_mut_ptr()
                    .cast::<n::STORAGE_PROTOCOL_DATA_DESCRIPTOR>(),
                broken,
            );
        }
        assert!(parse_smart(&bytes).is_err());
    }

    #[test]
    fn device_parser_uses_bus_type_and_bounded_model_string() {
        let mut bytes = vec![0_u8; 96];
        let length = bytes.len() as u32;
        bytes[0..4]
            .copy_from_slice(&(size_of::<n::STORAGE_DEVICE_DESCRIPTOR>() as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        let bus = std::mem::offset_of!(n::STORAGE_DEVICE_DESCRIPTOR, BusType);
        bytes[bus..bus + 4].copy_from_slice(&(n::BusTypeNvme as u32).to_le_bytes());
        let product = std::mem::offset_of!(n::STORAGE_DEVICE_DESCRIPTOR, ProductIdOffset);
        bytes[product..product + 4].copy_from_slice(&48_u32.to_le_bytes());
        bytes[48..54].copy_from_slice(b"TEST\0\0");
        assert_eq!(parse_device(&bytes).unwrap().as_deref(), Some("TEST"));
        bytes[product..product + 4].copy_from_slice(&200_u32.to_le_bytes());
        assert!(parse_device(&bytes).is_err());
        bytes[bus..bus + 4].copy_from_slice(&0_u32.to_le_bytes());
        assert!(parse_device(&bytes).unwrap().is_none());
    }
}
