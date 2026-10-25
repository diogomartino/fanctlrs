mod core;

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::PathBuf,
    time::Duration,
};

use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt, Theme,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    popover::Popover,
    scroll::ScrollableElement,
    slider::{Slider, SliderEvent, SliderState},
    v_flex,
};
use gpui_kit::*;

use crate::core::{
    AUTO_SEED_DUTY, Chip, Curve, Mode, chips, config_dir, curve_key, daemon_running, enforce, fan_file, fans_writable,
    format_kv, parse_kv, pct_to_duty, read_config, read_num,
};

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
}

impl Kind {
    fn icon(self, t: &Theme) -> (assets::IconName, Hsla) {
        match self {
            Kind::Temp => (assets::IconName::Thermometer, t.danger),
            Kind::Fan => (assets::IconName::Fan, t.info),
            Kind::Volt => (assets::IconName::Zap, t.warning),
            Kind::Curr => (assets::IconName::Activity, t.success),
            Kind::Power => (assets::IconName::Plug, t.chart_4),
        }
    }

    fn format(self, raw: f64) -> String {
        match self {
            Kind::Temp => format!("{:.1} °C", raw / 1000.0),
            Kind::Fan => format!("{raw:.0} RPM"),
            Kind::Volt => format!("{:.2} V", raw / 1000.0),
            Kind::Curr => format!("{:.2} A", raw / 1000.0),
            Kind::Power => format!("{:.1} W", raw / 1_000_000.0),
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
        .unwrap_or_else(|| base.to_string())
}

struct Sensor {
    key: String,
    default: String,
    kind: Kind,
    path: PathBuf,
    value: Option<f64>,
}

struct Group {
    key: String,
    default: String,
    sensors: Vec<Sensor>,
}

fn discover_sensors(chips: &[Chip], names: &HashMap<String, String>) -> Vec<Group> {
    chips
        .iter()
        .filter_map(|chip| {
            let mut sensors: Vec<(u32, Sensor)> = fs::read_dir(&chip.dir)
                .ok()?
                .flatten()
                .filter_map(|e| {
                    let file = e.file_name().to_string_lossy().into_owned();
                    let (kind, n, base) = parse_sensor_file(&file)?;
                    let sensor = Sensor {
                        key: format!("{}/{base}", chip.key),
                        default: default_label(chip, base, names),
                        kind,
                        path: e.path(),
                        value: None,
                    };
                    Some((kind as u32 * 1000 + n, sensor))
                })
                .collect();
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
        .collect()
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
            ("permission", self.writable),
            ("service", self.installed),
        ]
    }

    /// Cheap enough to run after every setup action, too slow for every tick.
    fn check(&mut self) {
        let chips = chips();
        self.installed = core::service_installed();
        self.running = daemon_running();
        self.writable = fans_writable(&chips);
        self.motherboard_fans = core::motherboard_fans(&chips);
        self.driver_at_boot = core::driver_loads_at_boot();
        self.starts_at_boot = core::starts_at_boot();
    }
}

struct FanControl {
    groups: Vec<Group>,
    fans: Vec<Fan>,
    names: HashMap<String, String>,
    hidden: HashSet<String>,
    profiles: Vec<String>,
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
            fans,
            names,
            hidden: read_config("hidden-sensors").lines().map(String::from).collect(),
            profiles: list_profiles(),
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
        this
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
        for s in self.groups.iter_mut().flat_map(|g| &mut g.sensors) {
            if !self.hidden.contains(&s.key) {
                s.value = read_num(&s.path);
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
        if name.is_empty() {
            self.names.remove(&key);
        } else {
            self.names.insert(key, name);
        }
        let contents = "# chip/sensor = Name, or chip@device/sensor = Name (written by the app)\n".to_string()
            + &format_kv(self.names.iter());
        self.save("names", contents);
        cx.notify();
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
                            .child(div().font_semibold().child(s.value.map_or("—".into(), |v| s.kind.format(v))))
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

        let needs_fix = !(s.motherboard_fans && s.driver_at_boot && s.writable);
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
                    .child(row(s.writable, "This user can control fans", None))
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            v_flex().gap_8().child(sensors).child(fans).child(service).into_any_element()
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
    use super::{Kind, matches_profile, needs_onboarding, parse_sensor_file};
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
}
