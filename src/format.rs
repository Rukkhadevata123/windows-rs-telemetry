//! 两个前端共用的数值格式。
const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = (1_u64 << 30) as f64;

pub fn rate(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= MIB {
        format!("{:.2} MiB/s", bytes_per_sec / MIB)
    } else {
        format!("{:.1} KiB/s", bytes_per_sec / KIB)
    }
}

/// 输入是 0..1 的比例。
pub fn percent(ratio: f64) -> String {
    format!("{:.1}%", ratio * 100.0)
}

pub fn ghz(mhz: f64) -> String {
    format!("{:.2} GHz", mhz / 1000.0)
}

pub fn gib_pair(used_bytes: u64, total_bytes: u64) -> String {
    format!(
        "{:.2} / {:.2} GiB",
        used_bytes as f64 / GIB,
        total_bytes as f64 / GIB
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn units_switch_at_one_mebibyte() {
        assert_eq!(super::rate(512.0), "0.5 KiB/s");
        assert_eq!(super::rate(1024.0 * 1024.0), "1.00 MiB/s");
        assert_eq!(super::gib_pair(1 << 30, 4 << 30), "1.00 / 4.00 GiB");
    }
}
