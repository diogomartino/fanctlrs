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
    let mut aura: Option<Aura> = None;
    let mut aura_sent = (String::new(), std::time::Instant::now());
    let mut anim_error = None;
    let started = std::time::Instant::now();
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
        // Re-sent every time: covers replugging, suspend and controller resets.
        let lights = read_config("lights");
        let (effect, speed) = parse_effect(&lights);
        let frame = Duration::from_secs_f32(1.0 / parse_fps(&lights) as f32);
        let mut animate = None;
        if lights_enabled(&lights) && !light_guide_running() {
            if effect == Effect::Static {
                if let Err(e) = send_frame(&lights_frame(&parse_lights(&lights))) {
                    errors.push(format!("lights: {e}"));
                }
            } else {
                let channels = parse_lights(&lights);
                animate = Some((parse_order(&lights, &channels), channels));
            }
            // The board keeps a static color by itself: only resend when it changes,
            // plus once a minute in case suspend or the BIOS reset it.
            let colors = parse_aura(&lights);
            if !colors.is_empty() && (lights != aura_sent.0 || aura_sent.1.elapsed() > Duration::from_secs(60)) {
                aura = aura.or_else(Aura::find);
                match aura.as_ref().map(|a| a.apply(&colors, effect, speed)) {
                    Some(Err(e)) => {
                        errors.push(format!("motherboard lights: {e}"));
                        aura = None;
                    }
                    _ => aura_sent = (lights.clone(), std::time::Instant::now()),
                }
            }
        }
        errors.extend(anim_error.take());
        // Log each new problem once instead of every 2 seconds.
        let error = errors.join("; ");
        if error != last_error {
            if !error.is_empty() {
                eprintln!("fanctlrs: {error}");
            }
            last_error = error;
        }
        // Nollie effects are drawn here, frame by frame, until the next fan step.
        let next_step = std::time::Instant::now() + Duration::from_secs(2);
        if let Some((order, lights)) = animate {
            match open_nollie() {
                Ok(Some(mut dev)) => {
                    // A stat per frame: the guide takes the lights right away, not up to 2 s later.
                    let mut next_frame = std::time::Instant::now();
                    while std::time::Instant::now() < next_step && !pid_file("guide").exists() {
                        // Wrapped hourly: f32 seconds get choppy after days. Every loop length divides an hour.
                        let t = (started.elapsed().as_secs_f64() % 3600.0) as f32;
                        if let Err(e) = write_frame(&mut dev, &effect_frame(&lights, &order, effect, speed, t)) {
                            anim_error = Some(format!("lights: {e}"));
                            break;
                        }
                        // Paced from the schedule, not from now, so writing a frame doesn't slow the rate.
                        next_frame = (next_frame + frame).max(std::time::Instant::now());
                        thread::sleep(next_frame.saturating_duration_since(std::time::Instant::now()));
                    }
                }
                Ok(None) => {}
                Err(e) => anim_error = Some(format!("lights: {e}")),
            }
        }
        thread::sleep(next_step.saturating_duration_since(std::time::Instant::now()));
    }
}

// --- ARGB lighting (Nollie 16CH) ---------------------------------------------

pub const LIGHT_CHANNELS: usize = 16;
pub const MAX_LEDS: u16 = 256;
/// Channel N as printed on the board -> the channel number the firmware uses (from OpenRGB).
const NOLLIE16_CHANNELS: [u8; LIGHT_CHANNELS] = [19, 18, 17, 16, 24, 25, 26, 27, 20, 21, 22, 23, 31, 30, 29, 28];
/// This channel ends a frame: it's always sent, and last.
const NOLLIE16_LAST: u8 = 31;

/// One ARGB channel: how many LEDs are chained on it, all set to one color.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Light {
    pub leds: u16,
    pub rgb: [u8; 3],
}

impl Light {
    /// `36 ff8800`
    pub fn parse(text: &str) -> Option<Self> {
        let (leds, hex) = text.split_once(' ')?;
        Some(Light { leds: leds.parse::<u16>().ok()?.min(MAX_LEDS), rgb: parse_rgb(hex)? })
    }

    pub fn format(&self) -> String {
        format!("{} {}", self.leds, format_rgb(self.rgb))
    }
}

/// The `lights` file: `enabled = yes` plus `ch1 = 36 ffffff` per channel.
/// Off by default, so fanctlrs leaves the controller alone unless asked.
pub fn lights_enabled(text: &str) -> bool {
    parse_kv(text).get("enabled").is_some_and(|v| v == "yes")
}

pub fn parse_lights(text: &str) -> [Light; LIGHT_CHANNELS] {
    let kv = parse_kv(text);
    std::array::from_fn(|i| kv.get(&format!("ch{}", i + 1)).and_then(|l| Light::parse(l)).unwrap_or_default())
}

fn parse_rgb(hex: &str) -> Option<[u8; 3]> {
    let c = u32::from_str_radix(hex.trim(), 16).ok()?;
    Some([(c >> 16) as u8, (c >> 8) as u8, c as u8])
}

fn format_rgb([r, g, b]: [u8; 3]) -> String {
    format!("{r:02x}{g:02x}{b:02x}")
}

/// Motherboard zones: `aura.85 = ff0000` (zone id in hex).
pub fn parse_aura(text: &str) -> HashMap<u8, [u8; 3]> {
    parse_kv(text)
        .iter()
        .filter_map(|(k, v)| Some((u8::from_str_radix(k.strip_prefix("aura.")?, 16).ok()?, parse_rgb(v)?)))
        .collect()
}

/// The channels sequence effects go through, `order = 16 15 14` (channel numbers).
/// Without the line: every channel in use, in channel order.
pub fn parse_order(text: &str, lights: &[Light; LIGHT_CHANNELS]) -> Vec<usize> {
    let used = |i: &usize| lights.get(*i).is_some_and(|l| l.leds > 0);
    let Some(line) = parse_kv(text).remove("order") else { return (0..LIGHT_CHANNELS).filter(used).collect() };
    let mut order: Vec<usize> = Vec::new();
    for i in line.split_whitespace().filter_map(|n| n.parse::<usize>().ok()?.checked_sub(1)).filter(used) {
        if !order.contains(&i) {
            order.push(i);
        }
    }
    order
}

pub fn format_lights(
    enabled: bool,
    lights: &[Light; LIGHT_CHANNELS],
    aura: &[(u8, [u8; 3])],
    effect: Effect,
    speed: u8,
    fps: u8,
    order: &[usize],
) -> String {
    let mut kv: HashMap<String, String> = lights.iter().enumerate().map(|(i, l)| (format!("ch{}", i + 1), l.format())).collect();
    let order: Vec<String> = order.iter().map(|i| (i + 1).to_string()).collect();
    kv.insert("order".into(), order.join(" "));
    kv.insert("effect".into(), effect.key().into());
    kv.insert("speed".into(), speed.to_string());
    kv.insert("fps".into(), fps.to_string());
    kv.extend(aura.iter().map(|&(zone, rgb)| (format!("aura.{zone:02x}"), format_rgb(rgb))));
    kv.insert("enabled".into(), if enabled { "yes" } else { "no" }.into());
    format_kv(kv.iter())
}

/// One look for every light. Static and Breathing use each light's color.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Effect {
    #[default]
    Static,
    Breathing,
    Cycle,
    Wave,
    /// One fan at a time, along the order.
    Snake,
    /// The next hue on each fan along the order, moving.
    FanRainbow,
    /// Fans light up one by one along the order, then go dark the same way.
    Fill,
    /// One fan travels along the order and back.
    Bounce,
    /// Random fans flash and fade.
    Sparkle,
}

impl Effect {
    pub const ALL: [Effect; 9] = [
        Effect::Static,
        Effect::Breathing,
        Effect::Cycle,
        Effect::Wave,
        Effect::Snake,
        Effect::FanRainbow,
        Effect::Fill,
        Effect::Bounce,
        Effect::Sparkle,
    ];

    /// Goes through the channels in the order the user set.
    pub fn uses_order(self) -> bool {
        matches!(self, Effect::Snake | Effect::FanRainbow | Effect::Fill | Effect::Bounce | Effect::Sparkle)
    }

    pub fn key(self) -> &'static str {
        match self {
            Effect::Static => "static",
            Effect::Breathing => "breathing",
            Effect::Cycle => "cycle",
            Effect::Wave => "wave",
            Effect::Snake => "snake",
            Effect::FanRainbow => "fan-rainbow",
            Effect::Fill => "fill",
            Effect::Bounce => "bounce",
            Effect::Sparkle => "sparkle",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Effect::Static => "Static",
            Effect::Breathing => "Breathing",
            Effect::Cycle => "Color cycle",
            Effect::Wave => "Rainbow wave",
            Effect::Snake => "Snake",
            Effect::FanRainbow => "Fan rainbow",
            Effect::Fill => "Fill",
            Effect::Bounce => "Bounce",
            Effect::Sparkle => "Sparkle",
        }
    }

    /// The ENE chip's built-in mode for the same look.
    /// The chip runs its effects on its own clock, so it can't take part in
    /// the order: during those effects it stays dark (static black).
    fn ene_mode(self) -> u8 {
        match self {
            Effect::Breathing => 2,
            Effect::Cycle => 4,
            Effect::Wave => 5,
            _ => 1,
        }
    }
}

pub const MAX_SPEED: u8 = 5;
/// Frames per second of ARGB effects: smoother costs more CPU (about 0.1% of a core at 15).
pub const MIN_FPS: u8 = 5;
pub const MAX_FPS: u8 = 60;

/// `fps = 30`; 15 if unset.
pub fn parse_fps(text: &str) -> u8 {
    parse_kv(text).get("fps").and_then(|f| f.parse::<u32>().ok()).unwrap_or(15).clamp(MIN_FPS as u32, MAX_FPS as u32) as u8
}

/// `effect = wave`, `speed = 3` (1 to MAX_SPEED).
pub fn parse_effect(text: &str) -> (Effect, u8) {
    let kv = parse_kv(text);
    let effect = kv.get("effect").and_then(|e| Effect::ALL.into_iter().find(|x| x.key() == e)).unwrap_or_default();
    let speed = kv.get("speed").and_then(|s| s.parse::<u32>().ok()).unwrap_or(3).clamp(1, MAX_SPEED as u32) as u8;
    (effect, speed)
}

/// Fully saturated color at hue `h` (0..1).
fn hue(h: f32) -> [u8; 3] {
    let f = |n: f32| {
        let k = (n + h * 6.0) % 6.0;
        ((1.0 - k.min(4.0 - k).clamp(0.0, 1.0)) * 255.0).round() as u8
    };
    [f(5.0), f(3.0), f(1.0)]
}

/// Brightness (0..1) of the fan at place `k` of `n` in the order, for the
/// sequence effects that dim each fan's own color. `steps`: fans' worth of
/// movement since the start (one loop of Snake is `n` steps).
fn sequence_level(effect: Effect, k: f32, n: f32, steps: f32) -> f32 {
    match effect {
        // How many fans behind the head: the head is lit, the one before fades out.
        Effect::Snake => (2.0 - (steps - k).rem_euclid(n)).clamp(0.0, 1.0),
        // n steps filling, n emptying, each fan fading in and out over one step.
        Effect::Fill => {
            let p = steps.rem_euclid(2.0 * n);
            if p < n { (p - k).clamp(0.0, 1.0) } else { 1.0 - (p - n - k).clamp(0.0, 1.0) }
        }
        Effect::Bounce if n <= 1.0 => 1.0,
        // The head goes 0 → n-1 → 0, crossfading between neighbours.
        Effect::Bounce => {
            let span = n - 1.0;
            let p = steps.rem_euclid(2.0 * span);
            let head = if p <= span { p } else { 2.0 * span - p };
            (1.0 - (head - k).abs()).clamp(0.0, 1.0)
        }
        // A new random fan flashes every step and fades over two.
        Effect::Sparkle => {
            let pick = |slot: u64| {
                // splitmix64
                let mut z = slot.wrapping_add(0x9E37_79B9_7F4A_7C15);
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                ((z ^ (z >> 31)) % n as u64) as f32
            };
            let (slot, f) = (steps.floor() as u64, steps.fract());
            let now = if pick(slot) == k { 1.0 - f / 2.0 } else { 0.0 };
            let before = if slot > 0 && pick(slot - 1) == k { 0.5 - f / 2.0 } else { 0.0 };
            f32::max(now, before)
        }
        _ => 1.0,
    }
}

/// The frame at `t` seconds into an effect. One cycle takes 12 s at speed 1, 2.4 s at speed 5.
/// Sequence effects go through `order`; channels not in it keep their color.
pub fn effect_frame(lights: &[Light; LIGHT_CHANNELS], order: &[usize], effect: Effect, speed: u8, t: f32) -> Frame {
    let phase = (t * speed as f32 / 12.0).fract();
    let fans = order.len() as f32;
    std::array::from_fn(|i| {
        let (n, rgb) = (lights[i].leds as usize, lights[i].rgb);
        let place = order.iter().position(|&c| c == i).map(|k| k as f32);
        (0..n)
            .map(|j| {
                led_color(match (effect, place) {
                    (Effect::FanRainbow, Some(k)) => hue((phase + k / fans).fract()),
                    (e, Some(k)) if e.uses_order() => {
                        let b = sequence_level(e, k, fans, t * speed as f32 / 12.0 * fans);
                        rgb.map(|v| (v as f32 * b).round() as u8)
                    }
                    (Effect::Static, _) => rgb,
                    (e, None) if e.uses_order() => rgb,
                    (Effect::Breathing, _) => {
                        let b = 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos();
                        rgb.map(|v| (v as f32 * b).round() as u8)
                    }
                    (Effect::Cycle, _) => hue(phase),
                    (Effect::Wave, _) => hue((phase + j as f32 / n as f32).fract()),
                    _ => unreachable!("every effect is matched above"),
                })
            })
            .collect()
    })
}

/// LEDs are linear: without this, the bit of green in an orange or the
/// 25% blue in a pink look far stronger than on screen.
/// ponytail: one curve for all LEDs, make it per device if one looks off.
const LED_GAMMA: f32 = 2.2;

pub fn led_color(rgb: [u8; 3]) -> [u8; 3] {
    rgb.map(|v| ((v as f32 / 255.0).powf(LED_GAMMA) * 255.0).round() as u8)
}

/// The color of every LED, per channel.
pub type Frame = [Vec<[u8; 3]>; LIGHT_CHANNELS];

pub fn lights_frame(lights: &[Light; LIGHT_CHANNELS]) -> Frame {
    std::array::from_fn(|i| vec![led_color(lights[i].rgb); lights[i].leds as usize])
}

/// Every LED a channel can have, off.
pub fn dark_frame() -> Frame {
    std::array::from_fn(|_| vec![[0; 3]; MAX_LEDS as usize])
}

/// One HID report per channel in use, in firmware channel order:
/// `0, channel, flag, count (BE16), then G R B per LED`.
fn nollie_reports(frame: &Frame) -> Vec<Vec<u8>> {
    let mut used: Vec<(u8, &[[u8; 3]])> = NOLLIE16_CHANNELS
        .into_iter()
        .zip(frame)
        .filter(|(ch, leds)| !leds.is_empty() || *ch == NOLLIE16_LAST)
        .map(|(ch, leds)| (ch, &leds[..leds.len().min(MAX_LEDS as usize)]))
        .collect();
    used.sort_by_key(|&(ch, _)| ch);
    used.into_iter()
        .map(|(ch, leds)| {
            let mut buf = vec![0u8; 1025];
            buf[1] = ch;
            buf[2] = if ch == NOLLIE16_LAST { 2 } else { 0 };
            buf[3..5].copy_from_slice(&(leds.len() as u16).to_be_bytes());
            for (out, &[r, g, b]) in buf[5..].chunks_mut(3).zip(leds) {
                out.copy_from_slice(&[g, r, b]);
            }
            buf
        })
        .collect()
}

fn nollie_hidraw() -> Option<PathBuf> {
    fs::read_dir("/sys/class/hidraw")
        .ok()?
        .flatten()
        .find(|e| fs::read_to_string(e.path().join("device/uevent")).is_ok_and(|u| u.contains("HID_ID=0003:00003061:00004716")))
        .map(|e| Path::new("/dev").join(e.file_name()))
}

pub fn lights_present() -> bool {
    nollie_hidraw().is_some()
}

/// `None` when no controller is plugged in.
fn open_nollie() -> io::Result<Option<fs::File>> {
    let Some(path) = nollie_hidraw() else { return Ok(None) };
    let dev = fs::OpenOptions::new().write(true).open(&path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    Ok(Some(dev))
}

fn write_frame(dev: &mut fs::File, frame: &Frame) -> io::Result<()> {
    for report in nollie_reports(frame) {
        dev.write_all(&report)?;
    }
    Ok(())
}

/// Send the colors. Does nothing when no controller is plugged in.
pub fn send_frame(frame: &Frame) -> io::Result<()> {
    match open_nollie()? {
        Some(mut dev) => write_frame(&mut dev, frame),
        None => Ok(()),
    }
}

/// While the window's channel guide runs, the daemon leaves the lights to it.
pub fn set_light_guide(running: bool) {
    let _ = if running { fs::write(pid_file("guide"), std::process::id().to_string()) } else { fs::remove_file(pid_file("guide")) };
}

fn light_guide_running() -> bool {
    running_cmdline("guide").is_some_and(|cmd| cmd.contains("fanctlrs"))
}

// --- ASUS Aura motherboard LEDs (ENE chip on the SMBus) ----------------------

/// The ENE chip's own registers, reached through a pointer at SMBus register 0x00.
const ENE_NAME: u16 = 0x1000;
const ENE_CONFIG: u16 = 0x1C00;
const ENE_EFFECT_COLORS: u16 = 0x8160;
const ENE_DIRECT: u16 = 0x8020;
const ENE_MODE: u16 = 0x8021;
const ENE_APPLY: u16 = 0x80A0;
const ENE_SPEED: u16 = 0x8022;
/// Second-generation motherboard chips, the ones OpenRGB knows share this layout.
/// ponytail: others (0107, DRAM, GPUs) use other registers, add them from OpenRGB's ENESMBusController.
const ENE_KNOWN: [&[u8]; 2] = [b"AUMA0-E6K5-0105", b"AUMA0-E6K5-0106"];

/// A motherboard's ENE Aura chip.
pub struct Aura {
    dev: fs::File,
    /// The zone each LED belongs to, in LED order.
    pub leds: Vec<u8>,
}

/// One `I2C_SMBUS` ioctl. `data` is the kernel's `i2c_smbus_data` union.
fn smbus(dev: &fs::File, read: bool, command: u8, size: u32, data: &mut [u8; 34]) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Args {
        read_write: u8,
        command: u8,
        size: u32,
        data: *mut u8,
    }
    let mut args = Args { read_write: read as u8, command, size, data: data.as_mut_ptr() };
    // SAFETY: args and data outlive the call, and data is as big as the kernel's union.
    if unsafe { libc::ioctl(dev.as_raw_fd(), 0x0720, &mut args) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

const SMBUS_BYTE_DATA: u32 = 2;
const SMBUS_WORD_DATA: u32 = 3;

impl Aura {
    fn read_byte_data(&self, reg: u8) -> io::Result<u8> {
        let mut data = [0; 34];
        smbus(&self.dev, true, reg, SMBUS_BYTE_DATA, &mut data)?;
        Ok(data[0])
    }

    fn read(&self, reg: u16) -> io::Result<u8> {
        let mut data = [0; 34];
        data[..2].copy_from_slice(&reg.to_be_bytes());
        smbus(&self.dev, false, 0x00, SMBUS_WORD_DATA, &mut data)?;
        self.read_byte_data(0x81)
    }

    fn write(&self, reg: u16, val: u8) -> io::Result<()> {
        let mut data = [0; 34];
        data[..2].copy_from_slice(&reg.to_be_bytes());
        smbus(&self.dev, false, 0x00, SMBUS_WORD_DATA, &mut data)?;
        data[0] = val;
        smbus(&self.dev, false, 0x01, SMBUS_BYTE_DATA, &mut data)
    }

    /// Look on every SMBus where ASUS boards put the chip. Only reads until the
    /// chip has identified itself: the same bus holds the RAM's SPD EEPROMs.
    pub fn find() -> Option<Aura> {
        use std::os::fd::AsRawFd;
        let mut buses: Vec<PathBuf> = fs::read_dir("/sys/class/i2c-dev").ok()?.flatten().map(|e| e.path()).collect();
        buses.sort();
        for bus in buses {
            if !fs::read_to_string(bus.join("name")).is_ok_and(|n| n.starts_with("SMBus")) {
                continue;
            }
            let Ok(dev) = fs::OpenOptions::new().read(true).write(true).open(Path::new("/dev").join(bus.file_name()?)) else { continue };
            for addr in [0x40, 0x4E, 0x4F] {
                // I2C_SLAVE
                if unsafe { libc::ioctl(dev.as_raw_fd(), 0x0703, addr as libc::c_ulong) } < 0 {
                    continue;
                }
                let aura = Aura { dev: dev.try_clone().ok()?, leds: vec![] };
                // ENE chips answer 0..15 on registers 0xA0..0xAF.
                if !(0xA0..0xB0).all(|r| aura.read_byte_data(r).is_ok_and(|v| v == r - 0xA0)) {
                    continue;
                }
                let name: Vec<u8> = (0..15).filter_map(|i| aura.read(ENE_NAME + i).ok()).collect();
                if !ENE_KNOWN.contains(&name.as_slice()) {
                    continue;
                }
                let count = aura.read(ENE_CONFIG + 2).ok()?.min(10);
                let leds = (0..count as u16).map(|i| aura.read(ENE_CONFIG + 0x1B + i)).collect::<io::Result<_>>().ok()?;
                return Some(Aura { leds, ..aura });
            }
        }
        None
    }

    /// Distinct zones, in LED order.
    pub fn zones(&self) -> Vec<u8> {
        let mut zones = self.leds.clone();
        zones.dedup();
        zones
    }

    /// A color per zone and an effect, both run by the chip itself. Zones not listed go white.
    pub fn apply(&self, colors: &HashMap<u8, [u8; 3]>, effect: Effect, speed: u8) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // The register pointer is shared: don't let the window and the daemon interleave.
        unsafe { libc::flock(self.dev.as_raw_fd(), libc::LOCK_EX) };
        let result = (|| {
            let bytes = self.leds.iter().flat_map(|z| {
                let color = colors.get(z).copied().unwrap_or([255; 3]);
                let [r, g, b] = led_color(if effect.uses_order() { [0; 3] } else { color });
                [r, b, g]
            });
            for (i, b) in bytes.enumerate() {
                self.write(ENE_EFFECT_COLORS + i as u16, b)?;
            }
            self.write(ENE_DIRECT, 0)?;
            self.write(ENE_MODE, effect.ene_mode())?;
            // 4 is the chip's slowest, 0 its fastest.
            self.write(ENE_SPEED, MAX_SPEED - speed.clamp(1, MAX_SPEED))?;
            self.write(ENE_APPLY, 1)
        })();
        unsafe { libc::flock(self.dev.as_raw_fd(), libc::LOCK_UN) };
        result
    }
}

pub fn aura_zone_name(zone: u8) -> String {
    match zone {
        0x82 | 0x83 => "Center".into(),
        0x84 => "Audio".into(),
        0x85 => "Back I/O".into(),
        0x86 => "RGB header".into(),
        0x87 => "RGB header 2".into(),
        0x88 => "Backplate".into(),
        0x8A => "DRAM".into(),
        0x8B => "PCIe".into(),
        0x91 => "RGB header 3".into(),
        z => format!("Zone {z:02x}"),
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
    script += "udevadm trigger --subsystem-match=hidraw --action=add\n";

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
    fn nollie_lights() {
        let mut lights = parse_lights("ch1 = 36 ff8000\nch13 = 0 ffffff\nch2 = oops\n");
        assert_eq!(lights[0], Light { leds: 36, rgb: [0xff, 0x80, 0] });
        assert_eq!(lights[1], Light::default());
        assert!(!lights_enabled("ch1 = 36 ff8000"));
        let text = format_lights(true, &lights, &[(0x85, [255, 0, 0])], Effect::Wave, 4, 30, &[0]);
        assert_eq!((parse_fps(&text), parse_fps(""), parse_fps("fps = 500")), (30, 15, MAX_FPS));
        assert_eq!(parse_effect(&text), (Effect::Wave, 4));
        assert_eq!(parse_order(&text, &lights), [0]);
        assert_eq!(parse_effect("effect = disco\nspeed = 99"), (Effect::Static, MAX_SPEED));
        assert!(lights_enabled(&text));
        assert_eq!(parse_lights(&text), lights);
        assert_eq!(parse_aura(&text)[&0x85], [255, 0, 0]);
        // Gamma: pure colors stay, the faint parts of a mix get much fainter.
        assert_eq!(led_color([255, 0, 0x44]), [255, 0, 14]);
        lights[4] = Light { leds: 12, rgb: [255, 0, 255] };
        let reports = nollie_reports(&lights_frame(&lights));
        // ch1 -> 19, ch5 -> 24, and the frame-ending 31 (ch13) even though it's empty.
        assert_eq!(reports.iter().map(|r| r[1]).collect::<Vec<_>>(), [19, 24, 31]);
        assert_eq!(reports[0].len(), 1025);
        assert_eq!(&reports[0][2..8], &[0, 0, 36, 0x38, 0xff, 0]); // count, then G R B (gamma-corrected)
        assert_eq!(&reports[1][5 + 11 * 3..5 + 12 * 3 + 1], &[0, 255, 255, 0]); // last LED (G R B), then nothing
        assert_eq!(&reports[2][2..5], &[2, 0, 0]);
    }

    #[test]
    fn effects() {
        assert_eq!([hue(0.0), hue(1.0 / 3.0), hue(2.0 / 3.0)], [[255, 0, 0], [0, 255, 0], [0, 0, 255]]);
        let mut lights = [Light::default(); LIGHT_CHANNELS];
        lights[0] = Light { leds: 4, rgb: [255, 0, 0] };
        let at = |effect, t| effect_frame(&lights, &[0], effect, 1, t)[0].clone();
        assert_eq!(at(Effect::Static, 5.0), vec![[255, 0, 0]; 4]);
        assert_eq!(at(Effect::Breathing, 0.0)[0], [0, 0, 0]); // dark at the start of a breath
        assert_eq!(at(Effect::Breathing, 6.0)[0], [255, 0, 0]); // full halfway through
        assert_eq!(at(Effect::Cycle, 4.0), vec![[0, 255, 0]; 4]); // a third of the way: green
        assert_eq!(at(Effect::Wave, 0.0)[2], [0, 255, 255]); // LEDs spread over the hues
        assert!(effect_frame(&lights, &[0], Effect::Wave, 1, 0.0)[1].is_empty()); // unused channels stay empty
    }

    #[test]
    fn sequence_effects() {
        let mut lights = [Light::default(); LIGHT_CHANNELS];
        for i in [0, 1, 2, 5] {
            lights[i] = Light { leds: 2, rgb: [255, 0, 0] };
        }
        // Order without the line: channels in use. With it: as written, unused and repeats dropped.
        assert_eq!(parse_order("", &lights), [0, 1, 2, 5]);
        assert_eq!(parse_order("order = 3 1 3 9 2", &lights), [2, 0, 1]);
        assert_eq!(parse_order("order =", &lights), Vec::<usize>::new());
        let order = [2, 0, 1]; // channel 6 (index 5) isn't in it
        // Speed 1, 3 fans: 12 s per loop, 4 s per fan.
        let lit = |t| effect_frame(&lights, &order, Effect::Snake, 1, t).map(|ch| ch.first().map_or(0, |c| c[0]));
        // Head on the first in the order (channel 3); the last one (channel 2) still fading from the loop before.
        assert_eq!(lit(1.0)[..3], [0, 135, 255]);
        assert_eq!(lit(5.0)[..3], [255, 0, 135]); // head moved to channel 1, channel 3 fading
        assert_eq!(lit(6.0)[2], 56); // channel 3 half way out (gamma-corrected)
        assert_eq!(lit(9.0)[..3], [135, 255, 0]); // head on channel 2, channel 1 fading, channel 3 out
        assert_eq!(lit(5.0)[5], 255); // not in the order: keeps its color
        // 3 fans: Fill is 6 steps (3 on, 3 off), Bounce 4 (0 → 2 → 0).
        let level = |e, steps| [0.0, 1.0, 2.0].map(|k| sequence_level(e, k, 3.0, steps));
        assert_eq!(level(Effect::Fill, 1.5), [1.0, 0.5, 0.0]); // filling, second fan half on
        assert_eq!(level(Effect::Fill, 3.0), [1.0, 1.0, 1.0]);
        assert_eq!(level(Effect::Fill, 4.5), [0.0, 0.5, 1.0]); // emptying from the first
        assert_eq!(level(Effect::Fill, 6.0), [0.0, 0.0, 0.0]);
        assert_eq!(level(Effect::Bounce, 1.0), [0.0, 1.0, 0.0]); // on the way out
        assert_eq!(level(Effect::Bounce, 2.5), [0.0, 0.5, 0.5]); // turned at the end, half way back
        for steps in [0.0, 0.3, 7.9, 1234.5] {
            let lit = level(Effect::Sparkle, steps);
            assert!(lit.iter().filter(|b| **b > 0.0).count() <= 2 && lit.iter().all(|b| (0.0..=1.0).contains(b)));
        }
        let rainbow = effect_frame(&lights, &order, Effect::FanRainbow, 1, 0.0);
        assert_eq!([rainbow[2][0], rainbow[0][0], rainbow[1][0]], [[255, 0, 0], [0, 255, 0], [0, 0, 255]]);
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
