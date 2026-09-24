//! PDH CPU 频率估算、磁盘速率与 GPU Engine 实例。查询对象只在创建它的线程中使用。
use std::{
    ptr,
    time::{Duration, Instant},
};

use crate::bindings::{dxgi, pdh as native};
use crate::sampling::MAX_SAMPLE_GAP;

const READ_TOTAL: &str = r"\PhysicalDisk(_Total)\Disk Read Bytes/sec";
const WRITE_TOTAL: &str = r"\PhysicalDisk(_Total)\Disk Write Bytes/sec";
const GPU_ENGINES: &str = r"\GPU Engine(*)\Utilization Percentage";
const CPU_BASE_MHZ: &str = r"\Processor Information(_Total)\Processor Frequency";
const CPU_PERFORMANCE: &str = r"\Processor Information(_Total)\% Processor Performance";
const MIN_BASELINE_AGE: Duration = Duration::from_millis(500);
pub const RETRY_INTERVAL: Duration = Duration::from_secs(30);
// 这些状态码来自 Windows SDK 的 PdhMsg.h / winerror.h / dxgi.h。
const PDH_MORE_DATA: u32 = 0x8000_07D2;
const PDH_CSTATUS_VALID_DATA: u32 = 0;
const PDH_CSTATUS_NEW_DATA: u32 = 1;
const DXGI_ERROR_NOT_FOUND: u32 = 0x887A_0002;
const DXGI_ADAPTER_FLAG_SOFTWARE: u32 = 2;
/// EnumAdapters1 正常以 NOT_FOUND 结束；这个上限只防止驱动异常时无限枚举。
const MAX_ADAPTERS: u32 = 32;

#[derive(Debug, Clone, Copy)]
pub struct DiskRates {
    pub read_bps: f64,
    pub write_bps: f64,
}

#[derive(Debug, Clone)]
pub struct EngineReading {
    pub adapter: String,
    pub pid: u32,
    pub engine_type: String,
    pub percent: f64,
}

#[derive(Debug)]
pub struct Snapshot {
    pub disk: Result<DiskRates, String>,
    /// None 表示已匹配适配器但当前没有活动实例。
    pub gpu: Result<Option<EngineReading>, String>,
}

struct Query(native::PDH_HQUERY);

impl Drop for Query {
    fn drop(&mut self) {
        // SAFETY: 查询句柄由 PdhOpenQueryW 创建，关闭它也关闭其中的计数器。
        unsafe { native::PdhCloseQuery(self.0) };
    }
}

fn status(operation: &str, code: native::PDH_STATUS) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("{operation}: PDH 0x{:08X}", code as u32))
    }
}

fn open_query() -> Result<Query, String> {
    let mut handle = ptr::null_mut();
    // SAFETY: 空数据源代表本机，输出参数可写。
    status("PdhOpenQueryW", unsafe {
        native::PdhOpenQueryW(ptr::null(), 0, &mut handle)
    })?;
    Ok(Query(handle))
}

fn add_counter(query: &Query, path: &str) -> Result<native::PDH_HCOUNTER, String> {
    let mut handle = ptr::null_mut();
    let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    // SAFETY: 查询句柄有效，路径在调用期间保持有效且以 NUL 结尾。
    status("PdhAddEnglishCounterW", unsafe {
        native::PdhAddEnglishCounterW(query.0, path.as_ptr(), 0, &mut handle)
    })?;
    Ok(handle)
}

fn collect(query: &Query) -> Result<(), String> {
    // SAFETY: 查询句柄及其中的计数器均有效。
    status("PdhCollectQueryData", unsafe {
        native::PdhCollectQueryData(query.0)
    })
}

fn formatted_value(counter: native::PDH_HCOUNTER) -> Result<f64, String> {
    let mut value = native::PDH_FMT_COUNTERVALUE::default();
    // SAFETY: 输出缓冲区完整有效，counter 属于仍然开启的查询。
    status("PdhGetFormattedCounterValue", unsafe {
        native::PdhGetFormattedCounterValue(
            counter,
            native::PDH_FMT_DOUBLE,
            ptr::null_mut(),
            &mut value,
        )
    })?;
    if value.CStatus != PDH_CSTATUS_VALID_DATA && value.CStatus != PDH_CSTATUS_NEW_DATA {
        return Err(format!("PDH CStatus=0x{:08X}", value.CStatus));
    }
    // SAFETY: 已要求 double 格式，且 CStatus 有效。
    let result = unsafe { value.Anonymous.doubleValue };
    if result.is_finite() && result >= 0.0 {
        Ok(result)
    } else {
        Err(format!("无效计数器值：{result}"))
    }
}

/// 速率类计数器需要两次 collect；距基线太近或间隔过长时都不读取。
struct Baseline {
    started: Instant,
    last_poll: Instant,
}

enum Readiness {
    /// 间隔过长，调用方应 collect 一次作为新基线。
    Rebase,
    Warming,
    Ready,
}

impl Baseline {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            last_poll: now,
        }
    }

    fn check(&mut self, now: Instant) -> Readiness {
        if now.saturating_duration_since(self.last_poll) > MAX_SAMPLE_GAP {
            // 睡眠恢复或慢采样后只重建基线，不展示跨长间隔的平均值。
            *self = Self {
                started: now,
                last_poll: now,
            };
            Readiness::Rebase
        } else if now.saturating_duration_since(self.started) < MIN_BASELINE_AGE {
            Readiness::Warming
        } else {
            self.last_poll = now;
            Readiness::Ready
        }
    }
}

/// 用 Windows 报告的基准 MHz 和动态性能百分比估算整机 CPU 频率。
/// _Total 汇总所有逻辑处理器。
pub struct CpuFrequencySampler {
    query: Query,
    base: native::PDH_HCOUNTER,
    performance: native::PDH_HCOUNTER,
    baseline: Baseline,
}

impl CpuFrequencySampler {
    pub fn new() -> Result<Self, String> {
        let query = open_query()?;
        let base = add_counter(&query, CPU_BASE_MHZ)?;
        let performance = add_counter(&query, CPU_PERFORMANCE)?;
        collect(&query)?;
        Ok(Self {
            query,
            base,
            performance,
            baseline: Baseline::new(),
        })
    }

    /// 结果以 MHz 为单位；基线未就绪时返回 None。
    pub fn poll(&mut self) -> Result<Option<f64>, String> {
        match self.baseline.check(Instant::now()) {
            Readiness::Rebase => return collect(&self.query).map(|()| None),
            Readiness::Warming => return Ok(None),
            Readiness::Ready => {}
        }
        collect(&self.query)?;
        let base_mhz = formatted_value(self.base)?;
        let performance_percent = formatted_value(self.performance)?;
        if base_mhz <= 0.0 || performance_percent <= 0.0 {
            return Ok(None);
        }
        let estimated_mhz = base_mhz * performance_percent / 100.0;
        if estimated_mhz.is_finite() {
            Ok(Some(estimated_mhz))
        } else {
            Err("CPU 估算频率不是有限数".into())
        }
    }
}

struct DiskQuery {
    query: Query,
    read: native::PDH_HCOUNTER,
    write: native::PDH_HCOUNTER,
}

impl DiskQuery {
    fn new() -> Result<Self, String> {
        let query = open_query()?;
        let read = add_counter(&query, READ_TOTAL)?;
        let write = add_counter(&query, WRITE_TOTAL)?;
        collect(&query)?; // 两个速率计数器先建立基线。
        Ok(Self { query, read, write })
    }

    fn poll(&self) -> Result<DiskRates, String> {
        collect(&self.query)?;
        Ok(DiskRates {
            read_bps: formatted_value(self.read)?,
            write_bps: formatted_value(self.write)?,
        })
    }
}

#[derive(Clone)]
struct Adapter {
    name: String,
    marker: String,
}

fn adapters() -> Result<Vec<Adapter>, String> {
    // SAFETY: DXGI 返回由 COM 引用计数管理的工厂；接口均仅在本线程使用。
    let factory: dxgi::IDXGIFactory1 =
        unsafe { dxgi::CreateDXGIFactory1() }.map_err(|e| format!("创建 GPU 设备列表失败：{e}"))?;
    let mut result = Vec::new();
    for index in 0..MAX_ADAPTERS {
        // SAFETY: 工厂仍有效；仅在本线程枚举。
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code().0 as u32 == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(format!("枚举 GPU 适配器失败：{error}")),
        };
        let mut desc = dxgi::DXGI_ADAPTER_DESC1::default();
        // SAFETY: 完整的本地结构体输出。
        unsafe { adapter.GetDesc1(&mut desc) }
            .ok()
            .map_err(|e| format!("读取 GPU 适配器信息失败：{e}"))?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE != 0 {
            continue;
        }
        let end = desc
            .Description
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(desc.Description.len());
        result.push(Adapter {
            name: String::from_utf16_lossy(&desc.Description[..end]),
            marker: format!(
                "_luid_0x{:08x}_0x{:08x}_",
                desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart
            ),
        });
    }
    if result.is_empty() {
        Err("DXGI 没有枚举到物理 GPU 适配器".into())
    } else {
        Ok(result)
    }
}

fn name_from_buffer(
    buffer: &[u64],
    used_bytes: usize,
    pointer: *const u16,
) -> Result<String, String> {
    let base = buffer.as_ptr() as usize;
    let end = base.checked_add(used_bytes).ok_or("PDH 缓冲区地址溢出")?;
    let at = pointer as usize;
    if at < base || at >= end || !at.is_multiple_of(size_of::<u16>()) {
        return Err("PDH 实例名指针越界或未对齐".into());
    }
    let units = (end - at) / size_of::<u16>();
    // SAFETY: 指针、对齐和最大长度都限制在 PDH 返回的缓冲区内。
    let slice = unsafe { std::slice::from_raw_parts(pointer, units) };
    let len = slice
        .iter()
        .position(|&unit| unit == 0)
        .ok_or("PDH 实例名没有终止符")?;
    Ok(String::from_utf16_lossy(&slice[..len]))
}

fn formatted_array(counter: native::PDH_HCOUNTER) -> Result<Vec<(String, f64)>, String> {
    for _ in 0..3 {
        let (mut bytes, mut count) = (0_u32, 0_u32);
        // SAFETY: 空输出仅查询需要的缓冲区大小。
        let first = unsafe {
            native::PdhGetFormattedCounterArrayW(
                counter,
                native::PDH_FMT_DOUBLE,
                &mut bytes,
                &mut count,
                ptr::null_mut(),
            )
        };
        if first == 0 && bytes == 0 {
            return Ok(Vec::new());
        }
        if first as u32 != PDH_MORE_DATA || bytes == 0 {
            return Err(format!(
                "PDH 实例缓冲区大小查询失败：0x{:08X}",
                first as u32
            ));
        }
        let mut buffer = vec![0_u64; (bytes as usize).div_ceil(size_of::<u64>())];
        let mut available = (buffer.len() * size_of::<u64>()) as u32;
        // SAFETY: u64 缓冲区具有足够对齐，available 是可写容量。
        let code = unsafe {
            native::PdhGetFormattedCounterArrayW(
                counter,
                native::PDH_FMT_DOUBLE,
                &mut available,
                &mut count,
                buffer.as_mut_ptr().cast(),
            )
        };
        if code as u32 == PDH_MORE_DATA {
            continue;
        }
        if code != 0 {
            return Err(format!(
                "PdhGetFormattedCounterArrayW: 0x{:08X}",
                code as u32
            ));
        }
        let item_bytes = (count as usize)
            .checked_mul(size_of::<native::PDH_FMT_COUNTERVALUE_ITEM_W>())
            .ok_or("PDH 实例数溢出")?;
        if available as usize > buffer.len() * size_of::<u64>() || item_bytes > available as usize {
            return Err("PDH 实例数组超出输出缓冲区".into());
        }
        // SAFETY: count 个对齐的完整条目都在返回缓冲区内。
        let items = unsafe {
            std::slice::from_raw_parts(
                buffer
                    .as_ptr()
                    .cast::<native::PDH_FMT_COUNTERVALUE_ITEM_W>(),
                count as usize,
            )
        };
        let mut result = Vec::with_capacity(items.len());
        for item in items {
            if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA
                && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA
            {
                continue;
            }
            // SAFETY: 已要求 double 格式且 CStatus 有效。
            let value = unsafe { item.FmtValue.Anonymous.doubleValue };
            if value.is_finite() && (0.0..=100.0).contains(&value) {
                result.push((
                    name_from_buffer(&buffer, available as usize, item.szName)?,
                    value,
                ));
            }
        }
        return Ok(result);
    }
    Err("GPU Engine 实例数持续变化，缓冲区三次重试仍不足".into())
}

fn parse_instance(name: &str) -> Option<(u32, &str)> {
    let pid = name.strip_prefix("pid_")?.split('_').next()?.parse().ok()?;
    let engine_type = name.split_once("_engtype_")?.1;
    if engine_type.is_empty() {
        None
    } else {
        Some((pid, engine_type))
    }
}

struct GpuQuery {
    query: Query,
    counter: native::PDH_HCOUNTER,
    adapters: Vec<Adapter>,
}

impl GpuQuery {
    fn new() -> Result<Self, String> {
        let adapters = adapters()?;
        let query = open_query()?;
        let counter = add_counter(&query, GPU_ENGINES)?;
        collect(&query)?;
        Ok(Self {
            query,
            counter,
            adapters,
        })
    }

    fn poll(&self) -> Result<Option<EngineReading>, String> {
        collect(&self.query)?;
        let values = formatted_array(self.counter)?;
        let has_instances = !values.is_empty();
        let mut matched = 0;
        let mut busiest: Option<EngineReading> = None;
        for (name, percent) in values {
            let lower = name.to_ascii_lowercase();
            let Some(adapter) = self.adapters.iter().find(|a| lower.contains(&a.marker)) else {
                continue;
            };
            let Some((pid, engine_type)) = parse_instance(&name) else {
                continue;
            };
            matched += 1;
            if percent > 0.0
                && busiest
                    .as_ref()
                    .is_none_or(|current| percent > current.percent)
            {
                busiest = Some(EngineReading {
                    adapter: adapter.name.clone(),
                    pid,
                    engine_type: engine_type.to_owned(),
                    percent,
                });
            }
        }
        if matched == 0 {
            if has_instances {
                Err("没有匹配 DXGI 物理适配器的 GPU Engine 实例".into())
            } else {
                Ok(None)
            }
        } else {
            Ok(busiest)
        }
    }
}

pub struct Sampler {
    baseline: Baseline,
    disk: Result<DiskQuery, String>,
    gpu: Result<GpuQuery, String>,
    disk_retry_at: Instant,
    gpu_retry_at: Instant,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        let disk = DiskQuery::new();
        let gpu = GpuQuery::new();
        let baseline = Baseline::new();
        let retry_at = baseline.started + RETRY_INTERVAL;
        Self {
            baseline,
            disk,
            gpu,
            disk_retry_at: retry_at,
            gpu_retry_at: retry_at,
        }
    }

    /// 基线未就绪时返回 None；之后两项分别报告成功或失败。
    pub fn poll(&mut self) -> Option<Snapshot> {
        let now = Instant::now();
        match self.baseline.check(now) {
            Readiness::Rebase => {
                // 这里只重建基线；查询本身的错误留给下一轮 poll 报告。
                if let Ok(disk) = &self.disk {
                    let _ = collect(&disk.query);
                }
                if let Ok(gpu) = &self.gpu {
                    let _ = collect(&gpu.query);
                }
                return None;
            }
            Readiness::Warming => return None,
            Readiness::Ready => {}
        }
        let disk = self
            .disk
            .as_ref()
            .map_err(Clone::clone)
            .and_then(DiskQuery::poll);
        let gpu = self
            .gpu
            .as_ref()
            .map_err(Clone::clone)
            .and_then(GpuQuery::poll);
        if disk.is_err() && now >= self.disk_retry_at {
            self.disk = DiskQuery::new();
            self.disk_retry_at = now + RETRY_INTERVAL;
        }
        if gpu.is_err() && now >= self.gpu_retry_at {
            self.gpu = GpuQuery::new();
            self.gpu_retry_at = now + RETRY_INTERVAL;
        }
        Some(Snapshot { disk, gpu })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_parser_requires_pid_and_engine_type() {
        assert_eq!(
            parse_instance("pid_31520_luid_0x00000000_0x00010AD1_phys_0_eng_0_engtype_3D"),
            Some((31520, "3D"))
        );
        assert_eq!(
            parse_instance("luid_0x00000000_0x00010AD1_engtype_3D"),
            None
        );
        assert_eq!(parse_instance("pid_3_engtype_"), None);
    }

    #[test]
    fn instance_name_stays_within_returned_buffer() {
        let mut buffer = [0_u64; 2];
        let name = buffer.as_mut_ptr().cast::<u16>();
        // SAFETY: buffer 以 u64 对齐，前两个 u16 落在已分配的 16 字节内。
        unsafe {
            name.write('A' as u16);
            name.add(1).write(0);
        }
        assert_eq!(name_from_buffer(&buffer, 4, name).unwrap(), "A");
        let unaligned = (name as usize + 1) as *const u16;
        assert!(name_from_buffer(&buffer, 4, unaligned).is_err());
        let outside = (name as usize + 16) as *const u16;
        assert!(name_from_buffer(&buffer, 4, outside).is_err());
        assert!(name_from_buffer(&buffer, 2, name).is_err());
    }
}
