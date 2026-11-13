//! A scripted engine that speaks the real protocol, for tests without a GPU.
//!
//! `psst-server __fake-engine <script.json>` runs [`main`]. The script ([`Script`]) decides what
//! `hello` announces, how `load` goes, and how each `transcribe` is answered. Every request the
//! engine receives is appended to `<script>.requests.jsonl` ([`request_log_path`]) as a
//! [`LoggedRequest`]. Every `transcribe` request received consumes the next scripted step, even
//! one that is then rejected; steps are counted through that log, so they carry on across engine
//! restarts: a script of `[exit, result]` crashes once and then succeeds.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
    thread,
    time::Duration,
};

use psst_proto::{EngineResult, EngineTrace, Segment, Word};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Bias, Capabilities, DecodeError, EngineError, EngineErrorCode, EngineTimings, FromEngine, Hello, Load, Loaded,
    PROTOCOL_ENV, PROTOCOL_VERSION, Progress, ToEngine, Transcribe, TranscribeResult, decode, encode,
    frame::{FrameError, FrameLimits, read_frame, write_frame},
    payload_to_samples,
};

/// The fake engine's behaviour.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Script {
    pub hello: HelloScript,
    pub load: LoadScript,
    /// Answers to `transcribe`, in order. Once they run out, every request gets
    /// [`TranscribeStep::default`].
    pub transcribe: Vec<TranscribeStep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HelloScript {
    pub protocol: u32,
    pub bias: bool,
    pub max_audio_s: u32,
}

impl Default for HelloScript {
    fn default() -> Self {
        Self { protocol: PROTOCOL_VERSION, bias: true, max_audio_s: 900 }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoadScript {
    /// Sent first, in order.
    pub progress: Vec<Progress>,
    /// Then this long a pause, which is also the reported `load_ms`.
    pub delay_ms: u64,
    pub outcome: LoadOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoadOutcome {
    /// Replies `loaded`. The revision defaults to the requested one, else `fake-revision`.
    Loaded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<String>,
    },
    Error {
        code: EngineErrorCode,
        message: String,
    },
    /// Exits with this status without replying.
    Exit {
        code: u8,
    },
    /// Never replies.
    Hang,
}

impl Default for LoadOutcome {
    fn default() -> Self {
        Self::Loaded { revision: None }
    }
}

/// How one `transcribe` is answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscribeStep {
    /// Replies `result` with `text` as one segment (after a leading space, as Whisper writes it).
    /// Empty text gives no segments and `speech_ms = 0`. Words are included when the request's
    /// `options.word_timestamps` is true.
    Result {
        text: String,
        #[serde(default)]
        delay_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        no_speech_prob: Option<f32>,
        /// Defaults to the audio's length, or 0 for empty text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speech_ms: Option<u32>,
    },
    Error {
        code: EngineErrorCode,
        message: String,
        #[serde(default)]
        retryable: bool,
        #[serde(default)]
        delay_ms: u64,
    },
    /// Exits with this status (a crash) without replying.
    Exit {
        code: u8,
        #[serde(default)]
        delay_ms: u64,
    },
    /// Never replies.
    Hang,
}

impl Default for TranscribeStep {
    fn default() -> Self {
        Self::Result { text: "fake transcript".into(), delay_ms: 0, no_speech_prob: None, speech_ms: None }
    }
}

/// One line of the request log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoggedRequest {
    /// The header as received.
    pub header: Value,
    /// Payload length in samples.
    pub samples: usize,
}

/// Where requests are logged for the script at `script`.
pub fn request_log_path(script: &Path) -> PathBuf {
    let mut name = script.file_stem().unwrap_or_default().to_os_string();
    name.push(".requests.jsonl");
    script.with_file_name(name)
}

/// Reads the request log of the script at `script`; empty if nothing was logged yet.
pub fn read_request_log(script: &Path) -> io::Result<Vec<LoggedRequest>> {
    let text = match fs::read_to_string(request_log_path(script)) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    text.lines().map(|line| serde_json::from_str(line).map_err(io::Error::other)).collect()
}

/// How [`run`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Shutdown,
    EndOfInput,
    /// A scripted `exit` step.
    Scripted(u8),
}

/// Entry point of `psst-server __fake-engine <script.json>`.
pub fn main(script_path: &Path) -> ExitCode {
    if std::env::var(PROTOCOL_ENV).as_deref() != Ok("1") {
        eprintln!("fake-engine: {PROTOCOL_ENV}=1 is not set; refusing to start");
        return ExitCode::from(2);
    }
    let script: Script = match fs::read(script_path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
    {
        Ok(script) => script,
        Err(err) => {
            eprintln!("fake-engine: cannot read {}: {err}", script_path.display());
            return ExitCode::from(2);
        }
    };
    let log = request_log_path(script_path);
    match run(&script, Some(&log), &mut io::stdin().lock(), &mut io::stdout().lock()) {
        Ok(Exit::Shutdown | Exit::EndOfInput) => ExitCode::SUCCESS,
        Ok(Exit::Scripted(code)) => ExitCode::from(code),
        Err(err) => {
            eprintln!("fake-engine: {err}");
            ExitCode::from(3)
        }
    }
}

/// Serves requests from `input` until `shutdown`, the end of input, or a scripted exit.
///
/// Requests are appended to `log` when given; without a log, scripted steps are counted from
/// the start of this run.
pub fn run(
    script: &Script,
    log: Option<&Path>,
    input: &mut impl Read,
    output: &mut impl Write,
) -> Result<Exit, FrameError> {
    let limits = FrameLimits::for_max_audio_s(script.hello.max_audio_s);
    let send = |output: &mut dyn Write, msg: FromEngine| write_frame(output, &encode(&msg), &[]);
    send(
        output,
        FromEngine::Hello(Hello {
            protocol: script.hello.protocol,
            engine: "fake".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            capabilities: Capabilities { bias: script.hello.bias, max_audio_s: script.hello.max_audio_s },
        }),
    )?;

    let mut loaded: Option<Loaded> = None;
    let mut transcribes_this_run = 0;
    loop {
        let Some(frame) = read_frame(input, &limits)? else {
            return Ok(Exit::EndOfInput);
        };
        let samples = frame.payload.len() / 4;
        let transcribe_index = match log {
            Some(log) => log_request(log, &frame.header, samples),
            None => transcribes_this_run,
        };
        let request = match decode::<ToEngine>(&frame.header) {
            Ok(request) => request,
            Err(err) => {
                let code = match err {
                    DecodeError::UnknownType(_) => EngineErrorCode::Unsupported,
                    DecodeError::Malformed(_) => EngineErrorCode::BadRequest,
                };
                send(output, error(request_id(&frame.header), code, err.to_string(), false))?;
                continue;
            }
        };
        match request {
            ToEngine::Shutdown => return Ok(Exit::Shutdown),
            ToEngine::Load(load) => match &script.load.outcome {
                LoadOutcome::Exit { code } => return Ok(Exit::Scripted(*code)),
                LoadOutcome::Hang => hang(),
                outcome => {
                    for progress in &script.load.progress {
                        send(output, FromEngine::Progress(*progress))?;
                    }
                    sleep_ms(script.load.delay_ms);
                    let reply = match outcome {
                        LoadOutcome::Loaded { revision } => {
                            let reply = loaded_reply(&load, revision.as_deref(), script.load.delay_ms);
                            loaded = Some(reply.clone());
                            FromEngine::Loaded(reply)
                        }
                        LoadOutcome::Error { code, message } => {
                            loaded = None;
                            error(None, *code, message.clone(), false)
                        }
                        LoadOutcome::Exit { .. } | LoadOutcome::Hang => unreachable!(),
                    };
                    send(output, reply)?;
                }
            },
            ToEngine::Transcribe(request) => {
                transcribes_this_run += 1;
                let Some(model) = &loaded else {
                    let message = "transcribe before a successful load".to_owned();
                    send(output, error(Some(request.id), EngineErrorCode::NotLoaded, message, false))?;
                    continue;
                };
                let Some(audio) = payload_to_samples(&frame.payload) else {
                    let message = "payload length is not a multiple of 4".to_owned();
                    send(output, error(Some(request.id), EngineErrorCode::BadRequest, message, false))?;
                    continue;
                };
                let step = script.transcribe.get(transcribe_index).cloned().unwrap_or_default();
                let reply = match step {
                    TranscribeStep::Result { text, delay_ms, no_speech_prob, speech_ms } => {
                        sleep_ms(delay_ms);
                        let result = TranscribeResultScript { text, delay_ms, no_speech_prob, speech_ms };
                        FromEngine::Result(result.reply(model, &request, audio.len()))
                    }
                    TranscribeStep::Error { code, message, retryable, delay_ms } => {
                        sleep_ms(delay_ms);
                        error(Some(request.id), code, message, retryable)
                    }
                    TranscribeStep::Exit { code, delay_ms } => {
                        sleep_ms(delay_ms);
                        return Ok(Exit::Scripted(code));
                    }
                    TranscribeStep::Hang => hang(),
                };
                send(output, reply)?;
            }
        }
    }
}

/// Appends a request to the log and returns how many `transcribe` requests came before it.
fn log_request(log: &Path, header: &[u8], samples: usize) -> usize {
    let header =
        serde_json::from_slice(header).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(header).into()));
    let mut line = serde_json::to_vec(&LoggedRequest { header, samples }).expect("log entries serialise");
    line.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut file| file.write_all(&line))
        .unwrap_or_else(|err| panic!("fake-engine: cannot write {}: {err}", log.display()));
    let entries =
        fs::read_to_string(log).unwrap_or_else(|err| panic!("fake-engine: cannot read {}: {err}", log.display()));
    entries
        .lines()
        .filter_map(|line| serde_json::from_str::<LoggedRequest>(line).ok())
        .filter(|entry| entry.header.get("type").and_then(Value::as_str) == Some("transcribe"))
        .count()
        .saturating_sub(1)
}

fn request_id(header: &[u8]) -> Option<u64> {
    serde_json::from_slice::<Value>(header).ok()?.get("id")?.as_u64()
}

fn error(id: Option<u64>, code: EngineErrorCode, message: String, retryable: bool) -> FromEngine {
    FromEngine::Error(EngineError { id, code, message, retryable })
}

fn loaded_reply(load: &Load, scripted_revision: Option<&str>, delay_ms: u64) -> Loaded {
    let revision = scripted_revision.or(load.revision.as_deref()).unwrap_or("fake-revision");
    Loaded { model: load.model.clone(), revision: revision.into(), load_ms: delay_ms.try_into().unwrap_or(u32::MAX) }
}

struct TranscribeResultScript {
    text: String,
    delay_ms: u64,
    no_speech_prob: Option<f32>,
    speech_ms: Option<u32>,
}

impl TranscribeResultScript {
    fn reply(self, model: &Loaded, request: &Transcribe, samples: usize) -> TranscribeResult {
        let audio_ms = u32::try_from(samples / 16).unwrap_or(u32::MAX);
        let with_words = request.options.get("word_timestamps").and_then(Value::as_bool) == Some(true);
        let path = request.options.get("path").and_then(Value::as_str).unwrap_or("batched");
        let segments = if self.text.trim().is_empty() {
            Vec::new()
        } else {
            vec![segment(&self.text, audio_ms, self.no_speech_prob.unwrap_or(0.01), with_words)]
        };
        let speech_ms = self.speech_ms.unwrap_or(if segments.is_empty() { 0 } else { audio_ms });
        let prompt = prompt(&request.bias);
        TranscribeResult {
            id: request.id,
            timings: EngineTimings {
                decode_ms: self.delay_ms.try_into().unwrap_or(u32::MAX),
                prompt_tokens: prompt.split_whitespace().count() as u32,
                bias_terms_used: request.bias.terms.len() as u32,
                prompt_truncated: false,
            },
            trace: EngineTrace {
                engine: "fake".into(),
                model: model.model.clone(),
                revision: model.revision.clone(),
                path: path.into(),
                options: request.options.clone(),
                prompt,
                result: EngineResult { language: request.language.clone(), speech_ms: Some(speech_ms), segments },
            },
        }
    }
}

fn segment(text: &str, audio_ms: u32, no_speech_prob: f32, with_words: bool) -> Segment {
    let words: Vec<&str> = text.split_whitespace().collect();
    let step = audio_ms / words.len().max(1) as u32;
    let words = with_words.then(|| {
        (0..words.len())
            .map(|i| Word {
                text: format!(" {}", words[i]),
                start_ms: step * i as u32,
                end_ms: step * (i as u32 + 1),
                prob: 0.9,
            })
            .collect()
    });
    Segment {
        text: format!(" {}", text.trim()),
        start_ms: 0,
        end_ms: audio_ms,
        no_speech_prob: Some(no_speech_prob),
        words,
    }
}

/// The faster-whisper engine's prompt rule, without its token budget.
fn prompt(bias: &Bias) -> String {
    let terms = (!bias.terms.is_empty()).then(|| format!("{}.", bias.terms.join(", ")));
    [(!bias.style.is_empty()).then_some(bias.style.clone()), terms].into_iter().flatten().collect::<Vec<_>>().join(" ")
}

fn sleep_ms(ms: u64) {
    if ms > 0 {
        thread::sleep(Duration::from_millis(ms));
    }
}

fn hang() -> ! {
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}
