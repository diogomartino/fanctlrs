mod core;

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt, Theme,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState},
    h_flex,
    input::{Input, InputEvent, InputState},
    popover::Popover,
    scroll::ScrollableElement,
    slider::{Slider, SliderEvent, SliderState},
    switch::Switch,
    v_flex,
};
use gpui_kit::*;

use crate::core::{
    AUTO_SEED_DUTY, Chip, Curve, Effect, LIGHT_CHANNELS, Light, MAX_LEDS, Mode, chips, config_dir, curve_key, daemon_running, enforce,
    fan_file, fans_writable, format_kv, parse_kv, pct_to_duty, read_config, read_num,
};

/// `editing` key prefix for a light channel's LED count, `leds:3`.
const LEDS: &str = "leds:";

/// The channel-by-channel lighting setup: one channel lit at a time.
struct Guide {
    channel: usize,
    name: Entity<InputState>,
    leds: Entity<InputState>,
    _subs: Vec<Subscription>,
}

/// The guide's channel: all white until a count is typed, then that many
/// green and the rest red, so the count is right once nothing is red.
fn guide_frame(channel: usize, leds: Option<u16>) -> core::Frame {
    let mut frame = core::dark_frame();
    frame[channel] = (0..MAX_LEDS)
        .map(|j| match leds {
            None | Some(0) => [255, 255, 255],
            Some(n) if j < n => [0, 255, 0],
            Some(_) => [255, 0, 0],
        })
        .collect();
    frame
}

/// What to send to the lights; `None` leaves that device alone.
#[derive(Default)]
struct LightJob {
    frame: Option<core::Frame>,
    aura: Option<(HashMap<u8, [u8; 3]>, Effect, u8)>,
}

/// Sends lights off the UI thread: a color slider fires a change per pixel dragged,
/// and the motherboard chip takes tens of ms per update. Only the latest job is
/// sent, and a device only when its colors changed. Errors land in the returned slot.
fn spawn_light_sender(aura: Option<core::Aura>) -> (mpsc::Sender<LightJob>, Arc<Mutex<Option<String>>>) {
    let (tx, rx) = mpsc::channel::<LightJob>();
    let error = Arc::new(Mutex::new(None));
    let slot = error.clone();
    std::thread::spawn(move || {
        let (mut sent_frame, mut sent_aura) = (None, None);
        while let Ok(mut job) = rx.recv() {
            for later in rx.try_iter() {
                job.frame = later.frame.or(job.frame);
                job.aura = later.aura.or(job.aura);
            }
            let mut result = Ok(());
            if job.frame.is_some() && job.frame != sent_frame {
                result = core::send_frame(job.frame.as_ref().unwrap()).map_err(|e| format!("Couldn't set the lights: {e}"));
                sent_frame = job.frame;
            }
            if let (Some(a), Some(job)) = (&aura, job.aura) {
                if !job.0.is_empty() && Some(&job) != sent_aura.as_ref() {
                    result = result.and(a.apply(&job.0, job.1, job.2).map_err(|e| format!("Couldn't set the motherboard lights: {e}")));
                    sent_aura = Some(job);
                }
            }
            if let Err(e) = result {
                *slot.lock().unwrap() = Some(format!("{e}. Use Fix under Setup."));
                // Try again next time, e.g. after Fix.
                (sent_frame, sent_aura) = (None, None);
            }
        }
    });
    (tx, error)
}

/// What a color picker sets.
#[derive(Clone, Copy)]
enum LightTarget {
    All,
    Channel(usize),
    /// Index into `aura_colors`.
    Aura(usize),
}

/// Pure colors for the picker: the theme's are softened for screens,
/// and its "red" comes out pink on LEDs.
fn led_presets() -> Vec<Hsla> {
    [0xff0000, 0xff4000, 0xff9000, 0xffff00, 0x00ff00, 0x00ffff, 0x0000ff, 0x8000ff, 0xff00ff, 0xffffff]
        .into_iter()
        .map(|c| rgb(c).into())
        .collect()
}

fn rgb_to_hsla([r, g, b]: [u8; 3]) -> Hsla {
    rgb(u32::from_be_bytes([0, r, g, b])).into()
}

fn hsla_to_rgb(c: Hsla) -> [u8; 3] {
    let c = Rgba::from(c);
    [c.r, c.g, c.b].map(|v| (v * 255.0).round() as u8)
}

/// `editing` key for the "new profile" name field.
const NEW_PROFILE: &str = "profile:new";

/// Fans the profile lists must match; fans it doesn't list (or that are gone) don't matter.
fn matches_profile(current: &HashMap<String, String>, profile: &HashMap<String, String>) -> bool {
    !profile.is_empty() && profile.iter().all(|(k, v)| current.get(k).is_none_or(|c| c == v))
}

fn list_profiles() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(config_dir().join("profiles"))
        .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Temp,
    Fan,
    Volt,
    Curr,
    Power,
    /// Clock speed, raw in Hz.
    Freq,
    Percent,
    /// Memory, raw in bytes.
    Bytes,
}

impl Kind {
    fn icon(self, t: &Theme) -> (assets::IconName, Hsla) {
        match self {
            Kind::Temp => (assets::IconName::Thermometer, t.danger),
            Kind::Fan => (assets::IconName::Fan, t.info),
            Kind::Volt => (assets::IconName::Zap, t.warning),
            Kind::Curr => (assets::IconName::Activity, t.success),
            Kind::Power => (assets::IconName::Plug, t.chart_4),
            Kind::Freq => (assets::IconName::Gauge, t.chart_2),
            Kind::Percent => (assets::IconName::ChartNoAxesColumn, t.chart_3),
            Kind::Bytes => (assets::IconName::MemoryStick, t.chart_5),
        }
    }

    fn format(self, raw: f64) -> String {
        match self {
            Kind::Temp => format!("{:.1} °C", raw / 1000.0),
            Kind::Fan => format!("{raw:.0} RPM"),
            Kind::Volt => format!("{:.2} V", raw / 1000.0),
            Kind::Curr => format!("{:.2} A", raw / 1000.0),
            Kind::Power => format!("{:.1} W", raw / 1_000_000.0),
            Kind::Freq => format!("{:.0} MHz", raw / 1_000_000.0),
            Kind::Percent => format!("{raw:.0} %"),
            Kind::Bytes => format!("{:.2} GiB", raw / 1024f64.powi(3)),
        }
    }
}

/// "temp1_input" -> (Temp, 1, "temp1"). amdgpu reports power as "power1_average".
fn parse_sensor_file(file: &str) -> Option<(Kind, u32, &str)> {
    let base = file.strip_suffix("_input").or_else(|| {
        file.strip_suffix("_average").filter(|b| b.starts_with("power"))
    })?;
    let digits = base.find(|c: char| c.is_ascii_digit())?;
    let kind = match &base[..digits] {
        "temp" => Kind::Temp,
        "fan" => Kind::Fan,
        "in" => Kind::Volt,
        "curr" => Kind::Curr,
        "power" => Kind::Power,
        "freq" => Kind::Freq,
        _ => return None,
    };
    Some((kind, base[digits..].parse().ok()?, base))
}

/// Default label: hand-written `chip/sensor` name > driver label > raw name.
/// In-app renames (keyed `chip@device/sensor`) override this at display time.
fn default_label(chip: &Chip, base: &str, names: &HashMap<String, String>) -> String {
    names
        .get(&format!("{}/{base}", chip.name))
        .cloned()
        .or_else(|| fs::read_to_string(chip.dir.join(format!("{base}_label"))).ok().map(|l| l.trim().to_string()))
        .map(|l| match l.as_str() {
            // amdgpu's clock labels
            "sclk" => "Core clock".to_string(),
            "mclk" => "VRAM clock".to_string(),
            _ => l,
        })
        .unwrap_or_else(|| base.to_string())
}

/// Busy and used time from the `cpu` line of /proc/stat: (idle, total).
fn cpu_times(stat: &str) -> Option<(u64, u64)> {
    let fields: Vec<u64> = stat.lines().next()?.strip_prefix("cpu ")?.split_whitespace().take(8).map(|f| f.parse().ok()).collect::<Option<_>>()?;
    // user nice system idle iowait irq softirq steal
    Some((fields[3] + fields[4], fields.iter().sum()))
}

/// CPU load % since the previous sample, from /proc/stat.
fn cpu_load(prev: &mut Option<(u64, u64)>) -> Option<f64> {
    let now = cpu_times(&fs::read_to_string("/proc/stat").ok()?)?;
    let load = prev.and_then(|(idle, total)| {
        let (didle, dtotal) = (now.0.saturating_sub(idle), now.1.saturating_sub(total));
        (dtotal > 0).then(|| 100.0 * (1.0 - didle as f64 / dtotal as f64))
    });
    *prev = Some(now);
    load
}

/// RAM in use (total - available), in bytes, from /proc/meminfo.
fn ram_used(meminfo: &str) -> Option<f64> {
    let kb = |key: &str| meminfo.lines().find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse::<f64>().ok());
    Some((kb("MemTotal:")? - kb("MemAvailable:")?) * 1024.0)
}

/// Average power in µW between two readings of an energy counter (µJ), which
/// wraps around to 0 after `max`.
fn energy_power(prev: f64, now: f64, max: f64, secs: f64) -> Option<f64> {
    let used = if now >= prev { now - prev } else { now + max - prev };
    (secs > 0.0).then(|| used / secs)
}

/// How a sensor's value is obtained.
#[derive(Clone, Copy)]
enum Read {
    /// An energy counter in µJ at `path` that wraps at this value; shown as power.
    Energy(f64),
    /// A number in `path`, times this factor (cpufreq reports kHz, Kind::Freq wants Hz).
    File(f64),
    RamUsed,
    CpuLoad,
}

struct Sensor {
    key: String,
    default: String,
    kind: Kind,
    path: PathBuf,
    read: Read,
    value: Option<f64>,
    /// Last energy reading and when, for `Read::Energy`.
    prev: Option<(f64, Instant)>,
    /// Exists but this user may not read it (CPU energy before Fix).
    denied: bool,
}

impl Sensor {
    fn file(key: String, default: String, kind: Kind, path: PathBuf, scale: f64) -> Self {
        Sensor { key, default, kind, path, read: Read::File(scale), value: None, prev: None, denied: false }
    }

    fn computed(key: &str, default: &str, kind: Kind, read: Read) -> Self {
        Sensor { key: key.into(), default: default.into(), kind, path: PathBuf::new(), read, value: None, prev: None, denied: false }
    }
}

struct Group {
    key: String,
    default: String,
    sensors: Vec<Sensor>,
}

fn discover_sensors(chips: &[Chip], names: &HashMap<String, String>) -> Vec<Group> {
    let mut groups = chips
        .iter()
        .filter_map(|chip| {
            let mut sensors: Vec<(u32, Sensor)> = fs::read_dir(&chip.dir)
                .ok()?
                .flatten()
                .filter_map(|e| {
                    let file = e.file_name().to_string_lossy().into_owned();
                    let (kind, n, base) = parse_sensor_file(&file)?;
                    let sensor = Sensor::file(format!("{}/{base}", chip.key), default_label(chip, base, names), kind, e.path(), 1.0);
                    Some((kind as u32 * 1000 + n, sensor))
                })
                .collect();
            // amdgpu keeps load and VRAM numbers next to the device, not in hwmon.
            if chip.name == "amdgpu" {
                let extras = [
                    ("gpu_busy_percent", "GPU load", Kind::Percent),
                    ("mem_busy_percent", "VRAM load", Kind::Percent),
                    ("mem_info_vram_used", "VRAM used", Kind::Bytes),
                ];
                for (i, (file, label, kind)) in extras.into_iter().enumerate() {
                    let path = chip.dir.join("device").join(file);
                    if path.exists() {
                        let key = format!("{}/{file}", chip.key);
                        let label = names.get(&format!("{}/{file}", chip.name)).cloned().unwrap_or(label.into());
                        sensors.push((kind as u32 * 1000 + i as u32, Sensor::file(key, label, kind, path, 1.0)));
                    }
                }
            }
            if sensors.is_empty() {
                return None;
            }
            sensors.sort_by_key(|s| s.0);
            Some(Group {
                key: chip.key.clone(),
                default: chip.title.clone(),
                sensors: sensors.into_iter().map(|s| s.1).collect(),
            })
        })
        .collect::<Vec<_>>();

    // Everything about the CPU in one group: load and power first, then the
    // driver's temperatures, then per-thread clocks. Keys keep their old
    // "system/…" and "cpufreq/…" names so hidden sensors and renames carry over.
    let cpu = match groups.iter().position(|g| ["k10temp", "zenpower", "coretemp"].iter().any(|c| g.key.starts_with(c))) {
        Some(i) => i,
        None => {
            groups.push(Group { key: "cpu".into(), default: "CPU".into(), sensors: Vec::new() });
            groups.len() - 1
        }
    };
    let mut first = vec![Sensor::computed("system/cpu_load", "CPU load", Kind::Percent, Read::CpuLoad)];
    for (name, path, max) in core::cpu_energy_counters() {
        let (file, label) = match name.as_str() {
            "package-0" => ("cpu_package_power", "CPU power"),
            "core" => ("cpu_core_power", "CPU cores power"),
            _ => continue,
        };
        let key = format!("system/{file}");
        if first.iter().all(|s| s.key != key) {
            first.push(Sensor { read: Read::Energy(max), ..Sensor::file(key, label.into(), Kind::Power, path, 1.0) });
        }
    }
    groups[cpu].sensors.splice(0..0, first);

    let mut threads: Vec<u32> = fs::read_dir("/sys/devices/system/cpu")
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.strip_prefix("cpu")?.parse().ok()).collect())
        .unwrap_or_default();
    threads.sort();
    groups[cpu].sensors.extend(
        threads
            .into_iter()
            .map(|n| (n, PathBuf::from(format!("/sys/devices/system/cpu/cpu{n}/cpufreq/scaling_cur_freq"))))
            .filter(|(_, path)| path.exists())
            .map(|(n, path)| Sensor::file(format!("cpufreq/cpu{n}"), format!("CPU {n} clock"), Kind::Freq, path, 1000.0)),
    );

    groups.push(Group {
        key: "system".into(),
        default: "Memory".into(),
        sensors: vec![Sensor::computed("system/ram_used", "RAM used", Kind::Bytes, Read::RamUsed)],
    });

    // CPU, GPU, memory, then the rest in hardware order (the sort is stable).
    let rank = |key: &str| match key.split('@').next().unwrap_or(key) {
        "k10temp" | "zenpower" | "coretemp" | "cpu" => 0,
        "amdgpu" | "nouveau" => 1,
        "system" => 2,
        _ => 3,
    };
    groups.sort_by_key(|g| rank(&g.key));
    groups
}

struct Fan {
    /// `chip@device/pwmN`, for show/hide and saved speed.
    key: String,
    /// `chip@device/fanN`: renaming the card renames its RPM sensor too.
    name_key: String,
    default: String,
    chip_key: String,
    chip_default: String,
    dir: PathBuf,
    n: u32,
    slider: Entity<SliderState>,
    /// One slider per curve point (the % at that temperature).
    point_sliders: Vec<Entity<SliderState>>,
    rpm: f64,
    duty: f64,
    /// What the user asked for.
    mode: Mode,
    /// Kept while in another mode, so switching back restores it.
    curve: Curve,
    /// Hysteresis state for the curve.
    last: Option<(f32, f32)>,
    /// Times the firmware took the fan back and we re-applied the setting.
    reapplied: u32,
}

impl Fan {
    fn path(&self, file: &str) -> PathBuf {
        fan_file(&self.dir, self.n, file)
    }

    fn write(&self, file: &str, val: u8) -> io::Result<()> {
        fs::write(self.path(file), val.to_string())
    }
}

#[derive(Default)]
struct Service {
    installed: bool,
    running: bool,
    /// Whether this user may write fan controls (the udev rule is in place).
    writable: bool,
    /// Whether this user may read the CPU energy counters (same udev rule).
    energy_readable: bool,
    motherboard_fans: bool,
    driver_at_boot: bool,
    starts_at_boot: bool,
    busy: bool,
    message: Option<String>,
}

/// Should the setup screen open? Not if everything is set up. After "Skip for now"
/// (the file lists what was working then), only if one of those broke since.
fn needs_onboarding(items: &[(&str, bool)], skipped: Option<&str>) -> bool {
    if items.iter().all(|(_, ok)| *ok) {
        return false;
    }
    match skipped {
        None => true,
        Some(was_ok) => items.iter().any(|(name, ok)| !ok && was_ok.lines().any(|l| l == *name)),
    }
}

impl Service {
    fn items(&self) -> [(&'static str, bool); 4] {
        [
            ("driver", self.motherboard_fans),
            ("boot", self.driver_at_boot),
            ("permission", self.writable && self.energy_readable),
            ("service", self.installed),
        ]
    }

    /// Cheap enough to run after every setup action, too slow for every tick.
    fn check(&mut self) {
        let chips = chips();
        self.installed = core::service_installed();
        self.running = daemon_running();
        self.writable = fans_writable(&chips);
        self.energy_readable = core::cpu_energy_readable();
        self.motherboard_fans = core::motherboard_fans(&chips);
        self.driver_at_boot = core::driver_loads_at_boot();
        self.starts_at_boot = core::starts_at_boot();
    }
}

struct FanControl {
    groups: Vec<Group>,
    /// Previous /proc/stat sample for the CPU load sensor.
    cpu_prev: Option<(u64, u64)>,
    fans: Vec<Fan>,
    names: HashMap<String, String>,
    hidden: HashSet<String>,
    profiles: Vec<String>,
    lights: [Light; LIGHT_CHANNELS],
    light_pickers: Vec<Entity<ColorPickerState>>,
    /// Sets every channel at once.
    all_lights_picker: Entity<ColorPickerState>,
    lights_present: bool,
    lights_enabled: bool,
    effect: Effect,
    speed: u8,
    /// Channels Snake and Fan rainbow go through, in order.
    order: Vec<usize>,
    speed_slider: Entity<SliderState>,
    fps: u8,
    fps_slider: Entity<SliderState>,
    light_tx: mpsc::Sender<LightJob>,
    light_error: Arc<Mutex<Option<String>>>,
    /// Per motherboard zone; `None` until the user picks one, so the board keeps its own effect.
    aura_colors: Vec<(u8, Option<[u8; 3]>)>,
    aura_pickers: Vec<Entity<ColorPickerState>>,
    guide: Option<Guide>,
    service: Service,
    editing: Option<(String, Entity<InputState>)>,
    error: Option<String>,
    _edit_sub: Option<Subscription>,
    _subs: Vec<Subscription>,
    _poll: Task<()>,
    /// Showing the setup screen instead of the dashboard.
    onboarding: bool,
}

impl FanControl {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let chips = chips();
        let names = parse_kv(&read_config("names"));
        let groups = discover_sensors(&chips, &names);

        // Curves follow the CPU by default.
        let temps: Vec<&Sensor> = groups.iter().flat_map(|g| &g.sensors).filter(|s| s.kind == Kind::Temp).collect();
        let default_sensor = temps
            .iter()
            .find(|s| ["k10temp", "zenpower", "coretemp"].iter().any(|c| s.key.starts_with(c)))
            .or(temps.first())
            .map_or(String::new(), |s| s.key.clone());

        // Every `pwmN` with a `pwmN_enable` is a fan or pump the kernel lets us drive.
        let mut subs = Vec::new();
        let mut fans = Vec::new();
        for chip in &chips {
            for n in (1..=16).filter(|n| chip.dir.join(format!("pwm{n}_enable")).exists()) {
                let i = fans.len();
                let slider = cx.new(|_| SliderState::new().min(0.0).max(100.0).step(1.0));
                subs.push(cx.subscribe(&slider, move |this, _, ev: &SliderEvent, cx| match ev {
                    SliderEvent::Change(v) => {
                        this.set_manual(i, v.start());
                        cx.notify();
                    }
                    SliderEvent::Release(_) => this.save_fans(),
                }));
                let curve = Curve::new(default_sensor.clone());
                let point_sliders: Vec<_> = curve
                    .points
                    .iter()
                    .enumerate()
                    .map(|(j, &(_, pct))| {
                        let s = cx.new(|_| SliderState::new().min(0.0).max(100.0).step(1.0).default_value(pct));
                        subs.push(cx.subscribe(&s, move |this, _, ev: &SliderEvent, cx| match ev {
                            SliderEvent::Change(v) => {
                                this.fans[i].curve.points[j].1 = v.start();
                                cx.notify();
                            }
                            SliderEvent::Release(_) => this.save_fans(),
                        }));
                        s
                    })
                    .collect();
                let default = names
                    .get(&format!("{}/fan{n}", chip.name))
                    .cloned()
                    .unwrap_or_else(|| format!("{} fan {n}", chip.title));
                fans.push(Fan {
                    key: format!("{}/pwm{n}", chip.key),
                    name_key: format!("{}/fan{n}", chip.key),
                    default,
                    chip_key: chip.key.clone(),
                    chip_default: chip.title.clone(),
                    dir: chip.dir.clone(),
                    n,
                    slider,
                    point_sliders,
                    rpm: 0.0,
                    duty: 0.0,
                    mode: Mode::Auto,
                    curve,
                    last: None,
                    reapplied: 0,
                });
            }
        }

        let lights_text = read_config("lights");
        let lights = core::parse_lights(&lights_text);
        let aura = core::Aura::find();
        let saved_aura = core::parse_aura(&lights_text);
        let aura_colors: Vec<(u8, Option<[u8; 3]>)> =
            aura.as_ref().map_or(vec![], |a| a.zones().into_iter().map(|z| (z, saved_aura.get(&z).copied())).collect());
        let (light_tx, light_error) = spawn_light_sender(aura);
        let (effect, speed) = core::parse_effect(&lights_text);
        let speed_slider = cx.new(|_| SliderState::new().min(1.0).max(core::MAX_SPEED as f32).step(1.0).default_value(speed as f32));
        subs.push(cx.subscribe(&speed_slider, |this, _, ev: &SliderEvent, cx| match ev {
            SliderEvent::Change(v) => {
                this.speed = v.start() as u8;
                cx.notify();
            }
            SliderEvent::Release(_) => this.save_lights(),
        }));
        let fps = core::parse_fps(&lights_text);
        let fps_slider =
            cx.new(|_| SliderState::new().min(core::MIN_FPS as f32).max(core::MAX_FPS as f32).step(5.0).default_value(fps as f32));
        subs.push(cx.subscribe(&fps_slider, |this, _, ev: &SliderEvent, cx| match ev {
            SliderEvent::Change(v) => {
                this.fps = v.start() as u8;
                cx.notify();
            }
            SliderEvent::Release(_) => this.save_lights(),
        }));
        let mut picker = |color: Option<[u8; 3]>, target: LightTarget, subs: &mut Vec<Subscription>| {
            let state = cx.new(|cx| {
                let s = ColorPickerState::new(window, cx);
                if let Some(c) = color { s.default_value(rgb_to_hsla(c)) } else { s }
            });
            subs.push(cx.subscribe(&state, move |this, _, ev: &ColorPickerEvent, cx| {
                if let ColorPickerEvent::Change(Some(c)) = ev {
                    this.set_light_color(target, hsla_to_rgb(*c));
                    cx.notify();
                }
            }));
            state
        };
        let light_pickers = (0..LIGHT_CHANNELS).map(|i| picker(Some(lights[i].rgb), LightTarget::Channel(i), &mut subs)).collect();
        let aura_pickers = aura_colors.iter().enumerate().map(|(i, (_, c))| picker(*c, LightTarget::Aura(i), &mut subs)).collect();
        let all_lights_picker = picker(None, LightTarget::All, &mut subs);

        let poll = cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let alive = this.update_in(cx, |this, window, cx| {
                this.refresh(window, cx);
                cx.notify();
            });
            if alive.is_err() {
                break;
            }
        });

        let mut this = Self {
            groups,
            cpu_prev: None,
            fans,
            names,
            hidden: read_config("hidden-sensors").lines().map(String::from).collect(),
            profiles: list_profiles(),
            lights,
            light_pickers,
            all_lights_picker,
            lights_present: core::lights_present() || !aura_colors.is_empty(),
            light_tx,
            light_error,
            effect,
            speed,
            order: core::parse_order(&lights_text, &lights),
            speed_slider,
            fps,
            fps_slider,
            aura_colors,
            aura_pickers,
            lights_enabled: core::lights_enabled(&lights_text),
            guide: None,
            service: Service::default(),
            editing: None,
            error: core::running_as_root().then(|| {
                "Running as root: settings get saved as root and the background service can't be installed. \
                 Close this and start fanctlrs as your user; use Fix under Setup for fan permissions."
                    .to_string()
            }),
            _edit_sub: None,
            _subs: subs,
            _poll: poll,
            onboarding: false,
        };
        // Adopt whatever manual speeds are already set, then restore the last session.
        for f in &mut this.fans {
            let pct = (read_num(f.path("pwmN")).unwrap_or(0.0) * 100.0 / 255.0).round() as f32;
            if read_num(f.path("pwmN_enable")) == Some(1.0) {
                f.mode = Mode::Manual(pct);
            }
            f.slider.update(cx, |s, cx| s.set_value(pct, window, cx));
        }
        this.service.check();
        let skipped = fs::read_to_string(config_dir().join("setup-skipped")).ok();
        this.onboarding = needs_onboarding(&this.service.items(), skipped.as_deref());
        this.apply_state(&parse_kv(&read_config("fans")), window, cx);
        this.refresh(window, cx);
        // After a reboot the daemon does this; without it, opening the window does.
        if this.lights_enabled && !this.service.running {
            this.save_lights();
        }
        this
    }

    fn send_lights(&mut self, frame: core::Frame) {
        let _ = self.light_tx.send(LightJob { frame: Some(frame), aura: None });
    }

    /// The motherboard's zones the user picked a color for. `dark`: all off instead.
    fn send_aura(&mut self, dark: bool) {
        let colors = self.aura_colors.iter().filter_map(|&(z, c)| Some((z, if dark { [0; 3] } else { c? }))).collect();
        let (effect, speed) = if dark { (Effect::Static, 1) } else { (self.effect, self.speed) };
        let _ = self.light_tx.send(LightJob { frame: None, aura: Some((colors, effect, speed)) });
    }

    fn save_lights(&mut self) {
        if self.lights_enabled {
            // Nollie effects are animated by the background service.
            if self.effect == Effect::Static {
                self.send_lights(core::lights_frame(&self.lights));
            }
            self.send_aura(false);
        }
        let aura: Vec<(u8, [u8; 3])> = self.aura_colors.iter().filter_map(|&(z, c)| Some((z, c?))).collect();
        self.save("lights", core::format_lights(self.lights_enabled, &self.lights, &aura, self.effect, self.speed, self.fps, &self.order));
    }

    /// Off turns every LED off and leaves the controllers alone from then on.
    fn set_lights_enabled(&mut self, on: bool) {
        if !on {
            self.send_lights(core::dark_frame());
            self.send_aura(true);
        }
        self.lights_enabled = on;
        self.save_lights();
    }

    fn start_guide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        core::set_light_guide(true);
        self.guide_step(0, window, cx);
    }

    fn guide_step(&mut self, channel: usize, window: &mut Window, cx: &mut Context<Self>) {
        let key = format!("light:ch{}", channel + 1);
        let name_text = self.names.get(&key).cloned().unwrap_or_default();
        let leds_text = match self.lights[channel].leds {
            0 => String::new(),
            n => n.to_string(),
        };
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("e.g. Front fans").default_value(name_text));
        let leds = cx.new(|cx| InputState::new(window, cx).placeholder("LEDs").default_value(leds_text));
        name.update(cx, |s, cx| s.focus(window, cx));
        let on_input = |this: &mut Self, ev: &InputEvent, window: &mut Window, cx: &mut Context<Self>| match ev {
            InputEvent::Change => this.guide_preview(cx),
            InputEvent::PressEnter { .. } => this.guide_next(false, window, cx),
            _ => {}
        };
        let subs = vec![
            cx.subscribe_in(&name, window, move |this, _, ev, window, cx| on_input(this, ev, window, cx)),
            cx.subscribe_in(&leds, window, move |this, _, ev, window, cx| on_input(this, ev, window, cx)),
        ];
        self.guide = Some(Guide { channel, name, leds, _subs: subs });
        self.guide_preview(cx);
        cx.notify();
    }

    /// The LED count typed in the guide, if it's a number.
    fn guide_leds(&self, cx: &App) -> Option<u16> {
        let g = self.guide.as_ref()?;
        g.leds.read(cx).value().trim().parse::<u16>().ok().map(|n| n.min(MAX_LEDS))
    }

    fn guide_preview(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.guide.as_ref().map(|g| g.channel) else { return };
        self.send_lights(guide_frame(channel, self.guide_leds(cx)));
        cx.notify();
    }

    /// Keep what was typed for the current channel. `empty`: nothing's plugged in.
    fn guide_keep(&mut self, empty: bool, cx: &mut Context<Self>) {
        let Some(g) = &self.guide else { return };
        let (channel, name) = (g.channel, g.name.read(cx).value().trim().to_string());
        self.lights[channel].leds = if empty { 0 } else { self.guide_leds(cx).unwrap_or(0) };
        // A default white instead of black, so a newly found channel lights up.
        if self.lights[channel].leds > 0 && self.lights[channel].rgb == [0; 3] {
            self.lights[channel].rgb = [255; 3];
        }
        self.set_name(format!("light:ch{}", channel + 1), name);
    }

    fn guide_next(&mut self, empty: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.guide.as_ref().map(|g| g.channel) else { return };
        self.guide_keep(empty, cx);
        if channel + 1 < LIGHT_CHANNELS {
            self.guide_step(channel + 1, window, cx);
        } else {
            self.finish_guide(cx);
        }
    }

    fn guide_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.guide.as_ref().map(|g| g.channel) else { return };
        self.guide_keep(false, cx);
        self.guide_step(channel.saturating_sub(1), window, cx);
    }

    fn finish_guide(&mut self, cx: &mut Context<Self>) {
        self.guide = None;
        core::set_light_guide(false);
        self.save_lights();
        cx.notify();
    }

    fn set_light_color(&mut self, target: LightTarget, color: [u8; 3]) {
        match target {
            LightTarget::Channel(i) => self.lights[i].rgb = color,
            LightTarget::Aura(i) => self.aura_colors[i].1 = Some(color),
            LightTarget::All => {
                self.lights.iter_mut().for_each(|l| l.rgb = color);
                self.aura_colors.iter_mut().for_each(|z| z.1 = Some(color));
            }
        }
        self.save_lights();
    }

    /// Apply `fan key -> mode` (+ `key#curve`) from the `fans` file or a profile.
    /// Fans not listed are left alone.
    fn apply_state(&mut self, state: &HashMap<String, String>, window: &mut Window, cx: &mut Context<Self>) {
        for i in 0..self.fans.len() {
            let key = self.fans[i].key.clone();
            // The editor has a fixed number of points.
            if let Some(curve) = state.get(&curve_key(&key)).and_then(|c| Curve::parse(c)) {
                if curve.points.len() == self.fans[i].point_sliders.len() {
                    for (s, &(_, pct)) in self.fans[i].point_sliders.iter().zip(&curve.points) {
                        s.update(cx, |s, cx| s.set_value(pct, window, cx));
                    }
                    self.fans[i].curve = curve;
                }
            }
            let hw_manual = read_num(self.fans[i].path("pwmN_enable")) == Some(1.0);
            match state.get(&key).and_then(|m| Mode::parse(m)) {
                // Already in firmware control: nothing to do (and no root needed).
                Some(Mode::Auto) if self.fans[i].mode == Mode::Auto && !hw_manual => {}
                Some(Mode::Auto) => self.set_auto(i),
                Some(Mode::Manual(pct)) => {
                    self.set_manual(i, pct);
                    self.fans[i].slider.update(cx, |s, cx| s.set_value(pct, window, cx));
                }
                Some(Mode::Curve) => {
                    self.fans[i].mode = Mode::Curve;
                    self.fans[i].last = None;
                }
                None => {}
            }
        }
    }

    fn fan_state(&self) -> HashMap<String, String> {
        self.fans
            .iter()
            .flat_map(|f| [(f.key.clone(), f.mode.format()), (curve_key(&f.key), f.curve.format())])
            .collect()
    }

    fn sensor_temp(&self, key: &str) -> Option<f32> {
        let s = self.groups.iter().flat_map(|g| &g.sensors).find(|s| s.key == key)?;
        read_num(&s.path).map(|t| t as f32 / 1000.0)
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.light_error.lock().unwrap().take() {
            self.error = Some(e);
        }
        for s in self.groups.iter_mut().flat_map(|g| &mut g.sensors) {
            if !self.hidden.contains(&s.key) {
                s.value = match s.read {
                    Read::File(scale) => read_num(&s.path).map(|v| v * scale),
                    Read::RamUsed => fs::read_to_string("/proc/meminfo").ok().and_then(|m| ram_used(&m)),
                    Read::CpuLoad => cpu_load(&mut self.cpu_prev),
                    Read::Energy(max) => {
                        s.denied = fs::File::open(&s.path).is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied);
                        let now = read_num(&s.path).map(|uj| (uj, Instant::now()));
                        let power = s.prev.zip(now).and_then(|((p, pt), (n, nt))| energy_power(p, n, max, (nt - pt).as_secs_f64()));
                        s.prev = now;
                        power
                    }
                };
            }
        }
        self.service.installed = core::service_installed();
        self.service.running = daemon_running();
        let temps: Vec<Option<f32>> = self.fans.iter().map(|f| self.sensor_temp(&f.curve.sensor)).collect();
        let mut error = None;
        for (f, temp) in self.fans.iter_mut().zip(temps) {
            // While the daemon runs it owns fan control; two controllers would fight.
            if !self.service.running {
                match enforce(&f.dir, f.n, &f.mode, Some(&f.curve), temp, &mut f.last) {
                    Ok(true) => f.reapplied += 1,
                    Ok(false) => {}
                    Err(e) => error = Some(format!("Couldn't set fan speed: {e}. Use Fix under Setup.")),
                }
            }
            f.rpm = read_num(f.path("fanN_input")).unwrap_or(0.0);
            f.duty = read_num(f.path("pwmN")).unwrap_or(0.0);
            // Unless manual, let the slider follow what's actually applied.
            if !matches!(f.mode, Mode::Manual(_)) {
                let pct = (f.duty * 100.0 / 255.0).round() as f32;
                f.slider.update(cx, |s, cx| s.set_value(pct, window, cx));
            }
        }
        if error.is_some() {
            self.error = error;
        }
    }

    fn display<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.names.get(key).map_or(default, String::as_str)
    }

    fn save(&mut self, file: &str, contents: String) {
        let path = config_dir().join(file);
        if let Err(e) = fs::create_dir_all(path.parent().unwrap()).and_then(|_| fs::write(&path, contents)) {
            self.error = Some(format!("Couldn't save {}: {e}", path.display()));
        }
    }

    /// The daemon reads this file, so save as soon as a fan's mode changes.
    fn save_fans(&mut self) {
        let state = self.fan_state();
        self.save("fans", format_kv(state.iter()));
    }

    fn save_profile(&mut self, name: &str) {
        let name = name.replace('/', "-");
        let state = self.fan_state();
        self.save(&format!("profiles/{name}"), format_kv(state.iter()));
        self.profiles = list_profiles();
    }

    fn load_profile(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let state = parse_kv(&read_config(&format!("profiles/{name}")));
        self.apply_state(&state, window, cx);
        self.save_fans();
    }

    fn delete_profile(&mut self, name: &str) {
        if let Err(e) = fs::remove_file(config_dir().join("profiles").join(name)) {
            self.error = Some(format!("Couldn't delete profile {name}: {e}"));
        }
        self.profiles = list_profiles();
    }

    fn profile_active(&self, name: &str) -> bool {
        matches_profile(&self.fan_state(), &parse_kv(&read_config(&format!("profiles/{name}"))))
    }

    fn toggle(&mut self, key: &str, show: bool, cx: &mut Context<Self>) {
        if show {
            self.hidden.remove(key);
        } else {
            self.hidden.insert(key.to_string());
        }
        let mut keys: Vec<_> = self.hidden.iter().cloned().collect();
        keys.sort();
        self.save("hidden-sensors", keys.join("\n"));
        cx.notify();
    }

    fn start_edit(&mut self, key: String, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(cx);
        let state = cx.new(|cx| InputState::new(window, cx).default_value(text));
        state.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        self._edit_sub = Some(cx.subscribe_in(&state, window, |this, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_edit(cx);
            }
        }));
        self.editing = Some((key, state));
        cx.notify();
    }

    /// Empty name = back to the default.
    fn commit_edit(&mut self, cx: &mut Context<Self>) {
        let Some((key, state)) = self.editing.take() else { return };
        self._edit_sub = None;
        let name = state.read(cx).value().trim().to_string();
        if key == NEW_PROFILE {
            if !name.is_empty() {
                self.save_profile(&name);
            }
            cx.notify();
            return;
        }
        if let Some(i) = key.strip_prefix(LEDS).and_then(|i| i.parse::<usize>().ok()) {
            if let Ok(n) = name.parse::<u16>() {
                self.lights[i].leds = n.min(MAX_LEDS);
                self.save_lights();
            }
            cx.notify();
            return;
        }
        self.set_name(key, name);
        cx.notify();
    }

    /// Empty name = back to the default.
    fn set_name(&mut self, key: String, name: String) {
        if name.is_empty() {
            self.names.remove(&key);
        } else {
            self.names.insert(key, name);
        }
        let contents = "# chip/sensor = Name, or chip@device/sensor = Name (written by the app)\n".to_string()
            + &format_kv(self.names.iter());
        self.save("names", contents);
    }

    /// A name that turns into a text field on double-click.
    fn renamable(&self, key: &str, default: &str, cx: &Context<Self>) -> AnyElement {
        if let Some((k, state)) = &self.editing {
            if k == key {
                return Input::new(state).small().into_any_element();
            }
        }
        let text = self.display(key, default).to_string();
        let key = key.to_string();
        div()
            .truncate()
            .child(text.clone())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    if ev.click_count == 2 {
                        this.start_edit(key.clone(), text.clone(), window, cx);
                    }
                }),
            )
            .into_any_element()
    }

    fn report(&mut self, i: usize, r: io::Result<()>) {
        let name = self.display(&self.fans[i].name_key, &self.fans[i].default).to_string();
        self.error = r.err().map(|e| format!("{name}: {e}. Use Fix under Setup."));
    }

    fn set_manual(&mut self, i: usize, pct: f32) {
        let f = &self.fans[i];
        let r = f.write("pwmN_enable", 1).and_then(|_| f.write("pwmN", pct_to_duty(pct)));
        if r.is_ok() {
            let was_manual = matches!(self.fans[i].mode, Mode::Manual(_));
            self.fans[i].mode = Mode::Manual(pct.clamp(0.0, 100.0).round());
            self.fans[i].duty = pct_to_duty(pct) as f64;
            // Tell the daemon right away, or it keeps running the old curve until release.
            if !was_manual {
                self.save_fans();
            }
        }
        self.report(i, r);
    }

    fn set_auto(&mut self, i: usize) {
        let f = &self.fans[i];
        // Some drivers (amdgpu) refuse a duty write outside manual mode; that's fine.
        let _ = f.write("pwmN", AUTO_SEED_DUTY);
        let r = f.write("pwmN_enable", 2);
        if r.is_ok() {
            self.fans[i].mode = Mode::Auto;
        }
        self.report(i, r);
    }

    fn set_curve(&mut self, i: usize) {
        self.fans[i].mode = Mode::Curve;
        self.fans[i].last = None;
        self.save_fans();
    }

    /// Move a curve point's temperature, keeping points in order and 5° apart.
    fn bump_temp(&mut self, i: usize, j: usize, delta: f32) {
        let points = &mut self.fans[i].curve.points;
        let lo = if j == 0 { 0.0 } else { points[j - 1].0 + 5.0 };
        let hi = points.get(j + 1).map_or(120.0, |p| p.0 - 5.0);
        points[j].0 = (points[j].0 + delta).clamp(lo, hi);
        self.fans[i].last = None;
        self.save_fans();
    }

    fn set_curve_sensor(&mut self, i: usize, key: String) {
        self.fans[i].curve.sensor = key;
        self.fans[i].last = None;
        self.save_fans();
    }

    /// (key, "Device · Sensor") for every temperature sensor.
    fn temp_sensors(&self) -> Vec<(String, String)> {
        self.groups
            .iter()
            .flat_map(|g| {
                let group = self.display(&g.key, &g.default);
                g.sensors
                    .iter()
                    .filter(|s| s.kind == Kind::Temp)
                    .map(move |s| (s.key.clone(), format!("{group} · {}", self.display(&s.key, &s.default))))
            })
            .collect()
    }

    fn run_service_task(&mut self, task: fn() -> Result<(), String>, cx: &mut Context<Self>) {
        self.service.busy = true;
        self.service.message = None;
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { task() }).await;
            let _ = this.update(cx, |this, cx| {
                this.service.busy = false;
                this.service.check();
                // Fans are found at startup; a driver loaded just now needs a restart.
                let listed = this.fans.iter().any(|f| f.chip_default == "Motherboard");
                this.service.message = result.err().or_else(|| {
                    (this.service.motherboard_fans && !listed).then(|| "Motherboard fans found. Reopen the app to control them.".into())
                });
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// (group title, [(key, label)]) for the show/hide popover.
    fn picker_entries(&self, fans: bool) -> Vec<(String, Vec<(String, String)>)> {
        if !fans {
            return self
                .groups
                .iter()
                .map(|g| {
                    let items = g.sensors.iter().map(|s| (s.key.clone(), self.display(&s.key, &s.default).to_string()));
                    (self.display(&g.key, &g.default).to_string(), items.collect())
                })
                .collect();
        }
        let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for f in &self.fans {
            let title = self.display(&f.chip_key, &f.chip_default).to_string();
            let entry = (f.key.clone(), self.display(&f.name_key, &f.default).to_string());
            match out.last_mut() {
                Some((t, items)) if *t == title => items.push(entry),
                _ => out.push((title, vec![entry])),
            }
        }
        out
    }

    fn picker(&self, id: &'static str, fans: bool, cx: &Context<Self>) -> Popover {
        let app = cx.entity();
        Popover::new(id)
            .anchor(Anchor::TopRight)
            .trigger(Button::new(id).icon(IconName::Eye).label("Show / hide").outline().small())
            .content(move |_, _, cx| {
                let this = app.read(cx);
                let groups = this.picker_entries(fans).into_iter().map(|(title, items)| {
                    v_flex()
                        .gap_1p5()
                        .child(div().text_xs().font_semibold().text_color(cx.theme().muted_foreground).child(title))
                        .children(items.into_iter().map(|(key, label)| {
                            let app = app.clone();
                            Checkbox::new(SharedString::from(key.clone()))
                                .label(label)
                                .checked(!this.hidden.contains(&key))
                                .on_change(move |show, _, cx| app.update(cx, |this, cx| this.toggle(&key, *show, cx)))
                        }))
                });
                div().w(px(280.)).max_h(px(460.)).child(
                    v_flex().id("picker-list").gap_4().p_1().overflow_y_scrollbar().children(groups),
                )
            })
    }

    fn section_header(&self, key: &str, default: &str, picker: Option<Popover>, cx: &Context<Self>) -> impl IntoElement {
        h_flex()
            .items_center()
            .gap_3()
            .child(div().flex_1().min_w_0().text_lg().font_semibold().child(self.renamable(key, default, cx)))
            .children(picker)
    }

    fn card(t: &Theme) -> Div {
        v_flex().px_3().py_2().gap_1().text_sm().rounded(t.radius_lg).border_1().border_color(t.border).bg(t.secondary)
    }

    fn sensors_section(&self, cx: &Context<Self>) -> AnyElement {
        let t = cx.theme();

        let groups = self.groups.iter().filter_map(|g| {
            let visible: Vec<_> = g.sensors.iter().filter(|s| !self.hidden.contains(&s.key)).collect();
            if visible.is_empty() {
                return None;
            }
            Some(
                Self::card(t)
                    .p_3()
                    .child(
                        div()
                            .pb_1()
                            .text_xs()
                            .font_semibold()
                            .text_color(t.muted_foreground)
                            .child(self.renamable(&g.key, &g.default, cx)),
                    )
                    .children(visible.into_iter().map(|s| {
                        let (icon, color) = s.kind.icon(t);
                        h_flex()
                            .gap_2()
                            .child(Icon::new(icon).small().text_color(color))
                            .child(div().flex_1().min_w_0().child(self.renamable(&s.key, &s.default, cx)))
                            .child(match s.value {
                                Some(v) => div().font_semibold().child(s.kind.format(v)),
                                None if s.denied => div().text_color(t.warning).child("needs Fix"),
                                None => div().font_semibold().child("—"),
                            })
                    })),
            )
        });

        v_flex()
            .gap_4()
            .child(self.section_header(
                "section:sensors",
                "Sensors",
                Some(self.picker("pick-sensors", false, cx)),
                cx,
            ))
            .child(v_flex().gap_3().children(groups))
            .into_any_element()
    }

    fn profiles_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let profiles = self.profiles.iter().enumerate().map(|(i, name)| {
            let (load, delete) = (name.clone(), name.clone());
            let button = Button::new(("profile", i)).label(name.clone()).small();
            let button = if self.profile_active(name) { button.primary() } else { button.outline() };
            h_flex()
                .child(button.on_click(cx.listener(move |this, _, window, cx| {
                    this.load_profile(&load, window, cx);
                    cx.notify();
                })))
                .child(Button::new(("profile-delete", i)).icon(IconName::Close).ghost().xsmall().on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.delete_profile(&delete);
                        cx.notify();
                    }),
                ))
        });
        let save = match &self.editing {
            Some((k, state)) if k == NEW_PROFILE => div().w(px(170.)).child(Input::new(state).small()).into_any_element(),
            _ => Button::new("profile-save")
                .icon(IconName::Plus)
                .label("Save profile")
                .ghost()
                .small()
                .on_click(cx.listener(|this, _, window, cx| this.start_edit(NEW_PROFILE.into(), String::new(), window, cx)))
                .into_any_element(),
        };
        h_flex().flex_wrap().items_center().gap_2().children(profiles).child(save)
    }

    fn curve_editor(&self, i: usize, f: &Fan, cx: &Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let app = cx.entity();
        let sensor_label = self
            .temp_sensors()
            .into_iter()
            .find(|(k, _)| *k == f.curve.sensor)
            .map_or("Pick a sensor".to_string(), |(_, label)| label);
        let temp = self.sensor_temp(&f.curve.sensor).map_or("—".into(), |t| format!("{t:.0} °C"));

        let sensor_picker = Popover::new(("curve-sensor", i))
            .anchor(Anchor::TopLeft)
            .trigger(Button::new(("curve-sensor-button", i)).label(sensor_label).outline().xsmall())
            .content(move |_, _, cx| {
                let sensors = app.read(cx).temp_sensors();
                let app = app.clone();
                div().w(px(260.)).max_h(px(320.)).child(
                    v_flex().id(("curve-sensor-list", i)).overflow_y_scrollbar().children(sensors.into_iter().map(|(key, label)| {
                        let app = app.clone();
                        Button::new(SharedString::from(format!("curve-{i}-{key}"))).label(label).ghost().small().on_click(
                            move |_, _, cx| app.update(cx, |this, cx| {
                                this.set_curve_sensor(i, key.clone());
                                cx.notify();
                            }),
                        )
                    })),
                )
            });

        let points = f.curve.points.iter().zip(&f.point_sliders).enumerate().map(|(j, (&(temp, pct), slider))| {
            h_flex()
                .gap_1()
                .items_center()
                .child(Button::new(("curve-down", i * 16 + j)).icon(IconName::Minus).ghost().xsmall().on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.bump_temp(i, j, -5.0);
                        cx.notify();
                    }),
                ))
                .child(div().w(px(42.)).text_center().child(format!("{temp:.0}°C")))
                .child(Button::new(("curve-up", i * 16 + j)).icon(IconName::Plus).ghost().xsmall().on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.bump_temp(i, j, 5.0);
                        cx.notify();
                    }),
                ))
                .child(Slider::new(slider).flex_1())
                .child(div().w(px(40.)).text_right().child(format!("{pct:.0}%")))
        });

        v_flex()
            .pt_1()
            .gap_1()
            .border_t_1()
            .border_color(t.border)
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(div().text_color(t.muted_foreground).child("Follows"))
                    .child(sensor_picker)
                    .child(div().flex_1())
                    .child(div().font_semibold().child(temp)),
            )
            .children(points)
    }

    fn fans_section(&self, cx: &Context<Self>) -> AnyElement {
        let t = cx.theme();
        let cards = self.fans.iter().enumerate().filter(|(_, f)| !self.hidden.contains(&f.key)).map(|(i, f)| {
            let (badge, badge_bg) = match f.mode {
                Mode::Auto => ("AUTO", t.success),
                Mode::Manual(_) => ("MANUAL", t.warning),
                Mode::Curve => ("CURVE", t.info),
            };
            let curve_button = Button::new(("curve", i)).label("Curve").small();
            let curve_button = if f.mode == Mode::Curve { curve_button.primary() } else { curve_button.ghost() };
            let auto_button = Button::new(("auto", i)).label("Auto").small();
            let auto_button = if f.mode == Mode::Auto { auto_button.primary() } else { auto_button.ghost() };
            Self::card(t)
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(assets::IconName::Fan).small().text_color(t.info))
                        .child(div().min_w_0().font_semibold().child(self.renamable(&f.name_key, &f.default, cx)))
                        .child(
                            div()
                                .px_1p5()
                                .rounded_full()
                                .text_xs()
                                .font_semibold()
                                .bg(badge_bg.opacity(0.18))
                                .text_color(badge_bg)
                                .child(badge),
                        )
                        // Shows how often the firmware grabbed the fan back.
                        .children((f.reapplied > 0).then(|| {
                            h_flex()
                                .gap_0p5()
                                .text_xs()
                                .text_color(t.muted_foreground)
                                .child(Icon::new(IconName::RefreshCw).xsmall())
                                .child(f.reapplied.to_string())
                        }))
                        .child(div().flex_1())
                        .child(div().font_semibold().child(format!("{:.0} RPM", f.rpm))),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Slider::new(&f.slider).flex_1())
                        .child(div().w(px(40.)).text_right().font_semibold().child(format!("{:.0}%", f.duty * 100.0 / 255.0)))
                        .child(curve_button.on_click(cx.listener(move |this, _, _, cx| {
                            this.set_curve(i);
                            cx.notify();
                        })))
                        .child(auto_button.on_click(cx.listener(move |this, _, _, cx| {
                            this.set_auto(i);
                            this.save_fans();
                            cx.notify();
                        }))),
                )
                .children((f.mode == Mode::Curve).then(|| self.curve_editor(i, f, cx)))
        });

        let empty = self.fans.is_empty().then(|| {
            div()
                .text_sm()
                .text_color(t.muted_foreground)
                .child("No controllable fans found. Your board's fan driver is probably not loaded, see the README.")
        });

        v_flex()
            .gap_4()
            .child(self.section_header(
                "section:fans",
                "Fans & pumps",
                Some(self.picker("pick-fans", true, cx)),
                cx,
            ))
            .children(empty)
            .child(self.profiles_bar(cx))
            .child(v_flex().gap_2().children(cards))
            .into_any_element()
    }

    fn lights_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let app = cx.entity();
        let switch = Switch::new("lights-enabled")
            .checked(self.lights_enabled)
            .on_change(move |on, _, cx| app.update(cx, |this, cx| {
                this.set_lights_enabled(*on);
                cx.notify();
            }));
        let header = |extra: Vec<AnyElement>, cx: &Context<Self>| {
            h_flex()
                .gap_3()
                .child(div().flex_1().min_w_0().text_lg().font_semibold().child(self.renamable("section:lights", "Lighting", cx)))
                .children(extra)
        };
        let muted = cx.theme().muted_foreground;
        if !self.lights_enabled {
            return v_flex()
                .gap_4()
                .child(header(vec![switch.into_any_element()], cx))
                .child(div().text_sm().text_color(muted).child(
                    "ARGB controller found. Turn this on to set its colors from here. While it's off, fanctlrs leaves the controller alone.",
                ))
                .into_any_element();
        }
        if let Some(g) = &self.guide {
            let t = cx.theme();
            let n = g.channel + 1;
            let hint = match self.guide_leds(cx) {
                None | Some(0) => format!(
                    "Channel {n} is lit white, everything else is off. Nothing lit? Then nothing's plugged in here. \
                     Otherwise type its LED count: that many LEDs turn green and any beyond it red."
                ),
                Some(_) => "Raise the count until nothing is red. A fan usually has 8 to 16 LEDs; add them up for chained fans.".into(),
            };
            let guide = Self::card(t)
                .p_3()
                .gap_3()
                .child(
                    h_flex()
                        .child(div().flex_1().font_semibold().child(format!("Channel {n} of {LIGHT_CHANNELS}")))
                        .child(Button::new("guide-cancel").label("Done").ghost().small().on_click(
                            cx.listener(|this, _, _, cx| {
                                this.guide_keep(false, cx);
                                this.finish_guide(cx);
                            }),
                        )),
                )
                .child(div().text_color(t.muted_foreground).child(hint))
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&g.name).small()))
                        .child(div().w(px(72.)).child(Input::new(&g.leds).small())),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new("guide-back").label("Back").ghost().small().disabled(g.channel == 0).on_click(
                            cx.listener(|this, _, window, cx| this.guide_back(window, cx)),
                        ))
                        .child(div().flex_1())
                        .child(Button::new("guide-empty").label("Nothing plugged in").outline().small().on_click(
                            cx.listener(|this, _, window, cx| this.guide_next(true, window, cx)),
                        ))
                        .child(
                            Button::new("guide-next")
                                .label(if n == LIGHT_CHANNELS { "Finish" } else { "Next" })
                                .primary()
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| this.guide_next(false, window, cx))),
                        ),
                );
            return v_flex().gap_4().child(header(vec![switch.into_any_element()], cx)).child(guide).into_any_element();
        }
        // Pickers follow the saved colors (e.g. after "All"); setting them emits no change.
        // The "all" picker shows the color shared by everything in use, if they agree.
        let mut used = self.lights.iter().filter(|l| l.leds > 0).map(|l| l.rgb).chain(self.aura_colors.iter().filter_map(|z| z.1));
        let shared = used.next().filter(|first| used.all(|c| c == *first));
        let pickers = self.light_pickers.iter().zip(self.lights.iter().map(|l| Some(l.rgb)));
        let pickers = pickers.chain(self.aura_pickers.iter().zip(self.aura_colors.iter().map(|z| z.1)));
        for (picker, color) in pickers.chain([(&self.all_lights_picker, shared)]) {
            if let Some(c) = color.filter(|&c| picker.read(cx).value().map(hsla_to_rgb) != Some(c)) {
                picker.update(cx, |s, cx| s.set_value(rgb_to_hsla(c), window, cx));
            }
        }
        let t = cx.theme();
        let configured = self.lights.iter().any(|l| l.leds > 0);
        let rows = self.lights.iter().zip(&self.light_pickers).enumerate().filter(|(_, (l, _))| l.leds > 0).map(|(i, (l, picker))| {
            let key = format!("light:ch{}", i + 1);
            let edit_key = format!("{LEDS}{i}");
            let count = match &self.editing {
                Some((k, state)) if *k == edit_key => div().w(px(64.)).child(Input::new(state).small()).into_any_element(),
                _ => {
                    let text = l.leds.to_string();
                    Button::new(("leds", i))
                        .label(format!("{} LEDs", l.leds))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(move |this, _, window, cx| this.start_edit(edit_key.clone(), text.clone(), window, cx)))
                        .into_any_element()
                }
            };
            h_flex()
                .gap_2()
                .min_h(px(28.))
                .opacity(if l.leds == 0 { 0.55 } else { 1.0 })
                .child(ColorPicker::new(picker).featured_colors(led_presets()).small())
                .child(div().flex_1().min_w_0().child(self.renamable(&key, &format!("Channel {}", i + 1), cx)))
                .child(count)
        });
        v_flex()
            .gap_4()
            .child(header(
                vec![
                    div().text_sm().text_color(t.muted_foreground).child("All").into_any_element(),
                    ColorPicker::new(&self.all_lights_picker).featured_colors(led_presets()).small().into_any_element(),
                    switch.into_any_element(),
                ],
                cx,
            ))
            .child(div().text_sm().text_color(t.muted_foreground).child(if configured {
                "Close OpenRGB, or the two will fight over the colors."
            } else {
                "Run the setup guide to find which channels have something plugged in."
            }))
            .child(self.effect_row(cx))
            .children(configured.then(|| Self::card(t).py_2().children(rows)))
            .children((!self.aura_colors.is_empty()).then(|| {
                let zones = self.aura_colors.iter().zip(&self.aura_pickers).map(|(&(zone, _), picker)| {
                    h_flex()
                        .gap_2()
                        .min_h(px(28.))
                        .child(ColorPicker::new(picker).featured_colors(led_presets()).small())
                        .child(div().flex_1().min_w_0().child(self.renamable(&format!("aura:{zone:02x}"), &core::aura_zone_name(zone), cx)))
                });
                Self::card(t)
                    .py_2()
                    .child(div().pb_1().text_xs().font_semibold().text_color(t.muted_foreground).child("Motherboard"))
                    .children(zones)
            }))
            .child(
                h_flex().child(
                    Button::new("guide-start")
                        .icon(IconName::Settings)
                        .label("Channel setup guide")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| this.start_guide(window, cx))),
                ),
            )
            .into_any_element()
    }

    /// The order list, shown in the Order popover. Runs in the popover's
    /// context, so clicks go through `app` like the show/hide pickers.
    fn order_list(&self, app: &Entity<Self>, t: &Theme) -> Div {
        let name = |i: usize| self.display(&format!("light:ch{}", i + 1), &format!("Channel {}", i + 1)).to_string();
        // Channels emptied by the guide since stay saved but out of the way.
        let order: Vec<usize> = self.order.iter().copied().filter(|&i| self.lights[i].leds > 0).collect();
        let last = order.len().saturating_sub(1);
        let act = |f: fn(&mut Self, usize, usize), pos: usize, i: usize| {
            let app = app.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                app.update(cx, |this, cx| {
                    f(this, pos, i);
                    this.save_lights();
                    cx.notify();
                })
            }
        };
        let rows = order.iter().enumerate().map(|(pos, &i)| {
            let saved = self.order.iter().position(|&c| c == i).unwrap_or(pos);
            h_flex()
                .gap_1()
                .min_h(px(28.))
                .child(div().w(px(20.)).text_color(t.muted_foreground).child(format!("{}", pos + 1)))
                .child(div().flex_1().min_w_0().truncate().child(name(i)))
                .child(Button::new(("order-up", i)).icon(IconName::ChevronUp).ghost().xsmall().disabled(pos == 0).on_click(act(
                    |this, saved, _| this.order.swap(saved, saved - 1),
                    saved,
                    i,
                )))
                .child(Button::new(("order-down", i)).icon(IconName::ChevronDown).ghost().xsmall().disabled(pos == last).on_click(
                    act(|this, saved, _| this.order.swap(saved, saved + 1), saved, i),
                ))
                .child(Button::new(("order-remove", i)).icon(IconName::Close).ghost().xsmall().on_click(act(
                    |this, _, i| this.order.retain(|&c| c != i),
                    saved,
                    i,
                )))
        });
        let left_out = (0..LIGHT_CHANNELS).filter(|i| self.lights[*i].leds > 0 && !order.contains(i)).map(|i| {
            h_flex()
                .gap_1()
                .min_h(px(28.))
                .text_color(t.muted_foreground)
                .child(div().w(px(20.)))
                .child(div().flex_1().min_w_0().truncate().child(name(i)))
                .child(Button::new(("order-add", i)).icon(IconName::Plus).label("Add").ghost().xsmall().on_click(act(
                    |this, _, i| {
                        this.order.retain(|&c| c != i);
                        this.order.push(i);
                    },
                    0,
                    i,
                )))
        });
        v_flex()
            .w(px(300.))
            .gap_0p5()
            .text_sm()
            .child(div().pb_2().text_color(t.muted_foreground).child(
                "The effect goes from top to bottom, then starts over. Lights left out keep their color.",
            ))
            .children(rows)
            .children(left_out)
    }

    fn effect_row(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let buttons = Effect::ALL.into_iter().enumerate().map(|(i, effect)| {
            let b = Button::new(("effect", i)).label(effect.label()).small();
            let b = if self.effect == effect { b.primary() } else { b.ghost() };
            b.on_click(cx.listener(move |this, _, _, cx| {
                this.effect = effect;
                this.save_lights();
                cx.notify();
            }))
        });
        let animated = self.effect != Effect::Static;
        let app = cx.entity();
        let order = self.effect.uses_order().then(|| {
            Popover::new("order")
                .anchor(Anchor::TopRight)
                .trigger(Button::new("order-button").icon(assets::IconName::ListOrdered).label("Order").outline().small())
                .content(move |_, _, cx| app.read(cx).order_list(&app, cx.theme()))
        });
        v_flex()
            .gap_2()
            .child(h_flex().flex_wrap().gap_1().children(buttons))
            .children(animated.then(|| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().text_color(t.muted_foreground).child("Speed"))
                    .child(Slider::new(&self.speed_slider).flex_1())
                    .child(div().w(px(20.)).text_right().text_sm().child(self.speed.to_string()))
                    .children(order)
            }))
            .children(animated.then(|| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().text_color(t.muted_foreground).child("Smoothness"))
                    .child(Slider::new(&self.fps_slider).flex_1())
                    .child(div().w(px(48.)).text_right().text_sm().child(format!("{} fps", self.fps)))
            }))
            .children((animated && !self.service.running).then(|| {
                div()
                    .text_sm()
                    .text_color(t.warning)
                    .child("Effects on the ARGB controller run in the background service: install it under Setup.")
            }))
    }

    fn finish_onboarding(&mut self) {
        let items = self.service.items();
        if items.iter().all(|(_, ok)| *ok) {
            let _ = fs::remove_file(config_dir().join("setup-skipped"));
        } else {
            let was_ok: Vec<&str> = items.iter().filter(|(_, ok)| *ok).map(|(name, _)| *name).collect();
            self.save("setup-skipped", was_ok.join("\n"));
        }
        self.onboarding = false;
    }

    fn onboarding_view(&self, cx: &Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let done = self.service.items().iter().all(|(_, ok)| *ok);
        let next = Button::new("onboarding-next").label(if done { "Continue" } else { "Skip for now" });
        let next = if done { next.primary() } else { next.outline() };
        v_flex()
            .gap_6()
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_2xl().font_semibold().child("Welcome to fanctlrs"))
                    .child(div().text_sm().text_color(t.muted_foreground).child(
                        "A few things need setting up before fanctlrs can control your fans. Fix does the system \
                         part with one password prompt. The background service keeps your fan settings applied \
                         while this window is closed.",
                    )),
            )
            .child(self.service_section(false, cx))
            .child(div().text_sm().text_color(t.muted_foreground).child("Tip: double-click any name in the app to rename it."))
            .child(h_flex().justify_end().child(next.on_click(cx.listener(|this, _, _, cx| {
                this.finish_onboarding();
                cx.notify();
            }))))
    }

    fn service_section(&self, header: bool, cx: &Context<Self>) -> AnyElement {
        let t = cx.theme();
        let s = &self.service;
        let row = |ok: bool, label: &'static str, action: Option<Button>| {
            h_flex()
                .gap_2()
                .min_h(px(28.))
                .child(
                    Icon::new(if ok { IconName::CircleCheck } else { IconName::CircleX })
                        .small()
                        .text_color(if ok { t.success } else { t.muted_foreground }),
                )
                .child(div().flex_1().min_w_0().child(label))
                .children(action.map(|b| b.small().disabled(s.busy)))
        };

        // Everything Fix handles, i.e. every setup item except the service.
        let needs_fix = s.items().iter().any(|(name, ok)| *name != "service" && !ok);
        let fix = needs_fix.then(|| {
            Button::new("setup-fix")
                .label("Fix (asks for password)")
                .primary()
                .on_click(cx.listener(|this, _, _, cx| this.run_service_task(core::setup_system, cx)))
        });
        let service = if s.installed {
            Button::new("service-remove")
                .label("Remove")
                .outline()
                .on_click(cx.listener(|this, _, _, cx| this.run_service_task(core::uninstall_service, cx)))
        } else {
            Button::new("service-install")
                .label("Install")
                .primary()
                .on_click(cx.listener(|this, _, _, cx| this.run_service_task(core::install_service, cx)))
        };
        let boot = s.installed.then(|| {
            if s.starts_at_boot {
                Button::new("boot-off")
                    .label("Disable")
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| this.run_service_task(core::disable_start_at_boot, cx)))
            } else {
                Button::new("boot-on")
                    .label("Enable")
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| this.run_service_task(core::enable_start_at_boot, cx)))
            }
        });
        let service_label = match (s.installed, s.running) {
            (_, true) => "Background service running",
            (true, false) => "Background service installed, not running",
            (false, false) => "Background service (keeps curves running when closed)",
        };

        v_flex()
            .gap_4()
            .children(header.then(|| self.section_header("section:service", "Setup", None, cx)))
            .child(
                Self::card(t)
                    .py_3()
                    .gap_1()
                    .child(row(s.motherboard_fans, "Motherboard fan driver loaded", None))
                    .child(row(s.driver_at_boot, "Driver loads at boot", None))
                    .child(row(s.writable && s.energy_readable, "This user can control fans and read CPU power", None))
                    .children(fix.map(|b| h_flex().justify_end().pt_1().child(b.small().disabled(s.busy)))),
            )
            .child(
                Self::card(t)
                    .py_3()
                    .gap_1()
                    .child(row(s.running, service_label, Some(service)))
                    .children(boot.map(|b| row(s.starts_at_boot, "Starts at boot, before you log in", Some(b))))
                    .children(s.busy.then(|| div().text_color(t.muted_foreground).child("Working…")))
                    .children(s.message.clone().map(|m| div().text_color(t.danger).child(m))),
            )
            .into_any_element()
    }
}

impl Render for FanControl {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let error = self.error.clone().map(|e| {
            div()
                .px_4()
                .py_2()
                .rounded(cx.theme().radius_lg)
                .bg(cx.theme().danger.opacity(0.15))
                .text_color(cx.theme().danger)
                .text_sm()
                .child(e)
        });
        let sensors = self.sensors_section(cx);
        let fans = self.fans_section(cx);
        let content = if self.onboarding {
            self.onboarding_view(cx).into_any_element()
        } else {
            let service = self.service_section(true, cx);
            let lights = (self.lights_present || self.lights.iter().any(|l| l.leds > 0)).then(|| self.lights_section(window, cx));
            v_flex().gap_8().child(sensors).child(fans).children(lights).child(service).into_any_element()
        };
        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                v_flex()
                    .id("main")
                    .size_full()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex().p_6().gap_8().children(error).child(content).child(
                            div()
                                .text_xs()
                                .text_center()
                                .text_color(cx.theme().muted_foreground)
                                .child(concat!("fanctlrs v", env!("CARGO_PKG_VERSION"))),
                        ),
                    ),
            )
    }
}

fn main() {
    if std::env::args().any(|a| a == "--daemon") {
        core::run_daemon();
        return;
    }

    // Without a display the window can't open; don't linger invisibly
    // (the tray would think the window is already open).
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        eprintln!("fanctlrs: no display (WAYLAND_DISPLAY/DISPLAY not set)");
        std::process::exit(1);
    }
    application().with_assets(assets::AllAssets).run(|cx| {
        init(cx);
        // Closing the window ends the app; the tray (in the daemon) can reopen it.
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(440.), px(900.)), cx))),
            titlebar: Some(TitlebarOptions { title: Some("Fan Control".into()), ..Default::default() }),
            ..Default::default()
        };
        open_window(options, cx, |window, cx| {
            Theme::sync_system_appearance(Some(window), cx);
            cx.new(|cx| FanControl::new(window, cx))
        })
        .expect("failed to open window");
    });
}

#[cfg(test)]
mod tests {
    use super::{Kind, cpu_times, energy_power, guide_frame, matches_profile, needs_onboarding, parse_sensor_file, ram_used};
    use crate::core::parse_kv;

    #[test]
    fn sensor_files() {
        assert_eq!(parse_sensor_file("temp1_input"), Some((Kind::Temp, 1, "temp1")));
        assert_eq!(parse_sensor_file("fan6_input"), Some((Kind::Fan, 6, "fan6")));
        assert_eq!(parse_sensor_file("in0_input"), Some((Kind::Volt, 0, "in0")));
        assert_eq!(parse_sensor_file("power1_average"), Some((Kind::Power, 1, "power1")));
        assert_eq!(parse_sensor_file("temp1_max"), None);
        assert_eq!(parse_sensor_file("pwm1_enable"), None);
        assert_eq!(Kind::Temp.format(34000.0), "34.0 °C");
        assert_eq!(Kind::Power.format(32_000_000.0), "32.0 W");
    }

    #[test]
    fn profile_matching() {
        let current = parse_kv("a/pwm3 = 40\na/pwm4 = auto\na/pwm6 = 100\n");
        assert!(matches_profile(&current, &parse_kv("a/pwm3 = 40\na/pwm4 = auto\n")));
        assert!(!matches_profile(&current, &parse_kv("a/pwm3 = 41\n")));
        assert!(!matches_profile(&current, &parse_kv("a/pwm4 = 30\n")));
        assert!(matches_profile(&current, &parse_kv("gone/pwm1 = 50\na/pwm6 = 100\n")));
        assert!(!matches_profile(&current, &parse_kv("")));
    }

    #[test]
    fn onboarding() {
        let ok = [("driver", true), ("boot", true), ("permission", true), ("service", true)];
        let missing = [("driver", true), ("boot", true), ("permission", true), ("service", false)];
        let broken = [("driver", false), ("boot", true), ("permission", true), ("service", false)];
        assert!(!needs_onboarding(&ok, None));
        assert!(needs_onboarding(&missing, None));
        // Skipped while only the service was missing: stay quiet...
        assert!(!needs_onboarding(&missing, Some("driver\nboot\npermission")));
        // ...until something that worked breaks.
        assert!(needs_onboarding(&broken, Some("driver\nboot\npermission")));
    }

    #[test]
    fn system_sensors() {
        assert_eq!(parse_sensor_file("freq1_input"), Some((Kind::Freq, 1, "freq1")));
        assert_eq!(Kind::Freq.format(4_775_112_000.0), "4775 MHz");
        assert_eq!(Kind::Bytes.format(1_367_453_696.0), "1.27 GiB");
        assert_eq!(cpu_times("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3\n"), Some((850, 1000)));
        assert_eq!(cpu_times("garbage"), None);
        assert_eq!(ram_used("MemTotal:  32 kB\nMemFree: 1 kB\nMemAvailable:  12 kB\n"), Some(20.0 * 1024.0));
    }

    #[test]
    fn cpu_power() {
        // 45 J in 1.5 s = 30 W (in µW, like hwmon power)
        assert_eq!(energy_power(1_000_000.0, 46_000_000.0, 1e12, 1.5), Some(30_000_000.0));
        // the counter wrapped: 10 J before the wrap + 20 J after it, in 1 s
        assert_eq!(energy_power(990_000_000.0, 20_000_000.0, 1_000_000_000.0, 1.0), Some(30_000_000.0));
        assert_eq!(energy_power(0.0, 1.0, 1e12, 0.0), None);
    }

    #[test]
    fn light_guide() {
        let f = guide_frame(2, Some(12));
        assert_eq!((f[2][11], f[2][12], f[2].len()), ([0, 255, 0], [255, 0, 0], 256));
        assert!(f[0].iter().all(|c| *c == [0; 3])); // every other channel off
        assert!(guide_frame(2, None)[2].iter().all(|c| *c == [255; 3]));
    }
}
