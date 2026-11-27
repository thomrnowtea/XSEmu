//! Per-title configuration (`%LOCALAPPDATA%\XCL\<title>\xcl.toml`) and the boot record
//! the launcher hands to the host process.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) fn gpu_markers_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("XCL_GPU_MARKERS").is_ok_and(|v| v.trim() == "1"))
}

/// The guard tripwire (`hle::d3d12x::bind::Trips`): translated compute programs count the lanes a
/// guard gave up in (a spent loop budget first of all), and the 30 s line names the pipelines.
/// On unless `XCL_NO_GUARD_TRIPS` is set; `XCL_GPU_MARKERS=1` always has it.
pub(crate) fn guard_trips_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| gpu_markers_enabled() || std::env::var_os("XCL_NO_GUARD_TRIPS").is_none())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "anaconda" (Series X) or "lockhart" (Series S).
    pub device_type: String,
    /// Raw value returned by kernelx `GetConsoleType`. Series values are not public;
    /// this is a guess and can be overridden.
    pub console_type_raw: u32,
    pub gamertag: String,
    pub user_id: u64,
    pub console_id: String,
    pub sandbox_id: String,
    /// Title memory budget reported by TitleMemoryStatus (MiB); 0 = the standard budget of
    /// `device_type` (13 824 Series X, 8 192 Series S).
    pub title_memory_mb: u64,
    /// "auto" (MicrosoftGame.Config), "standard" or "advanced" title memory mode.
    pub title_memory: String,
    /// Persistent local storage quota (MiB); 0 = MicrosoftGame.Config, else 4096.
    pub pls_mb: u64,
    /// Unified (GPU-visible) memory (MiB, up to 4 × 4095: above 4095 it is split into 4 GiB
    /// windows, see `gpu::uma`); 0 = 4095 for "advanced" title memory,
    /// else 2048. `XCL_UMA_MB` overrides.
    pub uma_mb: u64,
    pub log_level: String,
    pub log_to_stderr: bool,
    /// Executable inside the game folder (auto-detected when empty).
    pub exe: String,
    pub package_identifier: String,
    /// Xbox title id (hex) for XGameGetXboxTitleId; empty = `<TitleId>` of MicrosoftGame.Config.
    pub title_id: String,
    /// Console path prefix → target. `{game}` and `{data}` are expanded.
    pub mounts: BTreeMap<String, String>,
    /// Import DLL pattern → route: an HLE module ("kernelx.dll"), "@system" or "@stub".
    pub aliases: BTreeMap<String, String>,
    /// "dll!function" (or "*!function") → return value for auto-stubs.
    pub stub_returns: BTreeMap<String, String>,
    /// "d3d12" (real GPU, default) or "null" (no rendering).
    pub graphics: String,
    /// "auto" (high-performance GPU, then WARP), "warp", a number (that hardware adapter in
    /// high-performance order) or a part of the adapter's name ("radeon"). `XCL_ADAPTER` overrides.
    pub d3d12_adapter: String,
    /// Enable the D3D12 debug layer (needs Graphics Tools).
    pub d3d12_debug: bool,
    /// "auto" (show the title's frames in an XCL window) or "off".
    pub present: String,
    pub display_mode: String,
    /// Raytracing tier reported by `CheckFeatureSupport(D3D12_OPTIONS5)`:
    /// "auto" (host GPU tier), "tier1_1" (11), "tier1_0" (10), or "off" (0).
    /// `XCL_RAYTRACING` overrides.
    pub raytracing: String,
    pub window_width: u32,
    pub window_height: u32,
    /// The XCL window's position: "auto" (CW_USEDEFAULT – Windows may place it on any monitor),
    /// "center" (the primary monitor's work area), or "x,y" (top-left of the window, screen
    /// coordinates; clamped into the nearest monitor's work area). `XCL_WINDOW_POS` overrides.
    pub window_pos: String,
    /// Answer `XUserGetTokenAndSignature*` with XCL's local placeholder token (for a backend of
    /// your own that accepts it). Off: the request fails as on a console that cannot reach Xbox
    /// Live (0x80072EFD), and titles go their offline way – Xbox Live's and the titles' own
    /// services would reject the placeholder anyway (Minecraft retried its sign-in forever).
    /// With `XCL_NETWORK=0` there is no token either way: the request fails with
    /// E_GAMEUSER_FAILED_TO_GET_TOKEN, as on a console without a network (`hle::netoff`).
    pub xbox_live_tokens: bool,
    /// Keyboard and mouse play as the gamepad while the XCL window has the focus: WASD the
    /// left stick, the mouse the right stick (the cursor is held in the window), the mouse
    /// buttons the triggers, the wheel the bumpers (see `hle::kbm`).
    pub keyboard_mouse: bool,
    /// How far the right stick goes for a mouse speed: 1.0 = full deflection at 4000 mouse
    /// counts a second.
    pub mouse_sensitivity: f32,
    /// Frame interval (µs) the frame clock runs at instead of the one the title sets with
    /// `SetFrameIntervalX` (e.g. 16667 for 60 fps where the title asks for 33333); 0 = the
    /// title's own. Only a console interval of the display (`frame::interval_valid`) is taken.
    /// `XCL_FRAME_INTERVAL_US` overrides.
    pub frame_interval_us: u32,
}

impl Config {
    /// Fill the "auto" settings from the game's MicrosoftGame.Config (if any); afterwards every
    /// setting has a concrete value. Returns what came from the manifest (for the log).
    pub fn resolve(&mut self, manifest: Option<&crate::manifest::Manifest>) -> Vec<String> {
        let mut from = Vec::new();
        let series_s = self.is_series_s();
        // "Auto" / "Advanced" in xcl.toml: read in any case, as device_type and the manifest (#106)
        self.title_memory = self.title_memory.trim().to_ascii_lowercase();
        if self.title_memory == "auto" {
            let m = manifest.and_then(|m| if series_s { m.lockhart_memory.clone() } else { m.anaconda_memory.clone() });
            if let Some(m) = &m {
                from.push(format!("title memory {m}"));
            }
            self.title_memory = m.unwrap_or_else(|| "standard".into()).to_ascii_lowercase();
        }
        // the standard budgets; "advanced" adds memory the console docs do not give publicly –
        // the standard figure stays (xcl.toml can raise it)
        self.title_memory_mb = self.title_memory_budget_mb();
        if self.pls_mb == 0 {
            self.pls_mb = match manifest.and_then(|m| m.pls_mb) {
                Some(v) => {
                    from.push(format!("PLS {v} MB"));
                    v
                }
                None => 4096,
            };
        }
        if self.title_id.is_empty() {
            if let Some(t) = manifest.and_then(|m| m.title_id.clone()) {
                from.push(format!("TitleId {t}"));
                self.title_id = t;
            }
        }
        if self.uma_mb == 0 {
            self.uma_mb = if self.title_memory == "advanced" { 4095 } else { 2048 };
        }
        from
    }

    /// TitleMemoryStatus budget (MiB): `title_memory_mb`, 0 = the device's standard budget.
    pub fn title_memory_budget_mb(&self) -> u64 {
        match self.title_memory_mb {
            0 if self.is_series_s() => 8_192,
            0 => 13_824,
            v => v,
        }
    }
}

/// Mount target of the persistent local storage drive (`XPersistentLocalStorageGetPath`).
pub const PLS_MOUNT_TARGET: &str = "{data}\\pls";

impl Config {
    pub(crate) fn pls_quota_bytes(&self) -> u64 {
        match self.pls_mb {
            0 => 4096,
            mb => mb,
        }
        .saturating_mul(1024 * 1024)
    }

    /// The console drive the title is given as its persistent local storage root ("S:\"):
    /// the drive-letter mount onto `{data}\pls`. None when the mounts have no such drive.
    pub fn pls_drive(&self) -> Option<String> {
        self.mounts.iter().find_map(|(prefix, target)| {
            let b = prefix.as_bytes();
            let target = target.replace('/', "\\");
            let is_drive = b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
            (is_drive && target.trim_end_matches('\\').eq_ignore_ascii_case(PLS_MOUNT_TARGET)).then(|| format!("{}\\", prefix.to_ascii_uppercase()))
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut mounts = BTreeMap::new();
        mounts.insert("GAME:".into(), "{game}".into());
        mounts.insert("G:".into(), "{game}".into());
        // the package drive of the XDK days, still used by GDK titles (XDefiant: d:\hunter\...)
        mounts.insert("D:".into(), "{game}".into());
        mounts.insert("T:".into(), "{data}\\temp".into());
        // persistent local storage: on the console a folder mounted at the root of a drive
        mounts.insert("S:".into(), PLS_MOUNT_TARGET.into());
        Config {
            device_type: "anaconda".into(),
            console_type_raw: 6,
            gamertag: "Player1".into(),
            user_id: 0x0009_0000_0000_0001,
            console_id: CONSOLE_ID.into(),
            sandbox_id: "RETAIL".into(),
            title_memory_mb: 0,
            title_memory: "auto".into(),
            pls_mb: 0,
            uma_mb: 0,
            log_level: "info".into(),
            log_to_stderr: true,
            exe: String::new(),
            package_identifier: String::new(),
            title_id: String::new(),
            mounts,
            aliases: BTreeMap::new(),
            stub_returns: BTreeMap::new(),
            graphics: "d3d12".into(),
            d3d12_adapter: "auto".into(),
            d3d12_debug: false,
            present: "auto".into(),
            display_mode: "auto".into(),
            raytracing: "auto".into(),
            window_width: 1280,
            window_height: 720,
            window_pos: "auto".into(),
            xbox_live_tokens: false,
            keyboard_mouse: true,
            mouse_sensitivity: 1.0,
            frame_interval_us: 0,
        }
    }
}

const HEADER: &str = "# XCL – title configuration. Created automatically; you can edit it.\n\
# device_type: \"anaconda\" (Series X) or \"lockhart\" (Series S)\n\
# display_mode: \"auto\" (Lockhart 1280x720@60 SDR, other devices host), \"host\", or \"WxH[@60|@120]\" (SDR); XCL_DISPLAY_MODE overrides\n\
# mounts: console path prefix -> folder ({game} = game folder, read-only; {data} = XCL data)\n\
# aliases: DLL pattern -> \"kernelx.dll\" / \"xgameruntime.dll\" / \"d3d12_xs.dll\" / \"@system\" / \"@stub\"\n\
# stub_returns: \"dll!Function\" -> value the stub returns (e.g. \"E_NOTIMPL\", \"0\", \"0x1\")\n\
# frame_interval_us: frame interval in µs instead of the title's (0 = the title's): 16667 = 60 fps, 8333 = 120 fps (120 Hz displays only); XCL_FRAME_INTERVAL_US overrides\n\n";

/// What XSystemGetConsoleId returns: a made-up id in the console id format, 38 characters
/// (four groups of 8 hex digits and one of 2), which titles size their buffer for
/// (XSystemConsoleIdBytes, 39 with the terminator). No group is zero: a title may take the id
/// for a number.
pub const CONSOLE_ID: &str = "F4000001.58434C01.58434C02.58434C03.A5";
/// The defaults before: 41 characters, too long for that buffer, and an id of zeros only.
const OLD_CONSOLE_IDS: [&str; 2] = ["XCL00000000000000000000000000000000000001", "00000000.00000000.00000000.00000000.00"];

impl Config {
    pub fn load_or_create(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let mut c: Config = toml::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?;
                // default mounts added since the file was written
                for (k, v) in Config::default().mounts {
                    c.mounts.entry(k).or_insert(v);
                }
                // the old default console id, written into every xcl.toml created before
                if OLD_CONSOLE_IDS.contains(&c.console_id.as_str()) {
                    c.console_id = CONSOLE_ID.into();
                }
                Ok(c)
            }
            // only a missing file gets the defaults written: an unreadable one (locked, not UTF-8)
            // must not be replaced by them (issue #106)
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
            Err(_) => {
                let c = Config::default();
                let body = toml::to_string_pretty(&c).map_err(|e| e.to_string())?;
                if let Some(d) = path.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                let _ = std::fs::write(path, format!("{HEADER}{body}"));
                Ok(c)
            }
        }
    }

    pub fn is_series_s(&self) -> bool {
        self.device_type.eq_ignore_ascii_case("lockhart") || self.device_type.eq_ignore_ascii_case("series s")
    }

    pub(crate) fn reported_display_mode(&self) -> Option<DisplayMode> {
        self.display_mode_with_env(std::env::var("XCL_DISPLAY_MODE").ok().as_deref())
    }

    fn display_mode_with_env(&self, env: Option<&str>) -> Option<DisplayMode> {
        match env.and_then(DisplaySetting::parse).or_else(|| DisplaySetting::parse(&self.display_mode)).unwrap_or(DisplaySetting::Auto) {
            DisplaySetting::Fixed(mode) => Some(mode),
            DisplaySetting::Auto if self.is_series_s() => Some(DisplayMode { width: 1280, height: 720, hz: 60 }),
            DisplaySetting::Auto | DisplaySetting::Host => None,
        }
    }

    /// Configured `D3D12_RAYTRACING_TIER` override for `CheckFeatureSupport(D3D12_OPTIONS5)`:
    /// `None` keeps the host GPU tier ("1" / "on"), `Some(0 / 10 / 11)` forces NOT_SUPPORTED /
    /// TIER_1_0 / TIER_1_1; with ray tracing off (`raytracing_on`) always `Some(0)`.
    pub fn raytracing_tier_override(&self) -> Option<u32> {
        if !self.raytracing_on() {
            return Some(0);
        }
        parse_raytracing_tier(self.raytracing_mode().trim())
    }

    /// Ray tracing on (`hle::d3d12x::raytracing`): only when asked for, "1" / "on" or a tier, for
    /// now (the user, 2026-11-26 21:43Z: "You should just turn off ray tracing for now"); "auto",
    /// "off" and anything else leave it off. `XCL_RAYTRACING` overrides `xcl.toml`.
    pub fn raytracing_on(&self) -> bool {
        raytracing_mode_on(self.raytracing_mode().trim())
    }

    /// The frame interval override (µs, 0 = none): `XCL_FRAME_INTERVAL_US`, else `frame_interval_us`.
    pub(crate) fn frame_interval_override_us(&self) -> u32 {
        self.frame_interval_with_env(std::env::var("XCL_FRAME_INTERVAL_US").ok().as_deref())
    }

    fn frame_interval_with_env(&self, env: Option<&str>) -> u32 {
        env.and_then(|v| v.trim().parse().ok()).unwrap_or(self.frame_interval_us)
    }

    fn raytracing_mode(&self) -> String {
        std::env::var("XCL_RAYTRACING").unwrap_or_else(|_| self.raytracing.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub hz: u32,
}

enum DisplaySetting {
    Auto,
    Host,
    Fixed(DisplayMode),
}

impl DisplaySetting {
    fn parse(setting: &str) -> Option<Self> {
        let setting = setting.trim().to_ascii_lowercase();
        match setting.as_str() {
            "auto" => return Some(Self::Auto),
            "host" => return Some(Self::Host),
            _ => {}
        }
        let (size, hz) = setting.split_once('@').unwrap_or((&setting, "60"));
        let (width, height) = size.split_once('x')?;
        let (width, height, hz) = (width.parse().ok()?, height.parse().ok()?, hz.parse().ok()?);
        if !(1..=16384).contains(&width) || !(1..=16384).contains(&height) || !matches!(hz, 60 | 120) {
            return None;
        }
        Some(Self::Fixed(DisplayMode { width, height, hz }))
    }
}

pub fn raytracing_mode_on(mode: &str) -> bool {
    mode == "1" || mode.eq_ignore_ascii_case("on") || parse_raytracing_tier(mode).is_some_and(|tier| tier != 0)
}

pub fn parse_raytracing_tier(mode: &str) -> Option<u32> {
    if mode.eq_ignore_ascii_case("off") || mode.eq_ignore_ascii_case("none") || mode == "0" {
        Some(0)
    } else if mode.eq_ignore_ascii_case("tier1_0") || mode.eq_ignore_ascii_case("1.0") || mode == "10" {
        Some(10)
    } else if mode.eq_ignore_ascii_case("tier1_1") || mode.eq_ignore_ascii_case("1.1") || mode == "11" {
        Some(11)
    } else {
        None
    }
}

/// Everything the host process needs; written by the launcher as JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Boot {
    pub title: String,
    pub exe_path: PathBuf,
    pub game_root: PathBuf,
    pub data_dir: PathBuf,
    pub config: Config,
}

impl Boot {
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.data_dir.join("cache")
    }
    pub fn report_path(&self) -> PathBuf {
        self.logs_dir().join("report.json")
    }
    pub fn expand(&self, s: &str) -> String {
        s.replace("{game}", &self.game_root.to_string_lossy())
            .replace("{data}", &self.data_dir.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips_through_toml() {
        let c = Config::default();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.mounts.get("G:").map(String::as_str), Some("{game}"));
        assert_eq!(back.title_memory_mb, 0);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let c: Config = toml::from_str("device_type = \"lockhart\"\n").unwrap();
        assert!(c.is_series_s());
        assert_eq!(c.gamertag, "Player1");
        assert_eq!(c.raytracing, "auto");
        assert_eq!(c.frame_interval_us, 0);
    }

    #[test]
    fn frame_interval_override_env_beats_toml() {
        let c: Config = toml::from_str("frame_interval_us = 16667").unwrap();
        assert_eq!(c.frame_interval_with_env(None), 16667);
        assert_eq!(c.frame_interval_with_env(Some(" 8333 ")), 8333);
        assert_eq!(c.frame_interval_with_env(Some("0")), 0, "0 in the environment: the title's interval");
        assert_eq!(c.frame_interval_with_env(Some("fast")), 16667, "not a number: xcl.toml's");
        assert_eq!(Config::default().frame_interval_with_env(None), 0);
    }

    #[test]
    fn reported_display_defaults_to_720p_only_for_lockhart_and_accepts_overrides() {
        let mut c: Config = toml::from_str("device_type = \"lockhart\"").unwrap();
        let lockhart = Some(DisplayMode { width: 1280, height: 720, hz: 60 });
        assert_eq!(c.display_mode_with_env(None), lockhart);
        assert_eq!(c.display_mode_with_env(Some("0x720")), lockhart);
        assert_eq!(Config::default().display_mode_with_env(None), None);
        c = toml::from_str("device_type = \"lockhart\"\ndisplay_mode = \"1920x1080\"").unwrap();
        let toml = Some(DisplayMode { width: 1920, height: 1080, hz: 60 });
        assert_eq!(c.display_mode_with_env(None), toml);
        assert_eq!(c.display_mode_with_env(Some("1920x1080@30")), toml);
        assert_eq!(c.display_mode_with_env(Some("2560X1440@120")), Some(DisplayMode { width: 2560, height: 1440, hz: 120 }));
        assert_eq!(c.display_mode_with_env(Some("auto")), lockhart);
        assert_eq!(c.display_mode_with_env(Some("host")), None);
    }

    #[test]
    fn the_console_id_fits_the_console_id_buffer() {
        // XSystemConsoleIdBytes: the 38-character console id format plus its terminator
        assert_eq!(Config::default().console_id.len() + 1, 39);
        let dir = std::env::temp_dir().join(format!("xcl_cfg_console_id_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("xcl.toml");
        // five groups of hex digits, none of them zero
        let groups: Vec<&str> = CONSOLE_ID.split('.').collect();
        assert_eq!(groups.iter().map(|g| g.len()).collect::<Vec<_>>(), [8, 8, 8, 8, 2]);
        assert!(groups.iter().all(|g| u32::from_str_radix(g, 16).is_ok_and(|v| v != 0) && !g.contains(|c: char| c.is_ascii_lowercase())));
        // an xcl.toml written with an old default gets the new one; a user's own id is kept
        for old in OLD_CONSOLE_IDS {
            std::fs::write(&path, format!("console_id = \"{old}\"\n")).unwrap();
            assert_eq!(Config::load_or_create(&path).unwrap().console_id, CONSOLE_ID);
        }
        std::fs::write(&path, "console_id = \"12345678.12345678.12345678.12345678.12\"\n").unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap().console_id, "12345678.12345678.12345678.12345678.12");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pls_drive_is_the_mount_onto_the_pls_folder() {
        let mut c = Config::default();
        assert_eq!(c.pls_drive().as_deref(), Some("S:\\"));
        // a user-moved drive (any spelling of the target) is followed
        c.mounts.remove("S:");
        assert_eq!(c.pls_drive(), None);
        c.mounts.insert("p:".into(), "{data}/PLS\\".into());
        assert_eq!(c.pls_drive().as_deref(), Some("P:\\"));
        // directory-style prefixes are not drives
        c.mounts.remove("p:");
        c.mounts.insert("PLS:".into(), PLS_MOUNT_TARGET.into());
        assert_eq!(c.pls_drive(), None);
    }

    #[test]
    fn every_series_s_spelling_gets_the_series_s_memory() {
        let manifest = crate::manifest::Manifest {
            anaconda_memory: Some("Standard".into()),
            lockhart_memory: Some("Advanced".into()),
            ..Default::default()
        };
        for device in ["lockhart", "Series S"] {
            let mut c = Config { device_type: device.into(), ..Config::default() };
            c.resolve(Some(&manifest));
            assert_eq!((c.title_memory.as_str(), c.title_memory_mb), ("advanced", 8_192), "{device}");
        }
        let mut c = Config::default();
        c.resolve(Some(&manifest));
        assert_eq!((c.title_memory.as_str(), c.title_memory_mb), ("standard", 13_824));
    }

    #[test]
    fn title_memory_is_read_in_any_case() {
        let manifest = crate::manifest::Manifest { anaconda_memory: Some("Advanced".into()), ..Default::default() };
        for (setting, m) in [("Auto", Some(&manifest)), ("ADVANCED", None)] {
            let mut c = Config { title_memory: setting.into(), ..Config::default() };
            c.resolve(m);
            assert_eq!((c.title_memory.as_str(), c.uma_mb), ("advanced", 4095), "{setting}");
        }
    }

    #[test]
    fn ray_tracing_is_on_only_when_asked_for() {
        for on in ["1", "on", "ON", "tier1_0", "tier1_1", "11"] {
            assert!(raytracing_mode_on(on), "{on}");
        }
        for off in ["", "auto", "0", "off", "none", "2"] {
            assert!(!raytracing_mode_on(off), "{off}");
        }
    }

    #[test]
    fn raytracing_tier_parsing() {
        assert_eq!(parse_raytracing_tier("auto"), None);
        assert_eq!(parse_raytracing_tier(""), None);
        assert_eq!(parse_raytracing_tier("off"), Some(0));
        assert_eq!(parse_raytracing_tier("none"), Some(0));
        assert_eq!(parse_raytracing_tier("0"), Some(0));
        assert_eq!(parse_raytracing_tier("tier1_0"), Some(10));
        assert_eq!(parse_raytracing_tier("1.0"), Some(10));
        assert_eq!(parse_raytracing_tier("10"), Some(10));
        assert_eq!(parse_raytracing_tier("tier1_1"), Some(11));
        assert_eq!(parse_raytracing_tier("1.1"), Some(11));
        assert_eq!(parse_raytracing_tier("11"), Some(11));
    }
}
