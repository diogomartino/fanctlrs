//! Hardware, config and fan logic shared by the window and `fanctlrs --daemon`.
//! No GUI code here, so the daemon stays small.

use std::{
    collections::HashMap,
    fs, io,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

/// Written before handing a fan back to auto. Some chips (it87) keep the last
/// manual duty in auto mode until the firmware recalculates, so a fan stopped
/// at 0% could otherwise stay stopped.
pub const AUTO_SEED_DUTY: u8 = 51;

/// Degrees a temperature must fall before a curve slows a fan down again,
/// so fans don't hunt up and down around a curve point.
const HYSTERESIS: f32 = 3.0;

pub fn pct_to_duty(pct: f32) -> u8 {
    (pct.clamp(0.0, 100.0) * 255.0 / 100.0).round() as u8
}

pub fn read_num(path: impl AsRef<Path>) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("fanctlrs")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn read_config(file: &str) -> String {
    fs::read_to_string(config_dir().join(file)).unwrap_or_default()
}

/// `key = value` lines, used by every config file.
pub fn parse_kv(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

pub fn format_kv<'a>(pairs: impl Iterator<Item = (&'a String, &'a String)>) -> String {
    let mut lines: Vec<_> = pairs.map(|(k, v)| format!("{k} = {v}")).collect();
    lines.sort();
    lines.join("\n") + "\n"
}

pub fn pretty_chip(name: &str) -> &str {
    match name {
        "k10temp" | "zenpower" | "coretemp" => "CPU",
        "amdgpu" | "nouveau" => "GPU",
        "nvme" => "NVMe",
        n if n.starts_with("it8") || n.starts_with("nct6") || n.starts_with("f71") => "Motherboard",
        "asusec" => "ASUS EC",
        n if n.starts_with("ucsi") => "USB-C",
        n => n,
    }
}

pub struct Chip {
    pub dir: PathBuf,
    pub name: String,
    /// `name@device`: hwmonN changes between boots, the device path doesn't.
    pub key: String,
    pub title: String,
}

pub fn chips() -> Vec<Chip> {
    let mut dirs: Vec<PathBuf> = fs::read_dir("/sys/class/hwmon")
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    let mut chips: Vec<Chip> = dirs
        .into_iter()
        .map(|dir| {
            let name = fs::read_to_string(dir.join("name")).unwrap_or_default().trim().to_string();
            let dev = fs::canonicalize(dir.join("device"))
                .ok()
                .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                .unwrap_or_default();
            let title = pretty_chip(&name).to_string();
            Chip { dir, key: format!("{name}@{dev}"), name, title }
        })
        // Duplicates the Super I/O + EC readings, and frequent WMI polling is
        // known to upset some ASUS BIOSes.
        .filter(|c| c.name != "asus_wmi_sensors")
        .collect();
    // Two NVMe drives -> "NVMe 1", "NVMe 2".
    let titles: Vec<String> = chips.iter().map(|c| c.title.clone()).collect();
    for (i, c) in chips.iter_mut().enumerate() {
        if titles.iter().filter(|t| **t == c.title).count() > 1 {
            c.title = format!("{} {}", c.title, titles[..=i].iter().filter(|t| **t == c.title).count());
        }
    }
    chips
}

/// `chip@device/temp1` -> its `_input` file.
pub fn sensor_path(chips: &[Chip], key: &str) -> Option<PathBuf> {
    let (chip, base) = key.split_once('/')?;
    Some(chips.iter().find(|c| c.key == chip)?.dir.join(format!("{base}_input")))
}

/// `chip@device/pwm3` -> (chip dir, 3).
fn fan_location(chips: &[Chip], key: &str) -> Option<(PathBuf, u32)> {
    let (chip, pwm) = key.split_once('/')?;
    let n = pwm.strip_prefix("pwm")?.parse().ok()?;
    Some((chips.iter().find(|c| c.key == chip)?.dir.clone(), n))
}

pub fn fan_file(dir: &Path, n: u32, file: &str) -> PathBuf {
    dir.join(file.replace('N', &n.to_string()))
}

fn fan_write(dir: &Path, n: u32, file: &str, val: u8) -> io::Result<()> {
    fs::write(fan_file(dir, n, file), val.to_string())
}

/// Temperature (°C) -> fan %, as points sorted by temperature.
#[derive(Clone, Debug, PartialEq)]
pub struct Curve {
    /// Sensor key, `chip@device/tempN`.
    pub sensor: String,
    pub points: Vec<(f32, f32)>,
}

impl Curve {
    pub fn new(sensor: String) -> Self {
        Curve { sensor, points: vec![(40.0, 30.0), (60.0, 50.0), (75.0, 80.0), (85.0, 100.0)] }
    }

    /// `k10temp@0000:00:18.3/temp1 40:30 60:50 75:80 85:100`
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split_whitespace();
        let sensor = parts.next()?.to_string();
        let points: Vec<(f32, f32)> = parts
            .map(|p| {
                let (t, pct) = p.split_once(':')?;
                Some((t.parse().ok()?, pct.parse().ok()?))
            })
            .collect::<Option<_>>()?;
        (points.len() >= 2).then_some(Curve { sensor, points })
    }

    pub fn format(&self) -> String {
        let points: Vec<String> = self.points.iter().map(|(t, p)| format!("{t:.0}:{p:.0}")).collect();
        format!("{} {}", self.sensor, points.join(" "))
    }

    /// Linear between points, flat beyond the first and last.
    pub fn pct_at(&self, temp: f32) -> f32 {
        let (first, last) = (self.points[0], self.points[self.points.len() - 1]);
        if temp <= first.0 {
            return first.1;
        }
        for w in self.points.windows(2) {
            let ((t0, p0), (t1, p1)) = (w[0], w[1]);
            if temp <= t1 {
                return p0 + (p1 - p0) * (temp - t0) / (t1 - t0).max(f32::EPSILON);
            }
        }
        last.1
    }
}

/// Speed up right away, slow down only once the temperature has fallen
/// `HYSTERESIS` degrees below where the current speed was set.
/// `last` is (temperature, %) of the last change.
pub fn curve_target(curve: &Curve, temp: f32, last: &mut Option<(f32, f32)>) -> f32 {
    let want = curve.pct_at(temp);
    match *last {
        Some((t, pct)) if want < pct && temp > t - HYSTERESIS => pct,
        _ => {
            *last = Some((temp, want));
            want
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Auto,
    Manual(f32),
    Curve,
}

impl Mode {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "auto" => Some(Mode::Auto),
            "curve" => Some(Mode::Curve),
            n => n.parse().ok().map(Mode::Manual),
        }
    }

    pub fn format(&self) -> String {
        match self {
            Mode::Auto => "auto".into(),
            Mode::Manual(pct) => format!("{pct:.0}"),
            Mode::Curve => "curve".into(),
        }
    }
}

/// The `fans` file stores `key = mode` plus `key#curve = curve` per fan.
pub fn curve_key(fan_key: &str) -> String {
    format!("{fan_key}#curve")
}

/// Is the fan out of manual mode? `pwmN_enable = 0` is "full speed", and some
/// drivers (it87) report manual 100% (duty 255) that way, so that one still counts as ours.
fn firmware_took_back(enable: Option<f64>, duty: Option<f64>) -> bool {
    match enable {
        Some(1.0) => false,
        Some(0.0) => duty != Some(255.0),
        _ => true,
    }
}

/// One control step for one fan. Returns true when the firmware had taken
/// the fan back (BIOS/EC, suspend/resume) and we re-applied the user's setting.
/// `temp` is the curve sensor's reading in °C; unreadable means full speed.
pub fn enforce(dir: &Path, n: u32, mode: &Mode, curve: Option<&Curve>, temp: Option<f32>, last: &mut Option<(f32, f32)>) -> io::Result<bool> {
    let pct = match (mode, curve) {
        (Mode::Auto, _) => return Ok(false),
        (Mode::Manual(pct), _) => *pct,
        (Mode::Curve, Some(curve)) => match temp {
            Some(t) => curve_target(curve, t, last),
            None => 100.0,
        },
        (Mode::Curve, None) => 100.0,
    };
    let taken_back = firmware_took_back(read_num(fan_file(dir, n, "pwmN_enable")), read_num(fan_file(dir, n, "pwmN")));
    let duty = pct_to_duty(pct);
    // Some drivers (amdgpu) read back a slightly different duty than written.
    let off = read_num(fan_file(dir, n, "pwmN")).is_none_or(|d| (d - duty as f64).abs() > 2.0);
    // A manual speed is only rewritten if the firmware took the fan; a curve
    // is rewritten whenever its target moves.
    if taken_back || (*mode == Mode::Curve && off) {
        fan_write(dir, n, "pwmN_enable", 1)?;
        fan_write(dir, n, "pwmN", duty)?;
    }
    Ok(taken_back)
}

fn pid_file(role: &str) -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    runtime.join(format!("fanctlrs-{role}.pid"))
}

/// The command line of the process in `role`'s pid file, if it's still alive.
fn running_cmdline(role: &str) -> Option<String> {
    let pid = fs::read_to_string(pid_file(role)).ok()?;
    let cmd = fs::read(format!("/proc/{}/cmdline", pid.trim())).ok()?;
    Some(String::from_utf8_lossy(&cmd).into_owned())
}

/// The window leaves fan control to the daemon while it runs, so the two
/// never fight over a fan.
pub fn daemon_running() -> bool {
    running_cmdline("daemon").is_some_and(|cmd| cmd.contains("--daemon"))
}

/// Open windows: every `fanctlrs` process of this user that isn't the daemon.
/// Found by scanning /proc instead of a pid file, which goes stale when a
/// second window opens and closes.
fn gui_pids() -> Vec<String> {
    let me = std::process::id().to_string();
    fs::read_dir("/proc")
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|pid| pid.bytes().all(|b| b.is_ascii_digit()) && *pid != me)
        .filter(|pid| fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "fanctlrs"))
        .filter(|pid| fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| !String::from_utf8_lossy(&c).contains("--daemon")))
        .collect()
}

fn gui_running() -> bool {
    !gui_pids().is_empty()
}

/// Start the window from the tray as its own transient systemd unit:
/// - it gets the user manager's *current* environment. The daemon may have
///   started at boot, before the desktop exported WAYLAND_DISPLAY/DISPLAY,
///   so its own environment can't open a window.
/// - restarting the service (e.g. reinstalling it from that window) doesn't kill it.
fn open_gui() {
    if gui_running() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let child = Command::new("systemd-run")
        .args(["--user", "--quiet", "--collect"])
        .arg(&exe)
        .spawn()
        .or_else(|_| Command::new(&exe).spawn());
    if let Ok(mut child) = child {
        // Reap it when it exits, so it doesn't linger as a zombie.
        thread::spawn(move || child.wait());
    }
}

/// Tray "Exit": close the window, give every fan back to the firmware (a
/// stopped daemon would otherwise leave them frozen at their last speed) and
/// stop the service. The `fans` file is kept, so the next start restores it.
fn exit_all() {
    let windows = gui_pids();
    if !windows.is_empty() {
        let _ = Command::new("kill").args(&windows).status();
    }
    let state = parse_kv(&read_config("fans"));
    let chips = chips();
    for (key, mode) in &state {
        if Mode::parse(mode).is_some_and(|m| m != Mode::Auto) {
            if let Some((dir, n)) = fan_location(&chips, key) {
                let _ = fan_write(&dir, n, "pwmN", AUTO_SEED_DUTY);
                let _ = fan_write(&dir, n, "pwmN_enable", 2);
            }
        }
    }
    // --no-block: systemd stops the unit while we exit cleanly ourselves, so
    // Restart=on-failure doesn't bring it back. Also covers a daemon started by hand.
    let _ = Command::new("systemctl").args(["--user", "--no-block", "stop", "fanctlrs"]).status();
    let _ = fs::remove_file(pid_file("daemon"));
    std::process::exit(0);
}

struct FanTray;

impl ksni::Tray for FanTray {
    fn id(&self) -> String {
        "fanctlrs".into()
    }

    fn title(&self) -> String {
        "fanctlrs".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![fan_icon(32)]
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        open_gui();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            ksni::menu::StandardItem { label: "Open fanctlrs".into(), activate: Box::new(|_| open_gui()), ..Default::default() }.into(),
            ksni::MenuItem::Separator,
            ksni::menu::StandardItem { label: "Exit".into(), activate: Box::new(|_| exit_all()), ..Default::default() }.into(),
        ]
    }
}

/// A three-blade fan, drawn in code so there's no image file to ship.
/// ARGB32, big-endian, as the tray protocol wants.
fn fan_icon(size: i32) -> ksni::Icon {
    let c = (size as f32 - 1.0) / 2.0;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            let r = (dx * dx + dy * dy).sqrt() / c;
            let angle = dy.atan2(dx);
            // Blades curve as they go out; the hub is a solid disc.
            let blade = (3.0 * angle + 2.5 * r).sin() > 0.3 && r < 0.95;
            let on = r < 0.28 || (blade && r > 0.2);
            data.extend_from_slice(if on { &[255, 0x4f, 0xa3, 0xf7] } else { &[0, 0, 0, 0] });
        }
    }
    ksni::Icon { width: size, height: size, data }
}

/// `fanctlrs --daemon`: apply the `fans` file every 2 seconds, no window,
/// plus the tray icon.
pub fn run_daemon() {
    let _ = fs::write(pid_file("daemon"), std::process::id().to_string());
    // Registers whenever a tray (panel) shows up, also when started before login.
    use ksni::blocking::TrayMethods;
    let _tray = FanTray.assume_sni_available(true).spawn().map_err(|e| eprintln!("fanctlrs: no tray icon: {e}"));
    let mut last: HashMap<String, Option<(f32, f32)>> = HashMap::new();
    let mut last_error = String::new();
    loop {
        let state = parse_kv(&read_config("fans"));
        let chips = chips();
        let mut errors = Vec::new();
        for (key, mode) in &state {
            let (Some(mode), Some((dir, n))) = (Mode::parse(mode), fan_location(&chips, key)) else { continue };
            let curve = state.get(&curve_key(key)).and_then(|c| Curve::parse(c));
            let temp = curve
                .as_ref()
                .and_then(|c| sensor_path(&chips, &c.sensor))
                .and_then(read_num)
                .map(|t| t as f32 / 1000.0);
            if let Err(e) = enforce(&dir, n, &mode, curve.as_ref(), temp, last.entry(key.clone()).or_default()) {
                errors.push(format!("{key}: {e}"));
            }
        }
        // Log each new problem once instead of every 2 seconds.
        let error = errors.join("; ");
        if error != last_error {
            if !error.is_empty() {
                eprintln!("fanctlrs: {error}");
            }
            last_error = error;
        }
        thread::sleep(Duration::from_secs(2));
    }
}

// --- Installing the daemon and permissions -----------------------------------

fn unit_file() -> PathBuf {
    config_dir().parent().unwrap().join("systemd/user/fanctlrs.service")
}

fn installed_binary() -> PathBuf {
    home().join(".local/bin/fanctlrs")
}

pub fn service_installed() -> bool {
    unit_file().exists()
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let out = Command::new("systemctl").arg("--user").args(args).output().map_err(|e| format!("systemctl: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Root can't reach the user's service manager and would leave root-owned
/// files in the user's home.
pub fn running_as_root() -> bool {
    fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().nth(1))
        == Some("0")
}

/// A systemd user service: no root, starts at login.
pub fn install_service() -> Result<(), String> {
    if running_as_root() {
        return Err("Don't run fanctlrs with sudo: the service must be installed as your user. Close it and start it normally.".into());
    }
    if cfg!(debug_assertions) {
        return Err("This is a debug build (~750 MB). Install the service from `cargo run --release`.".into());
    }
    // Stop first: the running binary can't be overwritten ("text file busy").
    let _ = systemctl(&["stop", "fanctlrs"]);
    let bin = installed_binary();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if exe != bin {
        fs::create_dir_all(bin.parent().unwrap()).map_err(|e| e.to_string())?;
        fs::copy(&exe, &bin).map_err(|e| format!("copying to {}: {e}", bin.display()))?;
    }
    let unit = format!(
        "[Unit]\nDescription=fanctlrs fan control\n\n[Service]\nExecStart={} --daemon\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
        bin.display()
    );
    fs::create_dir_all(unit_file().parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(unit_file(), unit).map_err(|e| e.to_string())?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", "fanctlrs"])
}

pub fn uninstall_service() -> Result<(), String> {
    let _ = systemctl(&["disable", "--now", "fanctlrs"]);
    let _ = fs::remove_file(unit_file());
    let _ = fs::remove_file(installed_binary());
    systemctl(&["daemon-reload"])
}

/// Can this user write fan controls? (Opening a sysfs file for writing
/// without writing changes nothing.)
/// CPU energy counters (RAPL), for the CPU power sensors: (name, counter, wrap-around point in µJ).
pub fn cpu_energy_counters() -> Vec<(String, PathBuf, f64)> {
    let mut dirs: Vec<PathBuf> = fs::read_dir("/sys/class/powercap")
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    dirs.sort();
    dirs.into_iter()
        .filter(|d| d.join("energy_uj").exists())
        .filter_map(|d| {
            let name = fs::read_to_string(d.join("name")).ok()?.trim().to_string();
            Some((name, d.join("energy_uj"), read_num(d.join("max_energy_range_uj"))?))
        })
        .collect()
}

/// The udev rule makes the energy counters readable too. True when there are none.
pub fn cpu_energy_readable() -> bool {
    cpu_energy_counters().iter().all(|(_, path, _)| fs::File::open(path).is_ok())
}

pub fn fans_writable(chips: &[Chip]) -> bool {
    chips.iter().flat_map(|c| (1..=16).map(move |n| fan_file(&c.dir, n, "pwmN_enable"))).filter(|p| p.exists()).any(|p| fs::OpenOptions::new().write(true).open(p).is_ok())
}

/// Motherboard (Super I/O) fan drivers. Unlike GPU drivers these never load
/// on their own, so they have to be loaded and listed for boot.
const SUPERIO_DRIVERS: [&str; 5] = ["it87", "nct6775", "nct6683", "f71882fg", "w83627ehf"];

/// Drivers loaded right now. A Super I/O module only stays loaded if it found its chip.
fn superio_loaded() -> Vec<&'static str> {
    SUPERIO_DRIVERS.into_iter().filter(|m| Path::new("/sys/module").join(m).exists()).collect()
}

/// Any motherboard fan the driver exposes?
pub fn motherboard_fans(chips: &[Chip]) -> bool {
    chips.iter().any(|c| pretty_chip(&c.name) == "Motherboard" && c.dir.join("pwm1").exists())
}

/// Every loaded motherboard fan driver is listed in a modules-load.d file.
pub fn driver_loads_at_boot() -> bool {
    let listed: String = ["/etc/modules-load.d", "/usr/lib/modules-load.d", "/run/modules-load.d"]
        .iter()
        .filter_map(|d| fs::read_dir(d).ok())
        .flat_map(|d| d.flatten())
        .filter_map(|e| fs::read_to_string(e.path()).ok())
        .collect();
    let loaded = superio_loaded();
    !loaded.is_empty() && loaded.iter().all(|m| listed.lines().any(|l| l.trim() == *m))
}

/// Everything that needs root, behind one password prompt (polkit):
/// the udev rule, loading the motherboard fan driver, and loading it at boot.
/// Safe to run again: every step overwrites its own file.
pub fn setup_system() -> Result<(), String> {
    let group = if fs::read_to_string("/etc/group").unwrap_or_default().lines().any(|l| l.starts_with("wheel:")) { "wheel" } else { "sudo" };
    let rule = include_str!("../99-fanctlrs.rules").replace("wheel", group);
    let mut script = String::from("set -e\ncat > /etc/udev/rules.d/99-fanctlrs.rules\nudevadm control --reload\n");
    if superio_loaded().is_empty() {
        // Try each driver; the wrong ones refuse to load ("No such device").
        script += "modprobe nct6775 2>/dev/null || modprobe it87 ignore_resource_conflict=1 2>/dev/null || \
                   modprobe nct6683 2>/dev/null || modprobe f71882fg 2>/dev/null || true\n";
    }
    script += &format!(
        "for m in {}; do if [ -d /sys/module/$m ]; then echo $m; fi; done > /etc/modules-load.d/fanctlrs.conf\n",
        SUPERIO_DRIVERS.join(" ")
    );
    script += "if [ -d /sys/module/it87 ]; then echo 'options it87 ignore_resource_conflict=1' > /etc/modprobe.d/fanctlrs.conf; fi\n";
    script += "udevadm trigger --subsystem-match=hwmon --action=add\n";
    script += "udevadm trigger --subsystem-match=powercap --action=add\n";

    let mut child = Command::new("pkexec")
        .args(["sh", "-c", &script])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("pkexec: {e}"))?;
    child.stdin.take().unwrap().write_all(rule.as_bytes()).map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    if !motherboard_fans(&chips()) {
        return Err("Permissions are set, but no motherboard fan driver works on this board. \
                    Your chip may need an extra driver, see the README."
            .into());
    }
    Ok(())
}

fn linger_file() -> PathBuf {
    let user = std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).unwrap_or_default();
    Path::new("/var/lib/systemd/linger").join(user)
}

/// Linger = the user's services (ours) start at boot instead of at login.
pub fn starts_at_boot() -> bool {
    linger_file().exists()
}

fn loginctl(arg: &str) -> Result<(), String> {
    let out = Command::new("loginctl").arg(arg).output().map_err(|e| format!("loginctl: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

pub fn enable_start_at_boot() -> Result<(), String> {
    loginctl("enable-linger")
}

pub fn disable_start_at_boot() -> Result<(), String> {
    loginctl("disable-linger")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duty_conversion() {
        assert_eq!(pct_to_duty(0.0), 0);
        assert_eq!(pct_to_duty(50.0), 128);
        assert_eq!(pct_to_duty(100.0), 255);
        assert_eq!(pct_to_duty(150.0), 255);
    }

    #[test]
    fn config_round_trip() {
        let kv = parse_kv("# comment\n\nit8665/fan6 = Water Pump\n  amdgpu@0000:0b:00.0/fan1=GPU  \nbroken line\n");
        assert_eq!(kv.len(), 2);
        assert_eq!(kv["it8665/fan6"], "Water Pump");
        // Device paths contain ':' but never '=', so they survive a save/load.
        assert_eq!(parse_kv(&format_kv(kv.iter())), kv);
    }

    #[test]
    fn curve_interpolation() {
        let c = Curve::parse("k10temp@x/temp1 40:30 60:50 80:100").unwrap();
        assert_eq!(c.pct_at(20.0), 30.0);
        assert_eq!(c.pct_at(50.0), 40.0);
        assert_eq!(c.pct_at(70.0), 75.0);
        assert_eq!(c.pct_at(95.0), 100.0);
        assert_eq!(Curve::parse(&c.format()), Some(c));
        assert_eq!(Curve::parse("sensor 40:30"), None);
        assert_eq!(Curve::parse("sensor 40:30 oops"), None);
    }

    #[test]
    fn curve_hysteresis() {
        let c = Curve::parse("s 40:30 60:50").unwrap();
        let mut last = None;
        assert_eq!(curve_target(&c, 50.0, &mut last), 40.0);
        assert_eq!(curve_target(&c, 55.0, &mut last), 45.0); // up: immediately
        assert_eq!(curve_target(&c, 53.0, &mut last), 45.0); // down 2°: hold
        assert_eq!(curve_target(&c, 51.0, &mut last), 41.0); // down 4°: follow
    }

    #[test]
    fn modes() {
        assert_eq!(Mode::parse("auto"), Some(Mode::Auto));
        assert_eq!(Mode::parse("curve"), Some(Mode::Curve));
        assert_eq!(Mode::parse("40"), Some(Mode::Manual(40.0)));
        assert_eq!(Mode::parse("loud"), None);
        assert_eq!(Mode::Manual(40.0).format(), "40");
    }

    #[test]
    fn full_speed_counts_as_manual_100() {
        assert!(!firmware_took_back(Some(1.0), Some(120.0)));
        assert!(!firmware_took_back(Some(0.0), Some(255.0))); // it87: manual 100%
        assert!(firmware_took_back(Some(0.0), Some(120.0)));
        assert!(firmware_took_back(Some(2.0), Some(255.0)));
        assert!(firmware_took_back(None, None));
    }
}
