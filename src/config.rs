//! 两个窗口入口共用的命令行选项。

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub interface: Option<String>,
    pub disk: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(Config),
    ListNetwork,
    ListDisks,
    Help,
}

impl Command {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut config = Config::default();
        let mut action = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--help" | "-h" => set_action(&mut action, Self::Help)?,
                "--list-network" => set_action(&mut action, Self::ListNetwork)?,
                "--list-disks" => set_action(&mut action, Self::ListDisks)?,
                "--interface" => {
                    if config.interface.is_some() {
                        return Err("--interface 只能指定一次".into());
                    }
                    config.interface = Some(value(&mut args, "--interface")?);
                }
                "--disk" => {
                    if config.disk.is_some() {
                        return Err("--disk 只能指定一次".into());
                    }
                    config.disk = Some(value(&mut args, "--disk")?);
                }
                _ => return Err(format!("无法识别参数 {arg}。请运行 --help 查看可用选项。")),
            }
        }
        if let Some(action) = action {
            if config != Config::default() {
                return Err("列表或帮助命令不能与设备选择参数组合".into());
            }
            Ok(action)
        } else {
            Ok(Self::Run(config))
        }
    }
}

fn value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String, String> {
    let value = args.next().ok_or_else(|| format!("{option} 后需要参数"))?;
    if value.is_empty() || value.starts_with('-') {
        return Err(format!("{option} 后需要参数"));
    }
    Ok(value)
}

fn set_action(slot: &mut Option<Command>, action: Command) -> Result<(), String> {
    if slot.is_some() {
        return Err("只能指定一个操作命令".into());
    }
    *slot = Some(action);
    Ok(())
}

pub fn help(binary: &str) -> String {
    format!(
        "用法：{binary} [--interface 名称] [--disk \\\\.\\PhysicalDriveN]\n\
         选项：\n\
           --interface 名称  选择物理 WLAN 或以太网接口\n\
           --disk 路径       选择 NVMe 物理磁盘\n\
           --list-network    列出网络接口\n\
           --list-disks      列出 NVMe 磁盘\n\
           --help, -h        显示帮助"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_and_missing_options() {
        assert!(Command::parse(["--bogus".into()]).is_err());
        assert!(Command::parse(["--interface".into()]).is_err());
        assert!(Command::parse(["--disk".into(), "--help".into()]).is_err());
        assert!(
            Command::parse([
                "--interface".into(),
                "WLAN".into(),
                "--interface".into(),
                "VPN".into()
            ])
            .is_err()
        );
    }

    #[test]
    fn accepts_a_selected_device_for_window_mode() {
        assert_eq!(
            Command::parse(["--interface".into(), "WLAN".into()]).unwrap(),
            Command::Run(Config {
                interface: Some("WLAN".into()),
                disk: None
            })
        );
    }

    #[test]
    fn actions_cannot_be_combined_with_other_actions_or_sources() {
        assert_eq!(Command::parse(["-h".into()]).unwrap(), Command::Help);
        assert!(Command::parse(["--help".into(), "--list-disks".into()]).is_err());
        assert!(
            Command::parse(["--list-network".into(), "--interface".into(), "WLAN".into(),])
                .is_err()
        );
    }
}
