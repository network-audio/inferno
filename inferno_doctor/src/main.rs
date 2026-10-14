//! Checks the setup of an Inferno virtual soundcard: settings, clock, network ports, ALSA plugin,
//! realtime permissions and saved state, and says what to fix.
//!
//! Settings are read like the ALSA plugin reads them: the `pcm.inferno` ALSA config, then `INFERNO_*`
//! environment variables. Run it as the user (and with the environment) of the application using the device.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::net::{SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use alsa_sys_all::*;
use inferno_aoip::device_server::{
    clock_path_or_default, diagnose_missing_clock, state_dir, Settings,
};

const PLUGIN: &str = "libasound_module_pcm_inferno.so";
/// keys of the plugin's ALSA config that aren't `Settings` (see alsa_pcm_inferno)
const PLUGIN_KEYS: &[&str] = &[
    "TYPE",
    "HINT",
    "COMMENT",
    "DISABLE_POLLFD",
    "USE_FLOWS_CLOCK",
];
/// realtime priority the flows threads ask for
const FLOWS_PRIORITY: u8 = 81;

#[derive(Default)]
struct Report {
    errors: usize,
    warnings: usize,
}

impl Report {
    fn ok(&mut self, msg: impl AsRef<str>) {
        println!("✓ {}", msg.as_ref());
    }
    fn warn(&mut self, msg: impl AsRef<str>) {
        self.warnings += 1;
        println!("! {}", msg.as_ref());
    }
    fn error(&mut self, msg: impl AsRef<str>) {
        self.errors += 1;
        println!("✗ {}", msg.as_ref());
    }
    fn info(&mut self, msg: impl AsRef<str>) {
        println!("  {}", msg.as_ref());
    }
    fn section(&mut self, title: &str) {
        println!("\n{title}");
    }
}

/// The `pcm.<name>` ALSA config node's literal values (references to arguments like `$NAME` are skipped:
/// they only have values when the device is opened with arguments). None if the device isn't defined.
fn alsa_device_config(name: &str) -> Option<BTreeMap<String, String>> {
    unsafe {
        let mut top = std::ptr::null_mut();
        if snd_config_update_ref(&mut top) < 0 {
            return None;
        }
        let config = device_config_in(top, name);
        snd_config_unref(top);
        config
    }
}

unsafe fn device_config_in(top: *mut snd_config_t, name: &str) -> Option<BTreeMap<String, String>> {
    unsafe {
        let key = CString::new(format!("pcm.{name}")).unwrap();
        let mut node = std::ptr::null_mut();
        if snd_config_search(top, key.as_ptr(), &mut node) < 0 {
            return None;
        }
        let mut config = BTreeMap::new();
        let mut pos = snd_config_iterator_first(node);
        while pos != snd_config_iterator_end(node) {
            let entry = snd_config_iterator_entry(pos);
            let mut id: *const libc::c_char = std::ptr::null();
            let mut value: *mut libc::c_char = std::ptr::null_mut();
            if snd_config_get_id(entry, &mut id) == 0
                && snd_config_get_ascii(entry, &mut value) == 0
            {
                if let (Ok(k), Ok(v)) =
                    (CStr::from_ptr(id).to_str(), CStr::from_ptr(value).to_str())
                {
                    if !k.starts_with('@') && !v.starts_with('$') {
                        config.insert(k.to_owned(), v.to_owned());
                    }
                }
                libc::free(value as *mut libc::c_void);
            }
            pos = snd_config_iterator_next(pos);
        }
        Some(config)
    }
}

/// alsa-lib loads plugins from `<directory of libasound>/alsa-lib`: find the libasound this process uses.
fn alsa_plugin_dir() -> Option<PathBuf> {
    unsafe {
        let mut info: libc::Dl_info = std::mem::zeroed();
        if libc::dladdr(snd_config_update_ref as *const libc::c_void, &mut info) == 0
            || info.dli_fname.is_null()
        {
            return None;
        }
        let lib = PathBuf::from(CStr::from_ptr(info.dli_fname).to_str().ok()?);
        let lib = std::fs::canonicalize(&lib).unwrap_or(lib);
        Some(lib.parent()?.join("alsa-lib"))
    }
}

fn check_alsa(report: &mut Report) -> BTreeMap<String, String> {
    report.section("ALSA");
    match alsa_plugin_dir() {
        Some(dir) if dir.join(PLUGIN).is_file() => {
            report.ok(format!("plugin installed: {}", dir.join(PLUGIN).display()))
        }
        Some(dir) => report.error(format!(
            "plugin not found in {}: run ./install.sh",
            dir.display()
        )),
        None => report.warn("can't tell where alsa-lib looks for plugins"),
    }
    match alsa_device_config("inferno") {
        Some(config) => {
            report.ok("ALSA device \"inferno\" is defined");
            config
        }
        None => {
            report.error("no ALSA device \"inferno\": run ./install.sh (or add alsa_pcm_inferno/asoundrc to ~/.asoundrc)");
            BTreeMap::new()
        }
    }
}

fn check_settings(report: &mut Report, alsa_config: &BTreeMap<String, String>) -> Option<Settings> {
    report.section("Settings (ALSA config, then INFERNO_* variables)");
    let config = Settings::merged_config(alsa_config);
    let unknown = Settings::unknown_keys(&config, PLUGIN_KEYS);
    if !unknown.is_empty() {
        report.warn(format!(
            "unknown settings, ignored (misspelt?): {}",
            unknown.join(", ")
        ));
    }
    let settings = match Settings::try_new("inferno-doctor", "inferno-doctor", None, &config) {
        Ok(settings) => settings,
        Err(e) => {
            report.error(e.to_string());
            return None;
        }
    };
    let info = &settings.self_info;
    report.ok(format!(
        "valid: {} RX and {} TX channels at {} Hz, receive latency {} ms",
        info.rx_channels.len(),
        info.tx_channels.len(),
        info.sample_rate,
        info.latency_ns as f64 / 1e6
    ));
    if let Some(name) = config.get("NAME") {
        report.info(format!("device name: {name}"));
    } else {
        report.info(
            "device name: from the application's name and IP address (set NAME to choose one)",
        );
    }

    let iface = info.interface_name.clone().unwrap_or("?".into());
    report.info(format!("network: {} on {iface}", info.ip_address));
    if !config.contains_key("BIND_IP") {
        let candidates = netdev::get_interfaces()
            .into_iter()
            .filter(|i| !i.is_loopback() && !i.ipv4.is_empty())
            .map(|i| i.name)
            .collect::<Vec<_>>();
        if candidates.len() > 1 {
            report.warn(format!(
        "BIND_IP not set, using the default route's interface {iface} of {}: set BIND_IP to the interface \
         connected to the Dante network",
        candidates.join(", ")
      ));
        }
    }
    Some(settings)
}

async fn check_clock(report: &mut Report, settings: &Settings) {
    report.section("Clock");
    let path = clock_path_or_default(&settings.clock_path);
    let client = usrvclock::AsyncClient::start(path.clone(), Box::new(|_| {}));
    let mut rx = client.subscribe();
    let got = tokio::time::timeout(
        Duration::from_secs(3),
        rx.wait_for(|overlay| overlay.is_some()),
    )
    .await;
    match got {
        Ok(Ok(_)) => report.ok(format!("clock received from {}", path.display())),
        _ => report.error(format!(
            "no clock within 3 s: {}",
            diagnose_missing_clock(&path)
        )),
    }
    let _ = client.stop().await;
}

fn check_ports(report: &mut Report, settings: &Settings) {
    report.section("Network ports");
    let info = &settings.self_info;
    let ports = [
        ("ARC", info.arc_port),
        ("CMC", info.cmc_port),
        ("flows control", info.flows_control_port),
        ("device info", info.info_request_port),
    ];
    let busy: Vec<String> = ports
        .iter()
        .filter(|(_, port)| UdpSocket::bind(SocketAddrV4::new(info.ip_address, *port)).is_err())
        .map(|(name, port)| format!("{port} ({name})"))
        .collect();
    let all = ports
        .iter()
        .map(|(_, port)| port.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if busy.is_empty() {
        report.ok(format!("UDP ports {all} on {} are free", info.ip_address));
    } else {
        report.warn(format!(
            "UDP {} in use on {}: fine if this device is open in an application right now; \
             otherwise another device uses them, give each device its own ALT_PORT (at least 10 apart)",
            busy.join(", "),
            info.ip_address
        ));
    }
    report.info(format!(
        "a firewall must allow incoming UDP {all}, 5353 (mDNS), 4321 (multicast audio), 319 and 320 \
         (PTP) and audio flows (any port)"
    ));
}

fn rlimit(resource: libc::__rlimit_resource_t) -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    (unsafe { libc::getrlimit(resource, &mut lim) } == 0).then_some(lim.rlim_cur as u64)
}

fn check_realtime(report: &mut Report) {
    report.section("Realtime");
    let result =
        std::thread::spawn(|| inferno_aoip::utils::set_current_thread_realtime(FLOWS_PRIORITY))
            .join()
            .unwrap();
    let rtprio = rlimit(libc::RLIMIT_RTPRIO).map_or("?".into(), |v| v.to_string());
    match result {
    Ok(()) => report.ok("audio threads can run with realtime priority"),
    Err(e) => report.warn(format!(
      "audio threads can't get realtime priority ({e:?}; RLIMIT_RTPRIO is {rtprio}): audio may drop out under \
       load. Allow it, e.g. with rtprio 95 for your user or group in /etc/security/limits.conf"
    )),
  }
    if let Some(memlock) = rlimit(libc::RLIMIT_MEMLOCK) {
        let shown = if memlock == libc::RLIM_INFINITY as u64 {
            "unlimited".into()
        } else {
            format!("{} KiB", memlock / 1024)
        };
        report.info(format!("memlock limit: {shown}"));
    }
}

fn check_state(report: &mut Report, settings: &Settings) {
    report.section("Saved state");
    let dir = state_dir(&settings.self_info);
    if Path::new(&dir).is_dir() {
        report.ok(format!(
            "{dir} (delete it to reset subscriptions and channel names)"
        ));
    } else {
        report.info(format!("nothing saved yet; it will be in {dir}"));
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut report = Report::default();
    let alsa_config = check_alsa(&mut report);
    if let Some(settings) = check_settings(&mut report, &alsa_config) {
        check_clock(&mut report, &settings).await;
        check_ports(&mut report, &settings);
        check_realtime(&mut report);
        check_state(&mut report, &settings);
    }
    println!();
    match (report.errors, report.warnings) {
        (0, 0) => println!("All good."),
        (0, w) => println!("{w} warning(s), see above."),
        (e, w) => println!("{e} problem(s) to fix and {w} warning(s), see above."),
    }
    if report.errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_config_reads_literal_values_and_skips_arguments() {
        let text = r#"
pcm.inferno {
    type inferno
    @args.NAME { type string }
    NAME $NAME
    RX_CHANNELS "8"
    hint { show on }
}
"#;
        unsafe {
            let mut top = std::ptr::null_mut();
            assert_eq!(snd_config_top(&mut top), 0);
            let mut input = std::ptr::null_mut();
            let c = CString::new(text).unwrap();
            assert_eq!(
                snd_input_buffer_open(&mut input, c.as_ptr(), text.len() as isize),
                0
            );
            assert_eq!(snd_config_load(top, input), 0);
            snd_input_close(input);
            let config = device_config_in(top, "inferno").expect("device not found");
            assert!(device_config_in(top, "other").is_none());
            snd_config_delete(top);
            assert_eq!(config.get("type").map(String::as_str), Some("inferno"));
            assert_eq!(config.get("RX_CHANNELS").map(String::as_str), Some("8"));
            assert!(
                !config.contains_key("NAME"),
                "argument reference read as a value: {config:?}"
            );
            assert!(!config.keys().any(|k| k.starts_with('@')), "{config:?}");
        }
    }
}
