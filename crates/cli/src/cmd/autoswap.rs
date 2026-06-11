//! `subswap autoswap [on|off]` — 手动开关自动切换。

use anyhow::Result;
use subswap_core::settings;

pub fn run(toggle: Option<&str>) -> Result<()> {
    match toggle {
        None => {
            let enabled = settings::current().auto_swap.enabled;
            if enabled {
                println!("auto swap: on");
            } else {
                println!("auto swap: off");
            }
        }
        Some("on") => {
            settings::set_auto_swap_enabled(true)?;
            println!("auto swap: on");
        }
        Some("off") => {
            settings::set_auto_swap_enabled(false)?;
            println!("auto swap: off");
        }
        Some(other) => {
            anyhow::bail!("unknown argument {other:?}; expected 'on' or 'off'");
        }
    }
    Ok(())
}
