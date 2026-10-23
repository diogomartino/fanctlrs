# fanctlrs

A small Linux app to watch your sensors and control your fans and pumps.

It works with any fan the kernel exposes as `pwmN` under `/sys/class/hwmon`:
most motherboards (once the right driver is loaded) and AMD GPUs.

## 1. Find your fan chip

```bash
sudo pacman -S lm_sensors        # Debian/Ubuntu: sudo apt install lm-sensors
sudo sensors-detect --auto | grep -i found
```

Look for a line like `Found 'ITE IT8665E Super IO Sensors'` or `Found 'Nuvoton NCT6798D'`.

## 2. Install the driver if your chip needs one

| Chip | Driver |
|---|---|
| Nuvoton NCT67xx | `nct6775`, already in the kernel |
| ITE IT86xx / IT87xx | older chips: `it87`, already in the kernel. Newer ones (IT8665E, IT8686E, IT8688E…) need [it87](https://github.com/frankcrawford/it87): `yay -S it87-dkms-git` on Arch |
| AMD GPU | `amdgpu`, already loaded |

This is the only step the app can't do for you.

## 3. Get fanctlrs

Download `fanctlrs-vX.Y.Z-x86_64-linux.tar.gz` from the Releases page, extract it and run
`./fanctlrs`. Or build it yourself ([Rust](https://rustup.rs) 1.92 or newer):

```bash
cargo run --release
```

Run it as your normal user, never with `sudo`.

## 4. Setup, from the app

The **Setup** section at the bottom shows what's missing:

- **Motherboard fan driver loaded**, **Driver loads at boot**, **This user can control fans**:
  click **Fix**. It asks for your password once, loads your board's fan driver, makes it load
  at every boot and installs [99-fanctlrs.rules](99-fanctlrs.rules) so you never need `sudo` again.
- **Background service**: **Install** runs `fanctlrs --daemon` as a systemd user service:
  no window, about 10 MB of memory, checks every 2 seconds. It keeps your speeds and curves
  applied while the window is closed and puts them back after reboot, suspend or when the
  BIOS takes a fan back. While it runs, the window leaves fan control to it.
- **Starts at boot, before you log in**: without it the service starts when you log in, and the
  BIOS controls the fans until then.

If Fix says no driver works on your board: check step 2. If the driver is installed but refuses
to load, your BIOS is reserving the chip: add `acpi_enforce_resources=lax` to your kernel command
line and reboot.

The service also puts a fan icon in your system tray: click it to open the window.

Logs of the service: `journalctl --user -u fanctlrs`.

## Releasing

Every push builds and tests the app on GitHub. To publish a release: **Actions → Build &
release → Run workflow**, pick patch, minor or major. It bumps the version in `Cargo.toml`,
commits and tags it, and attaches the binary to a new GitHub release.

## Naming things

The driver only knows `fan1`, `temp2`… **Double-click any name** in the app (sections,
devices, sensors, fans) to rename it. Press Enter or click away to save; leave it
empty to go back to the default. Renaming a fan also renames its RPM sensor.

To find which header is which: set one fan to 0% and see which one stops.

## Saved settings

Everything you change is saved in `~/.config/fanctlrs/` and restored on the next launch,
including each fan's speed (or Auto):

| File | What |
|---|---|
| `names` | your renames. You can also write `chip/sensor = Name` lines by hand (names as `sensors` prints them, e.g. `it8665/fan6 = Water Pump`) |
| `hidden-sensors` | what you hid with Show / hide |
| `fans` | each fan's mode (`auto`, `curve` or a %) and its curve (`key#curve = sensor temp:% …`) |
| `profiles/<name>` | saved profiles, same format as `fans` |

## Fan curves

Click **Curve** on a fan to make it follow a temperature. Pick the sensor with the
button next to *Follows* (the CPU by default), then set 4 points: **−/+** move a point's
temperature, the slider sets the fan % at that temperature. Between points the speed is
interpolated; below the first and above the last it stays flat.

Fans speed up right away but only slow down once the temperature has dropped 3 °C, so
they don't hunt up and down. If the sensor can't be read, the fan goes to 100%.

Curves only run while the window is open, unless the background service is installed.

## Profiles

Set your fans the way you like (speeds, Auto or curves), click **Save profile** and give it a name. Click the
profile later to apply it in one go. The highlighted profile is the one currently
applied. The **×** next to it deletes it.

Run the app as your normal user (see step 4), not with `sudo`. Otherwise these files
end up owned by root and the app can't save.

## Good to know

- Use **Show / hide** to pick which sensors and fans appear. Your choice is saved.
- **Auto** hands a fan back to the BIOS (or GPU firmware).
- Some BIOSes quietly take fans back after a while (also after suspend). While the app
  is open it notices and re-applies your speed; the ↻ number on a fan shows how often.
- There's no lower limit on the sliders. **Don't stop your pump.** Without coolant flow your CPU overheats within seconds.
- Without the background service, fans keep their last speed when you close the app, and
  after a reboot the BIOS is in charge until you open it again.
- If the service is stopped, fans also keep their last speed.
