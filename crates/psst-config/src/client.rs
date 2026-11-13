use std::{collections::BTreeMap, env, fmt, path::PathBuf, str::FromStr};

use psst_proto::{DEFAULT_SOCKET, Endpoint};
use serde::{Deserialize, Serialize};

use crate::{ConfigFile, DEFAULT_PROFILE};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    pub server: Endpoint,
    pub profile: String,
    pub sounds: Sounds,
    pub audio: AudioConfig,
    pub overlay: OverlayConfig,
    pub menu: MenuConfig,
    /// By Wayland app id.
    pub apps: BTreeMap<String, AppRule>,
    /// Test seam.
    #[serde(skip_serializing_if = "InsertConfig::is_default")]
    pub insert: InsertConfig,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            server: Endpoint::Unix(DEFAULT_SOCKET.into()),
            profile: DEFAULT_PROFILE.into(),
            sounds: Sounds::default(),
            audio: AudioConfig::default(),
            overlay: OverlayConfig::default(),
            menu: MenuConfig::default(),
            apps: BTreeMap::new(),
            insert: InsertConfig::default(),
        }
    }
}

impl ClientConfig {
    pub fn default_path() -> Option<PathBuf> {
        let config_home = env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(config_home.join("psst").join(Self::FILE_NAME))
    }

    pub fn paste_for(&self, app_id: Option<&str>) -> PasteRule {
        let rule = app_id.and_then(|id| self.apps.get(id));
        let fallback = match app_id {
            Some(id) if KNOWN_TERMINALS.contains(&id) => PasteAction::CtrlShiftV,
            _ => PasteAction::CtrlV,
        };
        PasteRule {
            action: rule.and_then(|r| r.paste).unwrap_or(fallback),
            delay_ms: rule.map_or(0, |r| r.paste_delay_ms),
        }
    }
}

pub const KNOWN_TERMINALS: &[&str] = &[
    "com.mitchellh.ghostty",
    "Alacritty",
    "kitty",
    "foot",
    "footclient",
    "org.wezfurlong.wezterm",
    "org.gnome.Console",
    "org.gnome.Ptyxis",
    "org.kde.konsole",
    "xterm",
];

/// The chimes as recording starts and stops, and on an error or when nothing was heard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SoundsValue", into = "SoundsValue")]
pub enum Sounds {
    /// Two bubbly pops.
    #[default]
    Snap,
    /// Two struck glass notes.
    Glass,
    Off,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SoundsValue {
    Named(String),
    Enabled(bool),
}

impl TryFrom<SoundsValue> for Sounds {
    type Error = String;

    fn try_from(value: SoundsValue) -> Result<Self, Self::Error> {
        match value {
            SoundsValue::Enabled(true) => Ok(Self::default()),
            SoundsValue::Enabled(false) => Ok(Self::Off),
            SoundsValue::Named(name) => match name.as_str() {
                "snap" => Ok(Self::Snap),
                "glass" => Ok(Self::Glass),
                "off" => Ok(Self::Off),
                _ => Err(format!("sounds `{name}`: expected \"snap\", \"glass\" or false")),
            },
        }
    }
}

impl From<Sounds> for SoundsValue {
    fn from(sounds: Sounds) -> Self {
        match sounds {
            Sounds::Snap => Self::Named("snap".into()),
            Sounds::Glass => Self::Named("glass".into()),
            Sounds::Off => Self::Enabled(false),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub input: AudioInput,
    pub codec: AudioCodec,
}

/// Opus by default: on the eval set its effect on accuracy can't be told apart from noise, though
/// no lossy codec passes the eval gate's zero-change tolerances (`psst_client::OpusSettings`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    /// 6 % of PCM's bytes (1.9 instead of 32.4 KB/s). A server from before Opus, and the Unix
    /// socket, always get PCM.
    #[default]
    Opus,
    Pcm,
}

/// `file:<path>` is a test seam: each recording replays the WAV file (16 kHz mono 16-bit) from
/// its start, paced in real time, then silence until the recording stops.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum AudioInput {
    #[default]
    Default,
    Device(String),
    File(PathBuf),
}

impl FromStr for AudioInput {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "" => Err("audio.input must not be empty".into()),
            "default" => Ok(Self::Default),
            _ => match s.strip_prefix("file:") {
                Some(path) if path.starts_with('/') => Ok(Self::File(path.into())),
                Some(_) => Err(format!("audio.input `{s}`: file: needs an absolute path")),
                None => Ok(Self::Device(s.into())),
            },
        }
    }
}

impl fmt::Display for AudioInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Device(name) => f.write_str(name),
            Self::File(path) => write!(f, "file:{}", path.display()),
        }
    }
}

impl TryFrom<String> for AudioInput {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<AudioInput> for String {
    fn from(input: AudioInput) -> Self {
        input.to_string()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InsertConfig {
    /// When set, the daemon never connects to Wayland: every command it would send the Wayland
    /// thread is appended to this file as one JSON line, and pastes and copies succeed at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<PathBuf>,
}

impl InsertConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Sizes are in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OverlayConfig {
    pub anchor: Anchor,
    pub margin: u32,
    pub width: u32,
    pub height: u32,
    pub background: Color,
    pub foreground: Color,
    pub accent: Color,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            anchor: Anchor::Bottom,
            margin: 130,
            width: 240,
            height: 44,
            background: Color { r: 0x00, g: 0x00, b: 0x00, a: 0xff },
            foreground: Color { r: 0xf5, g: 0xf5, b: 0xf7, a: 0xff },
            accent: Color { r: 0xff, g: 0x45, b: 0x3a, a: 0xff },
        }
    }
}

/// The quick menu, which `psst menu` opens too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MenuConfig {
    /// A second `toggle` this soon after the first opens the menu instead of a dictation. The
    /// microphone starts at the first, so nothing said is lost, but the chime and the overlay
    /// wait this long for a second. 0 turns double-tapping off: dictation starts at once.
    pub double_tap_ms: u32,
}

impl Default for MenuConfig {
    fn default() -> Self {
        Self { double_tap_ms: 150 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl FromStr for Color {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || format!("colour `{s}`: expected #rrggbb or #rrggbbaa");
        let hex = s.strip_prefix('#').filter(|h| matches!(h.len(), 6 | 8) && h.is_ascii()).ok_or_else(err)?;
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|_| err());
        Ok(Self { r: byte(0)?, g: byte(2)?, b: byte(4)?, a: if hex.len() == 8 { byte(6)? } else { 0xff } })
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { r, g, b, a } = self;
        match a {
            0xff => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            _ => write!(f, "#{r:02x}{g:02x}{b:02x}{a:02x}"),
        }
    }
}

impl TryFrom<String> for Color {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Color> for String {
    fn from(color: Color) -> Self {
        color.to_string()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paste: Option<PasteAction>,
    /// Pause between setting the clipboard and pressing the chord, for XWayland apps.
    pub paste_delay_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "PasteValue", into = "PasteValue")]
pub enum PasteAction {
    CtrlV,
    CtrlShiftV,
    /// Leaves the text on the clipboard: no paste, and no restoring what was there.
    CopyOnly,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PasteValue {
    Chord(String),
    Enabled(bool),
}

impl TryFrom<PasteValue> for PasteAction {
    type Error = String;

    fn try_from(value: PasteValue) -> Result<Self, Self::Error> {
        match value {
            PasteValue::Chord(chord) if chord == "ctrl+v" => Ok(Self::CtrlV),
            PasteValue::Chord(chord) if chord == "ctrl+shift+v" => Ok(Self::CtrlShiftV),
            PasteValue::Enabled(false) => Ok(Self::CopyOnly),
            _ => Err("paste must be \"ctrl+v\", \"ctrl+shift+v\" or false".into()),
        }
    }
}

impl From<PasteAction> for PasteValue {
    fn from(action: PasteAction) -> Self {
        match action {
            PasteAction::CtrlV => Self::Chord("ctrl+v".into()),
            PasteAction::CtrlShiftV => Self::Chord("ctrl+shift+v".into()),
            PasteAction::CopyOnly => Self::Enabled(false),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasteRule {
    pub action: PasteAction,
    pub delay_ms: u32,
}

impl ConfigFile for ClientConfig {
    const FILE_NAME: &'static str = "client.toml";

    fn check(&self) -> Result<(), String> {
        if self.profile.is_empty() {
            return Err("profile must not be empty".into());
        }
        if self.overlay.width == 0 || self.overlay.height == 0 {
            return Err("overlay.width and overlay.height must be at least 1".into());
        }
        if self.insert.record.as_ref().is_some_and(|path| !path.is_absolute()) {
            return Err("insert.record must be an absolute path".into());
        }
        Ok(())
    }
}
