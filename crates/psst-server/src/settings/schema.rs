use psst_proto::api::Applies;

pub struct Group {
    pub id: &'static str,
    pub title: &'static str,
    pub advanced: bool,
    pub fields: &'static [Spec],
}

pub struct Spec {
    /// The key's dotted path in the file.
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub applies: Applies,
    /// Why the web UI never changes it, for a key the layers lock (`Layerable::LOCKED`).
    pub read_only: Option<&'static str>,
}

pub enum Kind {
    Bool,
    Int {
        min: u64,
        max: u64,
        unit: &'static str,
    },
    Choice(&'static [(&'static str, &'static str)]),
    /// The server's profiles.
    Profiles,
    /// `none` and the server's post-processing styles.
    PostProcessing,
    Text {
        optional: bool,
    },
    Color,
    List,
}

const fn editable(key: &'static str, label: &'static str, help: &'static str, kind: Kind, applies: Applies) -> Spec {
    Spec { key, label, help, kind, applies, read_only: None }
}

const fn read_only(
    key: &'static str,
    label: &'static str,
    help: &'static str,
    kind: Kind,
    reason: &'static str,
) -> Spec {
    Spec { key, label, help, kind, applies: Applies::NextStart, read_only: Some(reason) }
}

const NIX_ADMIN: &str = "Set in your Nix config: from here, a stolen browser session could make itself an admin.";

pub const SERVER: &[Group] = &[
    Group {
        id: "dictation",
        title: "Dictation",
        advanced: false,
        fields: &[
            editable(
                "limits.max_recording_s",
                "Longest recording",
                "Clients stop recording at this length.",
                Kind::Int { min: 1, max: 3600, unit: "s" },
                Applies::NextDictation,
            ),
            editable(
                "limits.max_active_sessions",
                "Dictations at once",
                "More than this many at the same time are turned away until one finishes.",
                Kind::Int { min: 1, max: 64, unit: "" },
                Applies::NextDictation,
            ),
            editable(
                "load_deadline_s",
                "Wait for the model",
                "A dictation fails if the model isn't ready within this time.",
                Kind::Int { min: 1, max: 3600, unit: "s" },
                Applies::NextDictation,
            ),
        ],
    },
    Group {
        id: "model",
        title: "Speech model",
        advanced: false,
        fields: &[
            editable(
                "model.model",
                "Model",
                "The Whisper model to load, by name or Hugging Face repository.",
                Kind::Text { optional: false },
                Applies::ModelReload,
            ),
            editable(
                "model.preload",
                "Load at start",
                "Loads the model as the server starts, so the first dictation doesn't wait.",
                Kind::Bool,
                Applies::NextStart,
            ),
        ],
    },
    Group {
        id: "web",
        title: "Web UI and history",
        advanced: false,
        fields: &[
            editable(
                "web_url",
                "Web UI address",
                "Login links start with it; empty uses the address a device reached the server by.",
                Kind::Text { optional: true },
                Applies::Now,
            ),
            editable(
                "history_days",
                "Keep history",
                "Dictations older than this are deleted.",
                Kind::Int { min: 1, max: 3650, unit: "days" },
                Applies::Now,
            ),
        ],
    },
    Group {
        id: "access",
        title: "Access",
        advanced: false,
        fields: &[
            read_only(
                "listen",
                "Listens on",
                "Addresses devices and browsers reach the server at.",
                Kind::List,
                "Set in your Nix config: a typo here could lock you out of this page.",
            ),
            read_only(
                "allow_from",
                "Accepts connections from",
                "Networks devices and browsers may connect from. psst has no TLS, so trust each one.",
                Kind::List,
                "Set in your Nix config: a typo here could lock you out of this page.",
            ),
            read_only(
                "socket",
                "Socket",
                "The Unix socket for this machine's users.",
                Kind::Text { optional: false },
                "Set in your Nix config: a typo here could lock you out of this page.",
            ),
            read_only(
                "local_users",
                "Local users",
                "Users on this machine who may use the server, as admins.",
                Kind::List,
                NIX_ADMIN,
            ),
            read_only("auto_pair", "Pair by tailnet", "Tailnet machines that pair themselves.", Kind::List, NIX_ADMIN),
            read_only(
                "tailscale_socket",
                "tailscaled socket",
                "Where the server asks tailscaled which machine is calling.",
                Kind::Text { optional: false },
                "Set in your Nix config: it decides which machines pair themselves.",
            ),
            read_only(
                "eval_mode",
                "Eval mode",
                "Accepts eval requests and sends traces with every result.",
                Kind::Bool,
                "Set in your Nix config: it's for evals, not everyday use.",
            ),
        ],
    },
    Group {
        id: "engine",
        title: "Engine",
        advanced: true,
        fields: &[
            editable(
                "model.device",
                "Runs on",
                "Where the model runs.",
                Kind::Choice(&[("cuda", "GPU (CUDA)"), ("cpu", "CPU"), ("auto", "Automatic")]),
                Applies::ModelReload,
            ),
            editable(
                "model.compute_type",
                "Precision",
                "Lower precision uses less memory and runs faster, for a little accuracy. Pick one for where the model runs.",
                Kind::Choice(&[
                    ("int8_float16", "Balanced (GPU)"),
                    ("float16", "Full (GPU)"),
                    ("int8", "Light (CPU)"),
                    ("float32", "Full (CPU)"),
                    ("auto", "Automatic"),
                ]),
                Applies::ModelReload,
            ),
            editable(
                "model.revision",
                "Revision",
                "A commit of the model's repository; empty for the latest.",
                Kind::Text { optional: true },
                Applies::ModelReload,
            ),
            read_only(
                "model.engine",
                "Engine",
                "The program that runs the model.",
                Kind::Text { optional: false },
                "Set in your Nix config, with the engine program it names.",
            ),
            read_only(
                "model.command",
                "Engine command",
                "How the server starts the engine.",
                Kind::List,
                "Set in your Nix config: from here, a browser session could run any program on this machine.",
            ),
        ],
    },
    Group {
        id: "decoding",
        title: "Decoding",
        advanced: true,
        fields: &[
            editable(
                "model.options.beam_size",
                "Beam size",
                "How many guesses the model weighs at each step; more is slower and a little more accurate.",
                Kind::Int { min: 1, max: 20, unit: "" },
                Applies::NextDictation,
            ),
            editable(
                "model.options.batch_size",
                "Batch size",
                "How many pieces of a long recording are transcribed at once.",
                Kind::Int { min: 1, max: 64, unit: "" },
                Applies::NextDictation,
            ),
            editable(
                "model.options.word_timestamps",
                "Word timings",
                "Times each word, which the dictionary uses to judge replacements.",
                Kind::Bool,
                Applies::NextDictation,
            ),
            editable(
                "model.options.vad_filter",
                "Skip silence",
                "Leaves out silence before transcribing. The batched path needs it.",
                Kind::Bool,
                Applies::NextDictation,
            ),
            editable(
                "model.options.terms_as",
                "Dictionary terms",
                "How dictionary terms reach the model.",
                Kind::Choice(&[("prompt", "In the prompt"), ("hotwords", "As hotwords")]),
                Applies::NextDictation,
            ),
            editable(
                "model.options.path",
                "Decoding path",
                "How the engine transcribes.",
                Kind::Choice(&[("batched", "Batched")]),
                Applies::NextDictation,
            ),
        ],
    },
];

pub const CLIENT: &[Group] = &[
    Group {
        id: "dictation",
        title: "Dictation",
        advanced: false,
        fields: &[
            editable(
                "profile",
                "Profile",
                "The profile new dictations use; the quick menu switches it until this changes.",
                Kind::Profiles,
                Applies::NextDictation,
            ),
            editable(
                "post_processing",
                "Post-processing",
                "How an LLM tidies new dictations; the quick menu switches it until this changes.",
                Kind::PostProcessing,
                Applies::NextDictation,
            ),
            editable(
                "post_processing_context",
                "Tell it the app",
                "Post-processing shapes the text for the app you dictate into. A window's title tells Gmail from \
                 its browser, but can hold an email's subject.",
                Kind::Choice(&[("none", "No"), ("app", "The app"), ("window", "The app and window title")]),
                Applies::NextDictation,
            ),
            editable(
                "menu.double_tap_ms",
                "Double tap",
                "A second toggle this soon opens the quick menu; 0 turns double-tapping off.",
                Kind::Int { min: 0, max: 1000, unit: "ms" },
                Applies::Now,
            ),
        ],
    },
    Group {
        id: "audio",
        title: "Sound and microphone",
        advanced: false,
        fields: &[
            editable(
                "sounds",
                "Chimes",
                "Played as recording starts and stops, and when something goes wrong.",
                Kind::Choice(&[("snap", "Snap"), ("glass", "Glass"), ("off", "Off")]),
                Applies::Now,
            ),
            editable(
                "audio.input",
                "Microphone",
                "default, or a microphone's name as PipeWire lists it.",
                Kind::Text { optional: false },
                Applies::NextDictation,
            ),
            editable(
                "audio.codec",
                "Audio codec",
                "Opus sends a twentieth of the data, for slow links; this machine's own server always gets PCM.",
                Kind::Choice(&[("opus", "Opus"), ("pcm", "PCM")]),
                Applies::NextDictation,
            ),
        ],
    },
    Group {
        id: "overlay",
        title: "Overlay",
        advanced: false,
        fields: &[
            editable(
                "overlay.anchor",
                "Position",
                "The screen edge the overlay sits at.",
                Kind::Choice(&[("bottom", "Bottom"), ("top", "Top")]),
                Applies::NextDictation,
            ),
            editable(
                "overlay.margin",
                "Distance from the edge",
                "Space between the overlay and the screen edge.",
                Kind::Int { min: 0, max: 2000, unit: "px" },
                Applies::NextDictation,
            ),
            editable(
                "overlay.width",
                "Width",
                "The overlay's width.",
                Kind::Int { min: 1, max: 2000, unit: "px" },
                Applies::NextDictation,
            ),
            editable(
                "overlay.height",
                "Height",
                "The overlay's height.",
                Kind::Int { min: 1, max: 400, unit: "px" },
                Applies::NextDictation,
            ),
            editable(
                "overlay.background",
                "Background",
                "The overlay's background colour.",
                Kind::Color,
                Applies::NextDictation,
            ),
            editable(
                "overlay.foreground",
                "Text",
                "The colour of text and icons.",
                Kind::Color,
                Applies::NextDictation,
            ),
            editable(
                "overlay.accent",
                "Accent",
                "The colour of the recording dot and alerts.",
                Kind::Color,
                Applies::NextDictation,
            ),
        ],
    },
    Group {
        id: "connection",
        title: "Connection",
        advanced: false,
        fields: &[read_only(
            "server",
            "Server",
            "The server this device dictates to.",
            Kind::Text { optional: false },
            "Set in the device's client.toml: changing it here could strand the device.",
        )],
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use psst_config::{ClientConfig, ServerConfig};
    use serde_json::Value;

    use super::{CLIENT, Group, SERVER};

    fn leaves(value: &Value, prefix: &str, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
                    leaves(value, &path, out);
                }
            }
            _ => {
                out.insert(prefix.to_owned());
            }
        }
    }

    fn missing(config: Value, schema: &[Group], elsewhere: &[&str]) -> Vec<String> {
        let mut keys = BTreeSet::new();
        leaves(&config, "", &mut keys);
        let shown: BTreeSet<&str> = schema.iter().flat_map(|g| g.fields).map(|f| f.key).collect();
        keys.into_iter().filter(|k| !shown.contains(k.as_str()) && !elsewhere.contains(&k.as_str())).collect()
    }

    /// The web UI is where every setting is changed: a key the files gain needs a row here.
    #[test]
    fn every_config_key_has_a_row() {
        let server = serde_json::to_value(ServerConfig::default()).unwrap();
        assert_eq!(missing(server, SERVER, &[]), Vec::<String>::new(), "server.toml keys without a row");
        let client = serde_json::to_value(ClientConfig::default()).unwrap();
        // Paste rules have their own editor; `insert` is a test seam.
        assert_eq!(
            missing(client, CLIENT, &["apps", "insert"]),
            Vec::<String>::new(),
            "client.toml keys without a row"
        );
    }
}
