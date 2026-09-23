//! 设备选择只在窗口启动时做一次；两个前端使用同一规则。
use std::io;

use crate::{config::Config, network, nvme};

#[derive(Clone, PartialEq, Eq)]
pub struct Sources {
    pub network_luid: Option<u64>,
    pub network_name: String,
    pub disk: Option<nvme::DiskDevice>,
    pub disk_name: String,
}

pub fn select(config: &Config) -> io::Result<Sources> {
    let network = match network::collect_interfaces() {
        Ok(interfaces) => network::select_interface(&interfaces, config.interface.as_deref())?
            .map(|selected| (selected.luid, selected.name.clone())),
        Err(error) if config.interface.is_none() => {
            eprintln!("WLAN 枚举失败，继续显示其它指标：{error}");
            None
        }
        Err(error) => return Err(error),
    };
    let disk = match nvme::select_disk(config.disk.as_deref()) {
        Ok(selected) => selected,
        Err(error) if config.disk.is_none() => {
            eprintln!("NVMe 枚举失败，继续显示其它指标：{error}");
            None
        }
        Err(error) => return Err(error),
    };
    Ok(Sources {
        network_luid: network.as_ref().map(|(luid, _)| *luid),
        network_name: network.map_or_else(|| "未选择 WLAN".into(), |(_, name)| name),
        disk_name: disk
            .as_ref()
            .map_or_else(|| "未选择 NVMe".into(), |disk| disk.name.clone()),
        disk,
    })
}
