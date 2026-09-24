//! 设备选择只在窗口启动时做一次；两个前端使用同一规则。
use std::io;

use crate::{
    config::{self, Command, Config},
    network, nvme,
};

#[derive(Clone, PartialEq, Eq)]
pub struct Sources {
    pub network_luid: Option<u64>,
    pub network_name: Option<String>,
    pub disk: Option<nvme::DiskDevice>,
}

/// 处理帮助和列表命令；只有需要打开窗口时才返回配置。
pub fn handle_command_line(binary: &str) -> io::Result<Option<Config>> {
    match Command::parse(std::env::args().skip(1)).map_err(io::Error::other)? {
        Command::Help => println!("{}", config::help(binary)),
        Command::ListNetwork => network::print_interfaces()?,
        Command::ListDisks => {
            for disk in nvme::list_disks()? {
                println!("{}  {}", disk.path, disk.name);
            }
        }
        Command::Run(config) => return Ok(Some(config)),
    }
    Ok(None)
}

pub fn select(config: &Config) -> io::Result<Sources> {
    let network = match network::collect_interfaces() {
        Ok(interfaces) => network::select_interface(&interfaces, config.interface.as_deref())?
            .map(|selected| (selected.luid, selected.name.clone())),
        Err(error) if config.interface.is_none() => {
            eprintln!("网络接口枚举失败，继续显示其它指标：{error}");
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
    let (network_luid, network_name) = network.unzip();
    Ok(Sources {
        network_luid,
        network_name,
        disk,
    })
}
