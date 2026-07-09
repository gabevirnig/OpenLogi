//! User configuration, persisted as TOML at the platform-standard config
//! path.
//!
//! Per-device state (button bindings, …) lives under the
//! [`Config::devices`] map, keyed by a stable physical-device identifier such
//! as `"receiver:abc123:slot:2"`. Schema migrations branch on
//! [`Config::schema_version`].

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::binding::{Action, Binding, ButtonId, GestureDirection, default_binding_for};
use crate::device::{Capabilities, DeviceKind, DeviceModelInfo};
use crate::paths::{self, PathsError};

/// The schema version the current build produces. Bumped on breaking layout
/// changes; readers branch on the parsed value before consuming the rest of
/// the file.
///
/// v3 changes the device map from model keys to physical-device keys. No v2
/// device entries are migrated because model-scoped settings cannot be assigned
/// safely when two identical devices exist.
///
/// v2 merged the per-device `button_bindings` + `gesture_bindings` maps into a
/// single `bindings: BTreeMap<ButtonId, Binding>`. A v1 file still loads (the
/// `RawDeviceConfig` shim folds the legacy fields) and self-heals to v2 on the
/// next save; [`Config::load_from_path`] rejects only versions *newer* than this
/// so a forward file fails loudly instead of silently losing bindings.
pub const SCHEMA_VERSION: u32 = 3;

/// Top-level config document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub schema_version: u32,
    /// Non-device-scoped preferences (autostart, tray, language, …).
    #[serde(default, skip_serializing_if = "AppSettings::is_default")]
    pub app_settings: AppSettings,
    /// Physical config key of the carousel-selected device, persisted so a
    /// restart restores the last view rather than always landing on the
    /// first paired device. `None` means "fall back to the first device".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_device: Option<String>,
    #[serde(default)]
    pub devices: BTreeMap<String, DeviceConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            app_settings: AppSettings::default(),
            selected_device: None,
            devices: BTreeMap::new(),
        }
    }
}

/// Light/dark appearance preference. `System` follows the OS appearance (the
/// historical behaviour); `Light` / `Dark` force a mode regardless of the OS.
/// Platform-free so the core crate stays GUI-agnostic — the GUI maps this onto
/// gpui-component's `ThemeMode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Follow the operating system's light/dark setting.
    #[default]
    System,
    /// Always use the light variant of the selected theme.
    Light,
    /// Always use the dark variant of the selected theme.
    Dark,
}

/// App-wide preferences not tied to any particular device.
///
/// All fields are `#[serde(default)]` so adding a new one is backward
/// compatible — old config files just keep the default for the new field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent on/off user preferences, not a state machine"
)]
pub struct AppSettings {
    /// When true, a macOS `LaunchAgent` plist at
    /// `~/Library/LaunchAgents/org.openlogi.openlogi.plist` is installed
    /// so the app starts on login (P2.2). The plist is reconciled with
    /// this field on every startup; flipping the flag and relaunching is
    /// enough to install / remove it.
    #[serde(default)]
    pub launch_at_login: bool,
    /// Opt-in update check (P2.8). **Off by default** to honour the
    /// README's "no telemetry, no auto-update poller" promise. When true,
    /// the app makes exactly one `HEAD /repos/AprilNEA/OpenLogi/releases/
    /// latest` request per launch and logs whether a newer version is
    /// available — no automatic download.
    #[serde(default)]
    pub check_for_updates: bool,
    /// Opt-in automatic install. When true *and* [`Self::check_for_updates`]
    /// surfaces a newer version, the GUI downloads and stages it in the
    /// background; the update is applied on the next restart (never mid-session,
    /// and never auto-relaunched). **Off by default** — it only acts after a
    /// check the user already opted into, and stays inert in unsigned dev builds
    /// where verification fails closed.
    #[serde(default)]
    pub auto_install_updates: bool,
    /// True once the first-run "check for updates?" prompt has been answered
    /// (either way), so it is never shown again. The prompt is how a
    /// privacy-conscious default of `check_for_updates = false` still lets a
    /// user opt in on first launch.
    #[serde(default)]
    pub update_prompt_seen: bool,
    /// Whether OpenLogi shows a macOS menu-bar (status item) icon — and, on
    /// Windows, the notification-area (tray) icon. `true` (default) → the
    /// agent is visible in the menu bar / tray; `false` → it runs with no
    /// visible presence (macOS additionally keeps the ordinary Dock icon
    /// while a window is open). Ignored on Linux.
    #[serde(default = "default_true")]
    pub show_in_menu_bar: bool,
    /// Whether the GUI automatically downloads device images from
    /// `assets.openlogi.org` when a device appears. `true` (default) keeps
    /// the current behavior; `false` makes no asset network requests at all
    /// (the app falls back to bundled art and the synthetic silhouette). A
    /// manual "Refresh assets" in Settings still fetches on demand regardless.
    #[serde(default = "default_true")]
    pub auto_download_assets: bool,
    /// UI language as a BCP-47-ish locale code matching the GUI's bundled
    /// locales (e.g. `"en"`, `"de"`, `"pt-BR"`, `"zh-CN"`, `"zh-TW"`; see the
    /// GUI's `i18n::SUPPORTED`). `None` means "follow the system locale", which
    /// the GUI resolves at startup. Stored here so a user's explicit choice
    /// survives restarts regardless of the OS setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Thumb-wheel responsiveness, on a [`MIN_THUMBWHEEL_SENSITIVITY`]–
    /// [`MAX_THUMBWHEEL_SENSITIVITY`] scale. It scales both the speed of the
    /// wheel's continuous horizontal scroll and how few rotation increments a
    /// custom wheel action needs to fire. [`DEFAULT_THUMBWHEEL_SENSITIVITY`]
    /// (the out-of-the-box value) means 1× scroll speed; the wheel is only
    /// diverted from native scrolling once this leaves the default.
    #[serde(default = "default_thumbwheel_sensitivity")]
    pub thumbwheel_sensitivity: i32,
    /// Light/dark appearance preference. Defaults to following the OS.
    #[serde(default)]
    pub appearance: Appearance,
    /// Name of the theme used in light mode (a [`crate`]-agnostic string
    /// matching a gpui-component theme, e.g. `"OpenLogi Light"`). `None` uses
    /// the OpenLogi brand light theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_light: Option<String>,
    /// Name of the theme used in dark mode. `None` uses the OpenLogi brand dark
    /// theme.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme_dark: Option<String>,
    /// Corner-radius override for the UI, in pixels (the Appearance page offers
    /// `0` / `6` / `12`). `None` keeps each theme's own radius.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_radius: Option<u8>,
}

/// Out-of-the-box [`AppSettings::thumbwheel_sensitivity`]. At this value the
/// wheel's horizontal scroll runs at 1× and the wheel is left to scroll
/// natively (no HID++ diversion) unless a binding diverges from its default.
pub const DEFAULT_THUMBWHEEL_SENSITIVITY: i32 = 14;
/// Lowest selectable [`AppSettings::thumbwheel_sensitivity`].
pub const MIN_THUMBWHEEL_SENSITIVITY: i32 = 1;
/// Highest selectable [`AppSettings::thumbwheel_sensitivity`].
pub const MAX_THUMBWHEEL_SENSITIVITY: i32 = 100;

impl AppSettings {
    /// `skip_serializing_if` helper: true when nothing diverges from the
    /// default, so empty settings don't clutter `config.toml`.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            launch_at_login: false,
            check_for_updates: false,
            auto_install_updates: false,
            update_prompt_seen: false,
            show_in_menu_bar: true,
            auto_download_assets: true,
            language: None,
            thumbwheel_sensitivity: DEFAULT_THUMBWHEEL_SENSITIVITY,
            appearance: Appearance::System,
            theme_light: None,
            theme_dark: None,
            ui_radius: None,
        }
    }
}

/// serde default for [`AppSettings::show_in_menu_bar`]: `true`, so the menu-bar
/// icon is on out of the box and configs predating the field keep that behavior.
fn default_true() -> bool {
    true
}

/// serde default for [`AppSettings::thumbwheel_sensitivity`]: keeps configs
/// predating the field at the 1× default.
const fn default_thumbwheel_sensitivity() -> i32 {
    DEFAULT_THUMBWHEEL_SENSITIVITY
}

/// Per-device RGB lighting: a single static color, brightness, and on/off.
/// Deliberately basic — per-key effects are a later addition.
///
/// Crosses the agent↔GUI IPC (`set_lighting`), so field order is wire format —
/// changes require a `PROTOCOL_VERSION` bump (guarded by
/// `openlogi-agent-core/tests/wire_format.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lighting {
    #[serde(default = "default_lighting_enabled")]
    pub enabled: bool,
    /// Static color as 6 hex digits `"RRGGBB"` (no leading `#`).
    #[serde(default = "default_lighting_color")]
    pub color: String,
    /// Brightness percent, clamped to 0–100 on load.
    #[serde(
        default = "default_lighting_brightness",
        deserialize_with = "deserialize_brightness"
    )]
    pub brightness: u8,
}

impl Default for Lighting {
    fn default() -> Self {
        Self {
            enabled: default_lighting_enabled(),
            color: default_lighting_color(),
            brightness: default_lighting_brightness(),
        }
    }
}

fn default_lighting_enabled() -> bool {
    true
}

fn default_lighting_color() -> String {
    "ffffff".to_string()
}

fn default_lighting_brightness() -> u8 {
    100
}

/// Clamp a deserialized brightness into the UI's `0..=100` range, so a
/// hand-edited `config.toml` can't feed out-of-range values into the scaling
/// math (which assumes `brightness <= 100`).
fn deserialize_brightness<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(u8::deserialize(deserializer)?.min(100))
}

/// Scroll-wheel mode for [`SmartShift`]: free-spin or ratchet (clicky).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WheelMode {
    Free,
    Ratchet,
}

/// Per-device SmartShift wheel configuration, persisted so the agent can
/// re-apply it when the device reconnects: the values are written to device
/// RAM and do not survive a power cycle (#189), despite earlier assumptions
/// that the device kept them in NVM.
///
/// Config-file only — never crosses the IPC (the agent reads it from
/// `config.toml` on reload), so it is free to evolve without a
/// `PROTOCOL_VERSION` bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartShift {
    pub mode: WheelMode,
    /// SmartShift auto-disengage threshold (`0x01`–`0xFE`, in 0.25 turn/s
    /// steps), or `0xFF` for a permanently engaged ratchet.
    pub auto_disengage: u8,
    /// Tunable-torque force percentage (`1`–`100`), `0` when the device
    /// doesn't support tunable torque.
    pub tunable_torque: u8,
}

/// Last-known identity of a device, captured while it was online so the UI can
/// render its card and the *correct* config panels before any live HID++ probe
/// completes — or while the device is asleep and can't be probed at all.
///
/// Every field is a **static property of the model**, not of the current
/// connection: an MX Master 3S has adjustable DPI whether or not it is awake.
/// That is what makes this safe to persist — it never goes stale. It is also
/// free of any per-unit identifier (no serial number, no unit id), so caching
/// it adds no privacy surface beyond the `config_key` already used as the map
/// key. Persisting identity is what stops a sleeping/just-booted mouse from
/// vanishing from the device list (and losing its Pointer/Buttons panels)
/// until a cold probe happens to win its race — see issue #159.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// The name shown in the carousel, as resolved from the asset registry the
    /// last time the device was online.
    pub display_name: String,
    /// HID++ model identity from feature 0x0003, when available. Persisted so
    /// the GUI can resolve the same curated asset while the device is asleep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_info: Option<DeviceModelInfo>,
    /// Firmware codename, when available. Used as an asset-resolution hint and
    /// as a readable fallback for devices without curated model metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codename: Option<String>,
    /// The device's resolved [`DeviceKind`] (asset registry preferred, HID++
    /// classification as fallback).
    pub kind: DeviceKind,
    /// Configuration capabilities measured from the device's HID++ feature
    /// table. This is the field that keeps a sleeping mouse's panels visible.
    pub capabilities: Capabilities,
}

/// Settings scoped to a single physical device.
///
/// Deserialization goes through `RawDeviceConfig` (`#[serde(from)]`) so
/// pre-v2 files — which split bindings across `button_bindings` +
/// `gesture_bindings` — fold into the unified [`Self::bindings`] map. Only
/// `bindings` is ever serialized, so a migrated file self-heals to the v2 shape
/// on its next save.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "RawDeviceConfig")]
pub struct DeviceConfig {
    /// Last-known identity (name / kind / capabilities), captured while the
    /// device was online. Lets the UI render this device — with the right
    /// config panels — on a cold start before any probe, or while it sleeps.
    /// `None` for configs written before this field existed or by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<DeviceIdentity>,
    /// Every rebindable button's binding: a single [`Action`], or — for any
    /// gesture-capable button (the dedicated HID++ gesture button and the
    /// OS-hook Middle/Back/Forward) — a [`Binding::Gesture`] per-direction map.
    /// A button is "in gesture mode" exactly when its binding is a
    /// [`Binding::Gesture`]; multiple buttons can be gesture buttons at once.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<ButtonId, Binding>,
    /// Per-application binding overlays (P1.4). Keyed by bundle identifier
    /// (e.g. `"com.microsoft.VSCode"` on macOS). When the foreground app's
    /// id matches a key here, those bindings take precedence; anything not
    /// listed falls through to `bindings`. Deliberately `Action`-valued (not
    /// `Binding`): a per-app override replaces the whole button with one
    /// action, never a per-direction gesture overlay.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub per_app_bindings: BTreeMap<String, BTreeMap<ButtonId, Action>>,
    /// Ordered list of DPI presets cycled through by
    /// [`Action::CycleDpiPresets`] and indexed by
    /// [`Action::SetDpiPreset`]. Empty means "no presets configured" —
    /// the cycle action becomes a no-op until the user adds at least one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dpi_presets: Vec<u32>,
    /// The sensor DPI the user committed for this device. Persisted because
    /// the value lives in device RAM and resets on a power cycle (#189); the
    /// agent re-applies it when the device reconnects. `None` until the user
    /// first changes DPI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi: Option<u32>,
    /// Per-device RGB lighting (static color + brightness + on/off). `None`
    /// until the user changes it, so it stays out of `config.toml` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lighting: Option<Lighting>,
    /// Per-device SmartShift wheel configuration, re-applied on reconnect for
    /// the same reason as [`Self::dpi`]. `None` until the user changes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smartshift: Option<SmartShift>,
    /// Invert this device's scroll-wheel direction relative to the OS setting
    /// (issue #126): on, a wheel tick scrolls the opposite way, so a user who
    /// keeps macOS "natural scrolling" for the trackpad can have a traditional
    /// "reverse" wheel on the mouse. Vertical only; the agent applies it through
    /// the device's HID++ native wheel-inversion mode when supported. `false`
    /// (default) is the native direction, and is omitted from `config.toml`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub invert_scroll: bool,
}

/// `skip_serializing_if` helper for plain `bool` fields whose default is
/// `false`: keeps an unset toggle out of `config.toml` entirely.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if requires a fn(&T) -> bool signature"
)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Deserialize-only shim that folds the pre-v2 `button_bindings` +
/// `gesture_bindings` fields into [`DeviceConfig::bindings`]. Never serialized
/// (only [`DeviceConfig`] is), so reading a legacy file and saving rewrites it
/// in the v2 shape.
#[derive(Deserialize)]
struct RawDeviceConfig {
    // A legacy `gesture_owner` scalar (from the single-gesture-button era) may
    // still be present in older configs; it is simply ignored now that gesture
    // mode is derived per-button from each [`Binding::Gesture`]. serde drops
    // unknown fields, so no explicit field is needed to tolerate it.
    #[serde(default)]
    identity: Option<DeviceIdentity>,
    /// v2 shape — present on already-migrated files; wins on any key collision.
    #[serde(default)]
    bindings: BTreeMap<ButtonId, Binding>,
    /// Legacy v1 per-button single bindings.
    #[serde(default)]
    button_bindings: BTreeMap<ButtonId, Action>,
    /// Legacy v1 flat gesture map (implicitly the gesture button's directions).
    #[serde(default)]
    gesture_bindings: BTreeMap<GestureDirection, Action>,
    #[serde(default)]
    per_app_bindings: BTreeMap<String, BTreeMap<ButtonId, Action>>,
    #[serde(default)]
    dpi_presets: Vec<u32>,
    #[serde(default)]
    dpi: Option<u32>,
    #[serde(default)]
    lighting: Option<Lighting>,
    #[serde(default)]
    smartshift: Option<SmartShift>,
    #[serde(default)]
    invert_scroll: bool,
}

impl From<RawDeviceConfig> for DeviceConfig {
    fn from(raw: RawDeviceConfig) -> Self {
        let mut bindings = raw.bindings; // the v2 map wins on every key.

        // Re-home the legacy flat gesture map under `GestureButton`. This MUST
        // happen before folding `button_bindings`, so a legacy single
        // `button_bindings[GestureButton]` entry coexisting with a
        // `gesture_bindings` map cannot claim the slot first and silently drop
        // the whole direction map (the pre-v2 rule was "gesture entries win").
        if !raw.gesture_bindings.is_empty() {
            bindings
                .entry(ButtonId::GestureButton)
                .or_insert_with(|| Binding::Gesture(raw.gesture_bindings));
        }
        for (button, action) in raw.button_bindings {
            // A legacy `button_bindings[GestureButton]` is vestigial and must not
            // become a `Binding::Single`: the gesture button never dispatched
            // through the per-button map (it is not an OS-hook button, and its
            // plain press routes through the gesture `Click` slot — see
            // agent-core `bindings_for`). A `Single` here would be unreachable —
            // the GUI hides it and the runtime ignores it — while folding it into
            // `Click` would resurrect a dead binding as a behavior change. Drop
            // it: the gesture map (re-homed above) already owns this button, and
            // an absent entry falls back to the canonical default, exactly as
            // pre-v2.
            if button == ButtonId::GestureButton {
                continue;
            }
            bindings.entry(button).or_insert(Binding::Single(action));
        }

        DeviceConfig {
            identity: raw.identity,
            bindings,
            per_app_bindings: raw.per_app_bindings,
            dpi_presets: raw.dpi_presets,
            dpi: raw.dpi,
            lighting: raw.lighting,
            smartshift: raw.smartshift,
            invert_scroll: raw.invert_scroll,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not resolve config path")]
    Path(#[from] PathsError),
    #[error("could not read config at {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse config at {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("could not write config at {path}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not serialize config")]
    Serialize(#[from] toml::ser::Error),
    #[error("config at {path} has unsupported schema_version {found}")]
    UnsupportedSchemaVersion { path: PathBuf, found: u32 },
}

#[allow(
    clippy::result_large_err,
    reason = "Config I/O keeps rich parse/write context and is not a hot path"
)]
impl Config {
    /// Loads the config from the default user path, returning
    /// [`Config::default`] if the file does not exist yet.
    pub fn load_or_default() -> Result<Self, ConfigError> {
        Self::load_from_path(&paths::config_path()?)
    }

    /// Same as [`Self::load_or_default`] but reads from `path`. Used by tests
    /// to avoid touching the real user config.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let mut config: Self =
                    toml::from_str(&text).map_err(|source| ConfigError::Parse {
                        path: path.to_path_buf(),
                        source,
                    })?;
                // Accept any version up to the current one: older files migrate
                // through the per-device [`RawDeviceConfig`] shim and self-heal on
                // the next save. Only a *newer* file is rejected — loudly, so a
                // downgraded binary refuses to load (and silently wipe) a config
                // it can't represent.
                if config.schema_version > SCHEMA_VERSION {
                    return Err(ConfigError::UnsupportedSchemaVersion {
                        path: path.to_path_buf(),
                        found: config.schema_version,
                    });
                }
                // Stamp the in-memory doc to the current version so a re-save
                // writes the migrated v2 shape (the device shim already folded
                // the legacy fields during deserialize).
                config.schema_version = SCHEMA_VERSION;
                Ok(config)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Writes the config atomically to the default user path: serialize to a
    /// sibling temp file, then rename over the target. On Unix the temp file
    /// is created with mode 0600.
    pub fn save_atomic(&self) -> Result<(), ConfigError> {
        self.save_to_path(&paths::config_path()?)
    }

    /// Same as [`Self::save_atomic`] but writes to `path`. Used by tests.
    pub fn save_to_path(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        }
        let body = toml::to_string_pretty(self)?;
        write_atomic(path, body.as_bytes()).map_err(|source| ConfigError::Write {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Returns the bindings stored for `device_key`, or an empty map if the
    /// device has no committed bindings yet.
    #[must_use]
    pub fn bindings_for(&self, device_key: &str) -> BTreeMap<ButtonId, Binding> {
        self.devices
            .get(device_key)
            .map(|d| d.bindings.clone())
            .unwrap_or_default()
    }

    /// Records `binding` for `button` on `device_key`, creating the device
    /// entry if needed. Replaces the whole binding (use
    /// [`Self::set_gesture_direction`] to edit one direction of a gesture
    /// binding in place).
    pub fn set_binding(&mut self, device_key: &str, button: ButtonId, binding: Binding) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .bindings
            .insert(button, binding);
    }

    /// Returns the gesture sub-bindings for `device_key`'s gesture button, or an
    /// empty map if it isn't in gesture mode. Derived from the unified
    /// [`DeviceConfig::bindings`]; kept as a convenience for the agent-side
    /// per-direction adapter.
    #[must_use]
    pub fn gesture_bindings_for(&self, device_key: &str) -> BTreeMap<GestureDirection, Action> {
        match self
            .devices
            .get(device_key)
            .and_then(|d| d.bindings.get(&ButtonId::GestureButton))
        {
            Some(Binding::Gesture(map)) => map.clone(),
            _ => BTreeMap::new(),
        }
    }

    /// Records `action` for one `direction` of `button`'s gesture binding,
    /// creating the device entry if needed.
    ///
    /// A button with no binding yet is seeded from its canonical
    /// [`default_binding_for`] — for [`ButtonId::GestureButton`] that is the full
    /// default direction map (including a [`GestureDirection::Click`]), so the
    /// merged map never persists a gesture binding whose click projection is a
    /// no-op. A prior [`Binding::Single`] is upgraded to [`Binding::Gesture`],
    /// preserving its action as the `Click` entry.
    pub fn set_gesture_direction(
        &mut self,
        device_key: &str,
        button: ButtonId,
        direction: GestureDirection,
        action: Action,
    ) {
        if let Binding::Gesture(map) = self.ensure_gesture_binding(device_key, button) {
            map.insert(direction, action);
        }
    }

    /// Ensure `button` on `device_key` is a [`Binding::Gesture`], creating the
    /// device + a default binding if needed and upgrading a [`Binding::Single`]
    /// in place (its action kept as the [`GestureDirection::Click`]). Returns the
    /// entry so the caller can finish it — seed every direction
    /// ([`Binding::fill_gesture_defaults`]) or set just one. Shared by
    /// [`Self::enable_gesture`] and [`Self::set_gesture_direction`] so the two
    /// promote a button into gesture mode identically.
    fn ensure_gesture_binding(&mut self, device_key: &str, button: ButtonId) -> &mut Binding {
        let entry = self
            .devices
            .entry(device_key.to_string())
            .or_default()
            .bindings
            .entry(button)
            .or_insert_with(|| default_binding_for(button));
        entry.upgrade_to_gesture();
        entry
    }

    /// Whether `button` on `device_key` is currently in gesture mode — i.e. its
    /// effective binding is a [`Binding::Gesture`]. Any number of buttons can be
    /// gesture buttons at once.
    ///
    /// A button with no stored binding falls back to its canonical
    /// [`default_binding_for`]: only the dedicated HID++ gesture button
    /// ([`ButtonId::GestureButton`]) defaults to a gesture binding, so it gestures
    /// out of the box while OS-hook buttons start as single actions. Turning a
    /// gesture button off (see [`Self::disable_gesture`]) stores an explicit
    /// [`Binding::Single`], which overrides that default.
    #[must_use]
    pub fn is_gesture_button(&self, device_key: &str, button: ButtonId) -> bool {
        match self.devices.get(device_key).and_then(|d| d.bindings.get(&button)) {
            Some(binding) => binding.is_gesture(),
            None => default_binding_for(button).is_gesture(),
        }
    }

    /// Every button on `device_key` currently in gesture mode, in
    /// [`ButtonId::ALL`] order. The set the runtime dispatches gestures for and
    /// the GUI highlights.
    #[must_use]
    pub fn gesture_buttons(&self, device_key: &str) -> Vec<ButtonId> {
        ButtonId::ALL
            .into_iter()
            .filter(|&button| self.is_gesture_button(device_key, button))
            .collect()
    }

    /// Put `button` into gesture mode on `device_key`, giving it a full
    /// five-direction [`Binding::Gesture`] map: a prior [`Binding::Single`] is
    /// kept as the [`GestureDirection::Click`] action, any existing swipe arms are
    /// preserved, and unbound directions are seeded from
    /// [`default_gesture_binding`](crate::binding::default_gesture_binding). A
    /// no-op shape when the button is already a gesture button. Independent of
    /// every other button — enabling one never demotes another.
    pub fn enable_gesture(&mut self, device_key: &str, button: ButtonId) {
        self.ensure_gesture_binding(device_key, button)
            .fill_gesture_defaults();
    }

    /// Take `button` out of gesture mode on `device_key`, demoting it to a
    /// [`Binding::Single`] of its current plain-click action so it stops driving
    /// swipe capture. A no-op when the button isn't a gesture button. Independent
    /// of every other button — the rest keep gesturing.
    pub fn disable_gesture(&mut self, device_key: &str, button: ButtonId) {
        if !self.is_gesture_button(device_key, button) {
            return;
        }
        // Materialize the effective click action (from the stored map, or the
        // canonical default for a default-on button that has no stored entry
        // yet) and store it as an explicit single binding — which reads as "off".
        let click = match self.devices.get(device_key).and_then(|d| d.bindings.get(&button)) {
            Some(binding) => binding.click_action(),
            None => default_binding_for(button).click_action(),
        };
        self.set_binding(device_key, button, Binding::Single(click));
    }

    /// Resolve the effective binding map for `device_key`, overlaying the
    /// per-app entry for `bundle_id` (if any) on top of the global per-device
    /// `bindings`. A per-app override replaces the whole button with a
    /// [`Binding::Single`]; everything else falls through.
    ///
    /// Returns an empty map when the device has no recorded bindings yet.
    /// Callers (the GUI / hook) layer their own defaults on top.
    #[must_use]
    pub fn effective_bindings(
        &self,
        device_key: &str,
        bundle_id: Option<&str>,
    ) -> BTreeMap<ButtonId, Binding> {
        let Some(device) = self.devices.get(device_key) else {
            return BTreeMap::new();
        };
        let mut out = device.bindings.clone();
        if let Some(bid) = bundle_id
            && let Some(overlay) = device.per_app_bindings.get(bid)
        {
            for (k, v) in overlay {
                out.insert(*k, Binding::Single(v.clone()));
            }
        }
        out
    }

    /// Records a per-app override. Creates the device + app entries as
    /// needed; passing an action of `None` removes the override and prunes
    /// the empty app map.
    pub fn set_per_app_binding(
        &mut self,
        device_key: &str,
        bundle_id: &str,
        button: ButtonId,
        action: Option<Action>,
    ) {
        let entry = self
            .devices
            .entry(device_key.to_string())
            .or_default()
            .per_app_bindings
            .entry(bundle_id.to_string())
            .or_default();
        match action {
            Some(a) => {
                entry.insert(button, a);
            }
            None => {
                entry.remove(&button);
            }
        }
        if let Some(d) = self.devices.get_mut(device_key) {
            d.per_app_bindings.retain(|_, m| !m.is_empty());
        }
    }

    /// HID++ config key of the carousel-selected device, if any.
    #[must_use]
    pub fn selected_device(&self) -> Option<&str> {
        self.selected_device.as_deref()
    }

    /// Update the carousel-selected device. Pass `None` to clear the
    /// selection (e.g. when the previously-selected device disappears).
    pub fn set_selected_device(&mut self, key: Option<String>) {
        self.selected_device = key;
    }

    /// The ordered DPI preset list for `device_key`, or an empty `Vec` if the
    /// device has none configured yet.
    #[must_use]
    pub fn dpi_presets(&self, device_key: &str) -> Vec<u32> {
        self.devices
            .get(device_key)
            .map(|d| d.dpi_presets.clone())
            .unwrap_or_default()
    }

    /// Replace the DPI preset list for `device_key`. Pass an empty `Vec` to
    /// clear (the device block is kept; the field is just omitted on save
    /// thanks to `skip_serializing_if`).
    pub fn set_dpi_presets(&mut self, device_key: &str, presets: Vec<u32>) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .dpi_presets = presets;
    }

    /// The last-known [`DeviceIdentity`] for `device_key`, or `None` if the
    /// device has never been seen online (or was configured before identities
    /// were recorded).
    #[must_use]
    pub fn device_identity(&self, device_key: &str) -> Option<&DeviceIdentity> {
        self.devices
            .get(device_key)
            .and_then(|d| d.identity.as_ref())
    }

    /// Record (or refresh) the identity captured for `device_key` while it was
    /// online, creating the device entry if needed.
    pub fn set_device_identity(&mut self, device_key: &str, identity: DeviceIdentity) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .identity = Some(identity);
    }

    /// Whether `device_key` has a non-empty per-app binding overlay for the
    /// foreground app `app` (bundle id). Drives the menu-bar popover's "override
    /// active" badge — when the current app has its own bindings for this
    /// device, the global bindings are (partly) overridden.
    #[must_use]
    pub fn has_app_override(&self, device_key: &str, app: &str) -> bool {
        self.devices.get(device_key).is_some_and(|d| {
            d.per_app_bindings
                .get(app)
                .is_some_and(|overlay| !overlay.is_empty())
        })
    }

    /// Iterate every device we've recorded an identity for, as
    /// `(config_key, identity)`. Used to seed offline placeholder cards so a
    /// known device stays visible (with its panels) before any live probe.
    pub fn known_identities(&self) -> impl Iterator<Item = (&str, &DeviceIdentity)> {
        self.devices
            .iter()
            .filter_map(|(k, d)| d.identity.as_ref().map(|i| (k.as_str(), i)))
    }

    /// The lighting config for `device_key`, or `None` if unset.
    #[must_use]
    pub fn lighting(&self, device_key: &str) -> Option<Lighting> {
        self.devices
            .get(device_key)
            .and_then(|d| d.lighting.clone())
    }

    /// Replace the lighting config for `device_key`.
    pub fn set_lighting(&mut self, device_key: &str, lighting: Lighting) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .lighting = Some(lighting);
    }

    /// The committed sensor DPI for `device_key`, or `None` if never set.
    #[must_use]
    pub fn dpi(&self, device_key: &str) -> Option<u32> {
        self.devices.get(device_key).and_then(|d| d.dpi)
    }

    /// Record the committed sensor DPI for `device_key`, so the agent can
    /// re-apply it when the device reconnects (#189).
    pub fn set_dpi(&mut self, device_key: &str, dpi: u32) {
        self.devices.entry(device_key.to_string()).or_default().dpi = Some(dpi);
    }

    /// The SmartShift wheel config for `device_key`, or `None` if never set.
    #[must_use]
    pub fn smartshift(&self, device_key: &str) -> Option<SmartShift> {
        self.devices.get(device_key).and_then(|d| d.smartshift)
    }

    /// Record the SmartShift wheel config for `device_key`, so the agent can
    /// re-apply it when the device reconnects (#189).
    pub fn set_smartshift(&mut self, device_key: &str, smartshift: SmartShift) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .smartshift = Some(smartshift);
    }

    /// Whether `device_key`'s scroll wheel is inverted (issue #126). `false`
    /// (the native direction) for an unconfigured or absent device.
    #[must_use]
    pub fn invert_scroll(&self, device_key: &str) -> bool {
        self.devices
            .get(device_key)
            .is_some_and(|d| d.invert_scroll)
    }

    /// Set whether `device_key`'s scroll wheel is inverted. The agent reads this
    /// on the next `ReloadConfig` and applies it in the OS hook.
    pub fn set_invert_scroll(&mut self, device_key: &str, invert: bool) {
        self.devices
            .entry(device_key.to_string())
            .or_default()
            .invert_scroll = invert;
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            io::Write::write_all(&mut f, bytes)?;
            f.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            io::Write::write_all(&mut f, bytes)?;
            f.sync_all()?;
        }
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "expect/unwrap are idiomatic in tests")]
mod tests {
    use super::*;
    use crate::binding::{default_binding, default_gesture_binding};

    fn write_and_read(config: &Config) -> Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        config.save_to_path(&path).expect("save");
        Config::load_from_path(&path).expect("load")
    }

    #[test]
    fn missing_file_yields_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nonexistent.toml");
        let cfg = Config::load_from_path(&path).expect("load");
        assert_eq!(cfg.schema_version, SCHEMA_VERSION);
        assert!(cfg.devices.is_empty());
    }

    #[test]
    fn lighting_roundtrips_per_device() {
        let mut cfg = Config::default();
        cfg.set_lighting(
            "g513",
            Lighting {
                enabled: true,
                color: "00aabb".to_string(),
                brightness: 75,
            },
        );
        let restored = write_and_read(&cfg);
        assert_eq!(
            restored.lighting("g513"),
            Some(Lighting {
                enabled: true,
                color: "00aabb".to_string(),
                brightness: 75,
            })
        );
        assert_eq!(restored.lighting("absent"), None);
    }

    #[test]
    fn dpi_roundtrips_per_device() {
        let mut cfg = Config::default();
        cfg.set_dpi("2b042", 1600);
        let restored = write_and_read(&cfg);
        assert_eq!(restored.dpi("2b042"), Some(1600));
        assert_eq!(restored.dpi("absent"), None);
    }

    #[test]
    fn smartshift_roundtrips_per_device() {
        let mut cfg = Config::default();
        cfg.set_smartshift(
            "2b042",
            SmartShift {
                mode: WheelMode::Ratchet,
                auto_disengage: 16,
                tunable_torque: 30,
            },
        );
        let restored = write_and_read(&cfg);
        assert_eq!(
            restored.smartshift("2b042"),
            Some(SmartShift {
                mode: WheelMode::Ratchet,
                auto_disengage: 16,
                tunable_torque: 30,
            })
        );
        assert_eq!(restored.smartshift("absent"), None);
    }

    #[test]
    fn invert_scroll_roundtrips_per_device() {
        let mut cfg = Config::default();
        // Default is the native direction for any device, present or not.
        assert!(!cfg.invert_scroll("2b042"));
        cfg.set_invert_scroll("2b042", true);
        let restored = write_and_read(&cfg);
        assert!(restored.invert_scroll("2b042"));
        assert!(!restored.invert_scroll("absent"));
    }

    #[test]
    fn default_invert_scroll_is_omitted_from_toml() {
        // A device block with only the default (false) invert_scroll must not
        // emit the field — `skip_serializing_if` keeps configs clean.
        let mut cfg = Config::default();
        cfg.set_binding("2b042", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.set_invert_scroll("2b042", false);
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("invert_scroll"),
            "default invert_scroll should be omitted: {body}"
        );
    }

    #[test]
    fn bindings_roundtrip_per_device() {
        let mut cfg = Config::default();
        cfg.set_binding("2b042", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.set_binding(
            "2b042",
            ButtonId::DpiToggle,
            Binding::Single(Action::CustomShortcut(crate::binding::KeyCombo {
                modifiers: crate::binding::KeyCombo::MOD_CMD,
                key_code: 0x23, // kVK_ANSI_P
                display: "⌘P".into(),
            })),
        );
        cfg.set_binding("4082d", ButtonId::Back, Binding::Single(Action::Paste));

        let parsed = write_and_read(&cfg);

        // Per-device isolation.
        let a = parsed.bindings_for("2b042");
        assert_eq!(a.get(&ButtonId::Back), Some(&Binding::Single(Action::Copy)));
        assert_eq!(
            a.get(&ButtonId::DpiToggle),
            Some(&Binding::Single(Action::CustomShortcut(
                crate::binding::KeyCombo {
                    modifiers: crate::binding::KeyCombo::MOD_CMD,
                    key_code: 0x23,
                    display: "⌘P".into(),
                }
            )))
        );

        let b = parsed.bindings_for("4082d");
        assert_eq!(
            b.get(&ButtonId::Back),
            Some(&Binding::Single(Action::Paste))
        );
        assert_eq!(b.len(), 1, "device b should only see its own bindings");

        // Unknown device returns empty map without panic.
        assert!(parsed.bindings_for("deadbeef").is_empty());
    }

    #[test]
    fn human_readable_toml_layout() {
        let mut cfg = Config::default();
        cfg.set_binding(
            "2b042",
            ButtonId::Back,
            Binding::Single(Action::BrowserBack),
        );
        let body = toml::to_string_pretty(&cfg).expect("serialize");

        // The key only contains [A-Za-z0-9_], so TOML emits it as a bare-word
        // table key (no surrounding quotes). The test asserts the observable
        // structure rather than locking in a specific quoting.
        assert!(body.contains("schema_version = 3"), "got: {body}");
        assert!(body.contains("[devices.2b042.bindings]"), "got: {body}");
        // A `Single` binding serializes byte-identically to the pre-v2 bare
        // `Action`, so the leaf line is unchanged.
        assert!(body.contains("Back = \"BrowserBack\""), "got: {body}");
    }

    #[test]
    fn dpi_presets_roundtrip_per_device() {
        let mut cfg = Config::default();
        cfg.set_dpi_presets("2b042", vec![800, 1600, 3200]);
        cfg.set_dpi_presets("4082d", vec![400, 1600]);

        let parsed = write_and_read(&cfg);

        assert_eq!(parsed.dpi_presets("2b042"), vec![800, 1600, 3200]);
        assert_eq!(parsed.dpi_presets("4082d"), vec![400, 1600]);
        assert!(parsed.dpi_presets("unknown").is_empty());
    }

    #[test]
    fn empty_dpi_presets_skip_serialization() {
        let mut cfg = Config::default();
        // Add a binding so the device block exists.
        cfg.set_binding("2b042", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.set_dpi_presets("2b042", vec![800]);
        cfg.set_dpi_presets("2b042", vec![]); // clear

        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("dpi_presets"),
            "empty dpi_presets should be omitted: {body}"
        );
    }

    #[test]
    fn device_identity_roundtrips_and_is_iterable() {
        use crate::device::{Capabilities, DeviceKind};

        let mut cfg = Config::default();
        let mouse = DeviceIdentity {
            display_name: "MX Master 3S".to_string(),
            model_info: None,
            codename: None,
            kind: DeviceKind::Mouse,
            capabilities: Capabilities {
                buttons: true,
                pointer: true,
                lighting: false,
                scroll_inversion: false,
            },
        };
        cfg.set_device_identity("2b034", mouse.clone());
        // Recording an identity must not disturb unrelated per-device state.
        cfg.set_binding(
            "2b034",
            ButtonId::Back,
            Binding::Single(Action::BrowserBack),
        );

        let parsed = write_and_read(&cfg);
        assert_eq!(parsed.device_identity("2b034"), Some(&mouse));
        assert_eq!(parsed.device_identity("absent"), None);
        assert_eq!(
            parsed.bindings_for("2b034").get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack)),
            "identity must coexist with bindings on the same device block"
        );
        assert_eq!(
            parsed.known_identities().collect::<Vec<_>>(),
            vec![("2b034", &mouse)]
        );
    }

    #[test]
    fn selected_device_roundtrips() {
        let mut cfg = Config::default();
        assert_eq!(cfg.selected_device(), None);
        cfg.set_selected_device(Some("2b042".into()));
        let parsed = write_and_read(&cfg);
        assert_eq!(parsed.selected_device(), Some("2b042"));
    }

    #[test]
    fn per_app_overlay_takes_precedence() {
        let mut cfg = Config::default();
        cfg.set_binding(
            "2b042",
            ButtonId::Back,
            Binding::Single(Action::BrowserBack),
        );
        cfg.set_binding(
            "2b042",
            ButtonId::Forward,
            Binding::Single(Action::BrowserForward),
        );
        cfg.set_per_app_binding(
            "2b042",
            "com.microsoft.VSCode",
            ButtonId::Back,
            Some(Action::Undo),
        );

        // Global: both buttons are browser nav.
        let global = cfg.effective_bindings("2b042", None);
        assert_eq!(
            global.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
        assert_eq!(
            global.get(&ButtonId::Forward),
            Some(&Binding::Single(Action::BrowserForward))
        );

        // VSCode: Back overridden (wrapped as Single), Forward inherits.
        let vscode = cfg.effective_bindings("2b042", Some("com.microsoft.VSCode"));
        assert_eq!(
            vscode.get(&ButtonId::Back),
            Some(&Binding::Single(Action::Undo))
        );
        assert_eq!(
            vscode.get(&ButtonId::Forward),
            Some(&Binding::Single(Action::BrowserForward))
        );

        // Unrelated app falls through.
        let other = cfg.effective_bindings("2b042", Some("com.apple.Safari"));
        assert_eq!(
            other.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
    }

    #[test]
    fn per_app_binding_removal_prunes_empty_app() {
        let mut cfg = Config::default();
        cfg.set_per_app_binding(
            "2b042",
            "com.example.App",
            ButtonId::Back,
            Some(Action::Copy),
        );
        cfg.set_per_app_binding("2b042", "com.example.App", ButtonId::Back, None);
        assert!(
            cfg.devices["2b042"].per_app_bindings.is_empty(),
            "removing last override should prune the app entry"
        );
    }

    #[test]
    fn app_settings_default_omits_block() {
        let cfg = Config::default();
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("app_settings"),
            "default app_settings should be omitted: {body}"
        );
    }

    #[test]
    fn app_settings_launch_at_login_roundtrips() {
        let mut cfg = Config::default();
        cfg.app_settings.launch_at_login = true;
        let parsed = write_and_read(&cfg);
        assert!(parsed.app_settings.launch_at_login);
    }

    #[test]
    fn cleared_selected_device_omits_field() {
        let mut cfg = Config::default();
        cfg.set_selected_device(Some("2b042".into()));
        cfg.set_selected_device(None);
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("selected_device"),
            "cleared selection should not appear: {body}"
        );
    }

    #[test]
    fn empty_device_block_is_skipped_in_output() {
        // Inserting then clearing should not leave a [devices."x"] header
        // with no bindings under it (skip_serializing_if on bindings).
        let mut cfg = Config::default();
        cfg.set_binding("2b042", ButtonId::Back, Binding::Single(Action::Copy));
        cfg.devices
            .get_mut("2b042")
            .expect("entry")
            .bindings
            .clear();
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(
            !body.contains("Back"),
            "cleared bindings should not appear: {body}"
        );
    }

    #[test]
    fn migrates_v1_button_and_gesture_bindings() {
        // A pre-v2 file: split button_bindings + a flat gesture_bindings map.
        let v1 = "\
schema_version = 1

[devices.2b042.button_bindings]
Back = \"BrowserBack\"

[devices.2b042.gesture_bindings]
Up = \"Copy\"
Click = \"Paste\"
";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, v1).expect("write");

        // v1 still loads (version <= current) and folds into the merged map.
        let cfg = Config::load_from_path(&path).expect("load v1");
        let bindings = cfg.bindings_for("2b042");
        assert_eq!(
            bindings.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
        let mut gesture = BTreeMap::new();
        gesture.insert(GestureDirection::Up, Action::Copy);
        gesture.insert(GestureDirection::Click, Action::Paste);
        assert_eq!(
            bindings.get(&ButtonId::GestureButton),
            Some(&Binding::Gesture(gesture))
        );

        // Saving self-heals to the current shape: stamped version + merged table,
        // legacy field names gone.
        let body = toml::to_string_pretty(&cfg).expect("serialize");
        assert!(body.contains("schema_version = 3"), "got: {body}");
        assert!(body.contains("[devices.2b042.bindings]"), "got: {body}");
        assert!(!body.contains("button_bindings"), "got: {body}");
        assert!(!body.contains("gesture_bindings"), "got: {body}");
    }

    #[test]
    fn migration_gesture_map_wins_over_legacy_single_gesture_button_entry() {
        // The data-loss guard: when a legacy single button_bindings[GestureButton]
        // entry coexists with a gesture_bindings map (reachable via hand-edited
        // or very old configs), the gesture map must survive — not be shadowed by
        // the single entry. Mirrors the pre-v2 "gesture entries win" rule.
        let v1 = "\
schema_version = 1

[devices.2b042.button_bindings]
GestureButton = \"MissionControl\"

[devices.2b042.gesture_bindings]
Up = \"Copy\"
Down = \"Paste\"
";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, v1).expect("write");

        let cfg = Config::load_from_path(&path).expect("load v1");
        let mut gesture = BTreeMap::new();
        gesture.insert(GestureDirection::Up, Action::Copy);
        gesture.insert(GestureDirection::Down, Action::Paste);
        assert_eq!(
            cfg.bindings_for("2b042").get(&ButtonId::GestureButton),
            Some(&Binding::Gesture(gesture)),
            "gesture map must win over the legacy single GestureButton entry"
        );
    }

    #[test]
    fn migration_drops_vestigial_lone_gesture_button_single() {
        // A v1 file with only `button_bindings[GestureButton]` and no
        // `gesture_bindings` (the pre-gesture-picker shape). That entry never
        // dispatched in v1 — the gesture button's plain press routes through the
        // gesture `Click` slot, not the per-button map — so migrating it to a
        // `Binding::Single` would leave an unreachable entry the GUI hides and the
        // runtime ignores. It must be dropped, not shadow the gesture path.
        let v1 = "\
schema_version = 1

[devices.2b042.button_bindings]
GestureButton = \"MissionControl\"
Back = \"BrowserBack\"
";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, v1).expect("write");

        let bindings = Config::load_from_path(&path)
            .expect("load v1")
            .bindings_for("2b042");
        // An ordinary button still migrates to a `Single`...
        assert_eq!(
            bindings.get(&ButtonId::Back),
            Some(&Binding::Single(Action::BrowserBack))
        );
        // ...but the vestigial gesture-button single is gone, leaving the button
        // to fall back to its canonical default rather than an unreachable entry.
        assert_eq!(bindings.get(&ButtonId::GestureButton), None);
    }

    #[test]
    fn rejects_newer_schema_version_but_accepts_v1() {
        // A future version is rejected loudly; the current and older versions
        // load (older ones migrate through the shim).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "schema_version = 99\n").expect("write");
        assert!(matches!(
            Config::load_from_path(&path).expect_err("v99 should fail"),
            ConfigError::UnsupportedSchemaVersion { found: 99, .. }
        ));

        fs::write(&path, "schema_version = 1\n").expect("write");
        assert!(
            Config::load_from_path(&path).is_ok(),
            "v1 should still load"
        );
    }

    #[test]
    fn set_gesture_direction_upgrades_single_to_gesture() {
        let mut cfg = Config::default();
        // Start from a Single binding, then bind a swipe direction.
        cfg.set_binding(
            "2b042",
            ButtonId::Back,
            Binding::Single(Action::BrowserBack),
        );
        cfg.set_gesture_direction("2b042", ButtonId::Back, GestureDirection::Up, Action::Copy);

        match cfg.bindings_for("2b042").get(&ButtonId::Back) {
            Some(Binding::Gesture(map)) => {
                // The prior single action is preserved as the Click entry.
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&Action::BrowserBack)
                );
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::Copy));
            }
            other => panic!("expected Gesture after upgrade, got {other:?}"),
        }
    }

    #[test]
    fn set_gesture_direction_on_fresh_gesture_button_seeds_click() {
        // Binding one direction on a never-configured gesture button must still
        // persist a `Click`, so the click projection is the canonical default
        // rather than `Action::None` (which reads as a no-op press).
        let mut cfg = Config::default();
        cfg.set_gesture_direction(
            "2b042",
            ButtonId::GestureButton,
            GestureDirection::Up,
            Action::Copy,
        );

        match cfg.bindings_for("2b042").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(map.get(&GestureDirection::Up), Some(&Action::Copy));
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&crate::binding::default_gesture_binding(
                        GestureDirection::Click
                    )),
                    "a fresh gesture button must seed a Click from its default"
                );
            }
            other => panic!("expected Gesture, got {other:?}"),
        }
    }

    #[test]
    fn gesture_button_defaults_to_hidpp_button_only() {
        let cfg = Config::default();
        // Out of the box (no config), only the dedicated HID++ gesture button is
        // in gesture mode; OS-hook buttons start as single actions.
        assert!(cfg.is_gesture_button("2b042", ButtonId::GestureButton));
        assert!(!cfg.is_gesture_button("2b042", ButtonId::Back));
        assert!(!cfg.is_gesture_button("2b042", ButtonId::Forward));
        assert_eq!(cfg.gesture_buttons("2b042"), vec![ButtonId::GestureButton]);
    }

    #[test]
    fn enable_gesture_supports_multiple_buttons_at_once() {
        let mut cfg = Config::default();
        // Enable two OS-hook buttons alongside the default HID++ gesture button —
        // no single-owner lock, so all three gesture simultaneously.
        cfg.set_binding("2b042", ButtonId::Back, Action::BrowserBack.into());
        cfg.enable_gesture("2b042", ButtonId::Back);
        cfg.enable_gesture("2b042", ButtonId::Forward);

        assert!(cfg.is_gesture_button("2b042", ButtonId::GestureButton));
        assert!(cfg.is_gesture_button("2b042", ButtonId::Back));
        assert!(cfg.is_gesture_button("2b042", ButtonId::Forward));
        assert_eq!(
            cfg.gesture_buttons("2b042"),
            vec![ButtonId::Back, ButtonId::Forward, ButtonId::GestureButton],
            "gesture_buttons returns every gesture button in ButtonId::ALL order"
        );

        let bindings = cfg.bindings_for("2b042");
        // Back is a full five-direction gesture button: its prior single action
        // stays as Click, and the swipe arms are seeded from defaults.
        match bindings.get(&ButtonId::Back) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&Action::BrowserBack)
                );
                assert_eq!(
                    map.get(&GestureDirection::Up),
                    Some(&default_gesture_binding(GestureDirection::Up)),
                    "an enabled button gets full default arms"
                );
            }
            other => panic!("expected Back to be a gesture binding, got {other:?}"),
        }
    }

    #[test]
    fn enable_gesture_seeds_a_fresh_button_with_full_directions() {
        let mut cfg = Config::default();
        // The dedicated HID++ gesture button gets the full default direction map.
        cfg.enable_gesture("2b042", ButtonId::GestureButton);
        match cfg.bindings_for("2b042").get(&ButtonId::GestureButton) {
            Some(Binding::Gesture(map)) => {
                for dir in GestureDirection::ALL {
                    assert_eq!(map.get(&dir), Some(&default_gesture_binding(dir)));
                }
            }
            other => panic!("expected full default gesture map, got {other:?}"),
        }

        // A fresh OS-hook button also gets all five directions, not just a Click:
        // its native action stays as Click, and the swipe arms are defaults — so
        // the GUI's shown defaults are exactly what the runtime dispatches.
        cfg.enable_gesture("2b042", ButtonId::Forward);
        match cfg.bindings_for("2b042").get(&ButtonId::Forward) {
            Some(Binding::Gesture(map)) => {
                assert_eq!(
                    map.get(&GestureDirection::Click),
                    Some(&default_binding(ButtonId::Forward))
                );
                for dir in [
                    GestureDirection::Up,
                    GestureDirection::Down,
                    GestureDirection::Left,
                    GestureDirection::Right,
                ] {
                    assert_eq!(map.get(&dir), Some(&default_gesture_binding(dir)));
                }
            }
            other => panic!("expected full gesture map for Forward, got {other:?}"),
        }
    }

    #[test]
    fn disable_gesture_demotes_only_the_named_button() {
        let mut cfg = Config::default();
        cfg.enable_gesture("2b042", ButtonId::Back);
        cfg.enable_gesture("2b042", ButtonId::Forward);

        // Turning one off leaves the other (and the HID++ default) gesturing.
        cfg.disable_gesture("2b042", ButtonId::Back);
        assert!(!cfg.is_gesture_button("2b042", ButtonId::Back));
        assert!(cfg.is_gesture_button("2b042", ButtonId::Forward));
        assert!(cfg.is_gesture_button("2b042", ButtonId::GestureButton));

        // Back is now a plain single action (its former Click), not a gesture.
        assert!(matches!(
            cfg.bindings_for("2b042").get(&ButtonId::Back),
            Some(Binding::Single(_))
        ));
    }

    #[test]
    fn disable_gesture_can_turn_off_the_default_hidpp_button() {
        let mut cfg = Config::default();
        // The HID++ gesture button is on by default with no stored entry; turning
        // it off must store an explicit Single that overrides that default.
        assert!(cfg.is_gesture_button("2b042", ButtonId::GestureButton));
        cfg.disable_gesture("2b042", ButtonId::GestureButton);
        assert!(!cfg.is_gesture_button("2b042", ButtonId::GestureButton));
        assert!(cfg.gesture_buttons("2b042").is_empty());

        // The explicit demotion survives a save/load round-trip.
        let parsed = write_and_read(&cfg);
        assert!(!parsed.is_gesture_button("2b042", ButtonId::GestureButton));
    }

    #[test]
    fn gesture_mode_roundtrips_via_bindings() {
        let mut cfg = Config::default();
        cfg.set_binding("2b042", ButtonId::Back, Action::BrowserBack.into());
        cfg.enable_gesture("2b042", ButtonId::Back);
        let parsed = write_and_read(&cfg);
        assert!(parsed.is_gesture_button("2b042", ButtonId::Back));
        assert_eq!(
            parsed.gesture_buttons("2b042"),
            vec![ButtonId::Back, ButtonId::GestureButton]
        );
    }

    #[test]
    fn legacy_gesture_owner_field_is_ignored_not_fatal() {
        // A pre-existing `gesture_owner` scalar from the single-gesture-button era
        // must not fail the whole-document parse; it is simply dropped now that
        // gesture mode is derived per-button from each `Binding::Gesture`.
        let toml = "\
schema_version = 2

[devices.2b042]
gesture_owner = \"Back\"

[devices.2b042.bindings]
Back = \"Copy\"
";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, toml).expect("write");

        let cfg =
            Config::load_from_path(&path).expect("a legacy gesture_owner must not fail the load");
        // The rest of the device config survived...
        assert_eq!(
            cfg.bindings_for("2b042").get(&ButtonId::Back),
            Some(&Binding::Single(Action::Copy))
        );
        // ...and gesture mode is now derived purely from the bindings: Back is a
        // Single here, so only the default HID++ gesture button gestures.
        assert!(!cfg.is_gesture_button("2b042", ButtonId::Back));
        assert!(cfg.is_gesture_button("2b042", ButtonId::GestureButton));
    }
}
