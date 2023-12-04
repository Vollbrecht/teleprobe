mod specifier;

use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{bail, Result};
use clap::Parser;
use probe_rs::{DebugProbeInfo, DebugProbeSelector, Lister, MemoryInterface, Permissions, Session};
pub use specifier::ProbeSpecifier;

static UHUBCTL_MUTEX: Mutex<()> = Mutex::new(());

const SETTLE_REPROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Clone, Parser)]
pub struct Opts {
    /// The probe to use (specified by eg. `VID:PID`, `VID:PID:Serial`, or just `Serial`).
    #[clap(long, env = "PROBE_RUN_PROBE")]
    pub probe: Option<ProbeSpecifier>,

    /// The probe clock frequency in kHz
    #[clap(long)]
    pub speed: Option<u32>,

    /// Chip name
    #[clap(long)]
    pub chip: String,

    /// Connect to device when NRST is pressed.
    #[clap(long)]
    pub connect_under_reset: bool,

    // If the target should be tried to be power cycled via USB
    #[clap(long)]
    pub power_reset: bool,

    #[clap(long, default_value = "1")]
    pub cycle_delay_seconds: f64,

    #[clap(long, default_value = "2000")]
    pub max_settle_time_millis: u64,
}

pub fn list() -> Result<()> {
    let probe_lister = Lister::new();
    let probes = probe_lister.list_all();
    if probes.is_empty() {
        println!("No probe found!");
        return Ok(());
    }
    for probe in probes {
        println!(
            "{:04x}:{:04x}:{} -- {:?} {}",
            probe.vendor_id,
            probe.product_id,
            probe.serial_number.unwrap_or_else(|| "SN unspecified".to_string()),
            probe.probe_type,
            probe.identifier,
        );
    }

    Ok(())
}

pub fn connect(opts: &Opts) -> Result<Session> {
    let mut probes = get_probe(&opts)?;

    {
        if opts.power_reset {
            if probes[0].serial_number.is_none() {
                bail!("power reset requires a serial number");
            }
            log::debug!("probe power reset");
            if let Err(err) = power_reset(&probes[0].serial_number.as_ref().unwrap(), 0.5) {
                log::warn!("power reset failed for: {}", err);
            }

            let end = Instant::now() + std::time::Duration::from_millis(opts.max_settle_time_millis);
            probes = vec![];
            while Instant::now() < end && probes.is_empty() {
                std::thread::sleep(SETTLE_REPROBE_INTERVAL);
                probes = match get_probe(&opts) {
                    Ok(p) => p,
                    Err(_) => probes,
                }
            }
            if probes.is_empty() {
                bail!("Probe not reappeared after power reset")
            }
        }
    }

    let selector = DebugProbeSelector {
        vendor_id: probes[0].vendor_id,
        product_id: probes[0].product_id,
        serial_number: probes[0].serial_number.clone(),
    };
    let lister = Lister::new();

    // GIANT HACK to reset both cores in rp2040.
    // Ideally this would be a custom sequence in probe-rs:
    // https://github.com/probe-rs/probe-rs/pull/1603
    if opts.chip.to_ascii_uppercase().starts_with("RP2040") {
        let mut probe = lister.open(selector)?;

        log::debug!("opened probe for rp2040 reset");

        if let Some(speed) = opts.speed {
            probe.set_speed(speed)?;
        }

        let perms = Permissions::new().allow_erase_all();
        let target = probe_rs::config::get_target_by_name(&opts.chip)?;
        let mut sess = probe.attach(target, perms)?;
        let mut core = sess.core(0)?;

        const PSM_FRCE_ON: u64 = 0x40010000;
        const PSM_FRCE_OFF: u64 = 0x40010004;
        const PSM_WDSEL: u64 = 0x40010008;

        const PSM_SEL_SIO: u32 = 1 << 14;
        const PSM_SEL_PROC0: u32 = 1 << 15;
        const PSM_SEL_PROC1: u32 = 1 << 16;

        const WATCHDOG_CTRL: u64 = 0x40058000;
        const WATCHDOG_CTRL_TRIGGER: u32 = 1 << 31;
        const WATCHDOG_CTRL_ENABLE: u32 = 1 << 30;

        log::debug!("rp2040: resetting SIO and processors");
        core.write_word_32(PSM_WDSEL, PSM_SEL_SIO | PSM_SEL_PROC0 | PSM_SEL_PROC1)?;
        core.write_word_32(WATCHDOG_CTRL, WATCHDOG_CTRL_ENABLE)?;
        core.write_word_32(WATCHDOG_CTRL, WATCHDOG_CTRL_ENABLE | WATCHDOG_CTRL_TRIGGER)?;
        log::debug!("rp2040: reset done, reattaching");
    }

    log::debug!("opened probe");

    let selector = DebugProbeSelector {
        vendor_id: probes[0].vendor_id,
        product_id: probes[0].product_id,
        serial_number: probes[0].serial_number.clone(),
    };

    let mut probe = lister.open(selector)?;

    if let Some(speed) = opts.speed {
        probe.set_speed(speed)?;
    }

    let perms = Permissions::new().allow_erase_all();

    let target = probe_rs::config::get_target_by_name(&opts.chip)?;

    let sess = if opts.connect_under_reset {
        probe.attach_under_reset(target, perms)?
    } else {
        probe.attach(target, perms)?
    };
    log::debug!("started session");

    Ok(sess)
}

fn get_probe(opts: &&Opts) -> Result<Vec<DebugProbeInfo>> {
    let lister = Lister::new();
    let probes = lister.list_all();
    let probes = if let Some(selected_probe) = &opts.probe {
        probes_filter(&probes, selected_probe)
    } else {
        probes
    };

    // ensure exactly one probe is found and open it
    if probes.is_empty() {
        bail!("no probe was found")
    }
    log::debug!("found {} probes", probes.len());
    if probes.len() > 1 {
        //let _ = print_probes(probes);
        bail!("more than one probe found; use --probe to specify which one to use");
    }
    Ok(probes)
}

pub fn probes_filter(probes: &[DebugProbeInfo], selector: &ProbeSpecifier) -> Vec<DebugProbeInfo> {
    probes
        .iter()
        .filter(|&p| {
            if let Some((vid, pid)) = selector.vid_pid {
                if p.vendor_id != vid || p.product_id != pid {
                    return false;
                }
            }

            if let Some(serial) = &selector.serial {
                if p.serial_number.as_deref() != Some(serial) {
                    return false;
                }
            }

            true
        })
        .cloned()
        .collect()
}

fn power_reset(probe_serial: &str, cycle_delay_seconds: f64) -> Result<()> {
    let _guard = UHUBCTL_MUTEX.lock();
    let output = Command::new("uhubctl")
        .arg("-a")
        .arg("cycle")
        .arg("-d")
        .arg(format!("{:.2}", cycle_delay_seconds))
        .arg("-s")
        .arg(probe_serial)
        .output();
    drop(_guard);

    match output {
        Ok(output) => {
            if output.status.success() {
                Ok(())
            } else {
                bail!(
                    "uhubctl failed for serial \'{}\' with delay {}:  {}",
                    cycle_delay_seconds,
                    probe_serial,
                    String::from_utf8_lossy(&output.stderr)
                )
            }
        }
        Err(e) => bail!("uhubctl failed: {}", e),
    }
}
