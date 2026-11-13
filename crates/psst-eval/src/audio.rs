use std::{
    env,
    fs::{self, File, TryLockError},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, bail, ensure};
use psst_client::{
    AudioCodec, AudioFrame, Client, Endpoint, Session, SessionEvent, StartOpts,
    api::{ApiClient, Method},
    upload_file,
};
use psst_proto::{
    ErrorCode, FRAME_MS, FRAME_SAMPLES, SAMPLE_RATE, ServerTimings,
    api::{ModelState, ModelStatus, routes},
    ws::{Codec, ErrorMsg, EvalOpts, FinalMsg, Mode},
};
use psst_server::{ServeConfig, serve_in_process};
use uuid::Uuid;

use crate::{cases::EvalSet, replay::replay, stale::stale_traces};

/// A first model download takes a while.
const MODEL_WAIT: Duration = Duration::from_secs(30 * 60);
const ATTEMPTS: usize = 3;
const LIVE_CLIPS: usize = 2;
const LATENCY_ROWS_S: [u32; 3] = [15, 30, 90];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyRow {
    pub over_s: u32,
    pub up_to_s: Option<u32>,
    pub dictations: usize,
    pub stop_to_final_ms: Spread,
    pub engine_ms: Spread,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spread {
    pub p50: u32,
    pub p95: u32,
    pub max: u32,
}

impl Spread {
    fn of(mut values: Vec<u32>) -> Spread {
        values.sort_unstable();
        let rank = |p: usize| values[(values.len() * p).div_ceil(100).max(1) - 1];
        Spread { p50: rank(50), p95: rank(95), max: values[values.len() - 1] }
    }
}

/// Leaves out dictations that waited for the model: the latency targets assume a loaded engine.
pub fn latency_rows(timings: &[ServerTimings]) -> Vec<LatencyRow> {
    let mut bounds: Vec<(u32, Option<u32>)> = Vec::new();
    let mut over = 0;
    for up_to in LATENCY_ROWS_S {
        bounds.push((over, Some(up_to)));
        over = up_to;
    }
    bounds.push((over, None));
    bounds
        .into_iter()
        .filter_map(|(over_s, up_to_s)| {
            let in_range = |t: &&ServerTimings| {
                t.audio_ms > over_s * 1000 && up_to_s.is_none_or(|up_to| t.audio_ms <= up_to * 1000)
            };
            let rows: Vec<&ServerTimings> = timings.iter().filter(|t| t.load_wait_ms == 0).filter(in_range).collect();
            (!rows.is_empty()).then(|| LatencyRow {
                over_s,
                up_to_s,
                dictations: rows.len(),
                stop_to_final_ms: Spread::of(rows.iter().map(|t| t.stop_to_final_ms).collect()),
                engine_ms: Spread::of(rows.iter().map(|t| t.engine_ms).collect()),
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct AudioOptions {
    pub evals: PathBuf,
    pub cache_dir: PathBuf,
    /// Off only for tests with the fake engine.
    pub require_gpu_session: bool,
    /// The committed traces are PCM, so they measure the model rather than the codec.
    pub codec: AudioCodec,
}

/// Must match where `scripts/gpu-session.sh` keeps its lock.
pub fn psst_cache_dir() -> PathBuf {
    if let Some(dir) = env::var_os("PSST_CACHE_DIR") {
        return dir.into();
    }
    let cache = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".cache"));
    cache.join("psst")
}

pub fn default_model_cache() -> PathBuf {
    psst_cache_dir().join("hf")
}

/// `scripts/gpu-session.sh` exports `PSST_GPU_SESSION` and holds the lock while its command runs.
pub fn check_gpu_session() -> anyhow::Result<()> {
    let lock = psst_cache_dir().join("gpu.lock");
    let held = File::open(&lock).is_ok_and(|file| matches!(file.try_lock(), Err(TryLockError::WouldBlock)));
    ensure!(
        env::var_os("PSST_GPU_SESSION").is_some() && held,
        "psst-eval audio uses the GPU, so it runs only inside scripts/gpu-session.sh (which holds {}):\n  \
         scripts/gpu-session.sh nix develop .#gpu --command cargo run --release -p psst-eval -- audio",
        lock.display()
    );
    Ok(())
}

pub async fn run(opts: &AudioOptions) -> anyhow::Result<Vec<LatencyRow>> {
    if opts.require_gpu_session {
        check_gpu_session()?;
    }
    let mut set = EvalSet::load(&opts.evals)?;
    ensure!(set.files.server.eval_mode, "{}/server.toml must set eval_mode = true", set.dir.display());
    ensure!(!set.cases.is_empty(), "{} has no cases", set.cases_path().display());

    let work = tempfile::Builder::new().prefix("psst-eval-audio-").tempdir().context("cannot create a temp dir")?;
    let mut clips = Vec::new();
    for case in &set.cases {
        let clip = set.clip_path(&case.id);
        let samples = read_flac(&clip).with_context(|| format!("case {}", case.id))?;
        let wav = work.path().join(format!("{}.wav", case.id));
        write_wav(&wav, &samples)?;
        clips.push((wav, samples));
    }

    fs::create_dir_all(&opts.cache_dir).with_context(|| format!("cannot create {}", opts.cache_dir.display()))?;
    let state_dir = work.path().join("state");
    fs::create_dir(&state_dir)?;
    let server = serve_in_process(ServeConfig {
        config_dir: set.dir.clone(),
        state_dir,
        cache_dir: opts.cache_dir.clone(),
        socket: Some(work.path().join("client.sock")),
    })
    .await
    .context("cannot start the eval server")?;
    let endpoint = Endpoint::Unix(server.socket().to_owned());
    // Shut the server down whatever happened, then report the first error.
    let recorded = match wait_for_model(&endpoint).await {
        Ok(()) => record(&mut set, &clips, &endpoint, opts.codec).await,
        Err(err) => Err(err),
    };
    let shutdown = server.shutdown().await.context("cannot shut the eval server down");
    let timings = recorded?;
    shutdown?;
    crate::cases::write_cases(&set.cases_path(), &set.cases)?;
    eprintln!("wrote {}", set.cases_path().display());

    // Freshly recorded, so a stale trace means the server and replay build requests differently.
    let stale = stale_traces(&set)?;
    if let Some(first) = stale.first() {
        bail!(
            "{} freshly recorded traces don't match the requests replay recomputes (first: case {} {}: {})",
            stale.len(),
            first.case,
            first.trace,
            first.reason
        );
    }
    let replay = replay(&set)?;
    replay.baseline.write(&set.baseline_path())?;
    eprintln!("wrote {}", set.baseline_path().display());
    crate::report::print_summary(&replay.baseline);
    let latency = latency_rows(&timings);
    crate::report::print_latency(&latency);
    Ok(latency)
}

async fn record(
    set: &mut EvalSet,
    clips: &[(PathBuf, Vec<i16>)],
    endpoint: &Endpoint,
    codec: AudioCodec,
) -> anyhow::Result<Vec<ServerTimings>> {
    let mut client = Client::connect(endpoint, None).await.context("cannot connect to the eval server")?;
    if codec != AudioCodec::Pcm {
        eprintln!("sending the audio as {codec:?}");
    }

    let profiles = set.cases.iter().map(|case| set.profile_for(&case.lang)).collect::<anyhow::Result<Vec<_>>>()?;
    let total = set.cases.len();
    let mut finals = Vec::with_capacity(total);
    for (i, (case, (wav, _))) in set.cases.iter_mut().zip(clips).enumerate() {
        let mut traces = Vec::new();
        for bias in [true, false] {
            let opts = start_opts(&profiles[i], Mode::Upload, bias, codec);
            let fin = transcribe(&mut client, endpoint, opts, |client, opts| upload_file(client, opts, wav))
                .await
                .with_context(|| format!("case {} ({})", case.id, if bias { "biased" } else { "raw" }))?;
            let trace = fin.trace.clone().context("the server sent no trace; is eval_mode on?")?;
            traces.push(trace);
            if bias {
                finals.push(fin);
            }
        }
        let raw = traces.pop();
        case.engine_output = traces.pop();
        case.engine_output_raw = raw;
        eprintln!("[{}/{total}] {}: {}", i + 1, case.id, finals.last().map_or("", |f| f.r#final.text.as_str()));
    }

    for index in live_cases(set, clips) {
        let case = &set.cases[index];
        let live = live_session(&mut client, start_opts(&profiles[index], Mode::Live, true, codec), &clips[index].1)
            .await
            .with_context(|| format!("live smoke test, case {}", case.id))?;
        let upload = &finals[index];
        ensure!(
            live.timings.audio_ms == upload.timings.audio_ms,
            "live smoke test, case {}: the server got {} ms of audio live and {} ms uploaded",
            case.id,
            live.timings.audio_ms,
            upload.timings.audio_ms
        );
        if live.r#final.text != upload.r#final.text {
            eprintln!(
                "warning: live smoke test, case {}: live gave {:?}, upload gave {:?}",
                case.id, live.r#final.text, upload.r#final.text
            );
        } else {
            eprintln!("live smoke test, case {}: same text as the upload", case.id);
        }
    }

    Ok(finals.iter().map(|fin| fin.timings).collect())
}

fn start_opts(profile: &psst_config::Profile, mode: Mode, bias: bool, codec: AudioCodec) -> StartOpts {
    StartOpts {
        session_id: Uuid::new_v4(),
        profile: Some(profile.name.clone()),
        post_processing: None,
        target: None,
        mode,
        resume: false,
        eval: (!bias).then_some(EvalOpts { bias: false }),
        codec,
    }
}

/// A server without Opus gets PCM instead, and the run would measure the wrong thing.
fn check_codec(session: &Session, asked: AudioCodec) -> anyhow::Result<()> {
    let wanted = match asked {
        AudioCodec::Pcm => Codec::PcmS16le,
        AudioCodec::Opus(_) => Codec::Opus,
    };
    ensure!(session.codec() == wanted, "the session sends {:?} instead of {wanted:?}", session.codec());
    Ok(())
}

fn live_cases(set: &EvalSet, clips: &[(PathBuf, Vec<i16>)]) -> Vec<usize> {
    let mut candidates: Vec<usize> = (0..set.cases.len()).filter(|&i| !set.cases[i].reference.is_empty()).collect();
    candidates.sort_by_key(|&i| (clips[i].1.len(), set.cases[i].id.clone()));
    let mut chosen: Vec<usize> = Vec::new();
    for &i in &candidates {
        if chosen.len() < LIVE_CLIPS && !chosen.iter().any(|&c| set.cases[c].lang == set.cases[i].lang) {
            chosen.push(i);
        }
    }
    for &i in &candidates {
        if chosen.len() < LIVE_CLIPS && !chosen.contains(&i) {
            chosen.push(i);
        }
    }
    chosen
}

enum Outcome {
    Final(Box<FinalMsg>),
    Refused(ErrorMsg),
}

/// Runs before any upload, so every biased upload counts in the latency.
async fn wait_for_model(endpoint: &Endpoint) -> anyhow::Result<()> {
    let api = ApiClient::new(endpoint.clone(), None);
    let deadline = Instant::now() + MODEL_WAIT;
    let mut shown = None;
    loop {
        let status: ModelStatus = api
            .call(Method::Get, routes::MODEL, None::<&()>)
            .await
            .context("cannot ask the eval server about its model")?;
        match status.state {
            ModelState::Loaded => return Ok(()),
            ModelState::Failed => bail!("the model failed: {}", status.error.unwrap_or_default()),
            // A preloading server starts in `loading`, so this is `preload = false`.
            ModelState::Unloaded => {
                bail!("the eval server's model is unloaded, and nothing here loads it: set `model.preload = true`")
            }
            state => {
                ensure!(Instant::now() < deadline, "the model is still {state:?} after {MODEL_WAIT:?}");
                if shown != Some(state) {
                    eprintln!("waiting for the model: {state:?}");
                    shown = Some(state);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn transcribe(
    client: &mut Client,
    endpoint: &Endpoint,
    opts: StartOpts,
    start: impl Fn(&mut Client, StartOpts) -> Session,
) -> anyhow::Result<Box<FinalMsg>> {
    let deadline = Instant::now() + MODEL_WAIT;
    let mut failures = 0;
    loop {
        // Every attempt is a new session: a refused or failed one is gone from the registry.
        let mut session = start(client, StartOpts { session_id: Uuid::new_v4(), ..opts.clone() });
        check_codec(&session, opts.codec)?;
        let err = match outcome(&mut session).await {
            Ok(Outcome::Final(fin)) => return Ok(fin),
            Ok(Outcome::Refused(err)) => err,
            Err(err) => {
                failures += 1;
                if failures >= ATTEMPTS {
                    return Err(err);
                }
                eprintln!("warning: {err:#}; reconnecting");
                *client = Client::connect(endpoint, None).await.context("cannot reconnect to the eval server")?;
                continue;
            }
        };
        if err.code == ErrorCode::ModelUnavailable {
            // A failed model stays failed until `psst-server model load`, which nothing here sends.
            ensure!(!model_failed(&err), "{}", err.message);
            if Instant::now() < deadline {
                let detail = err.detail.as_ref().map(|d| format!(" {d}")).unwrap_or_default();
                eprintln!("waiting for the model: {}{detail}", err.message);
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        }
        failures += 1;
        if !err.retryable || failures >= ATTEMPTS {
            bail!("the server answered `{:?}`: {}", err.code, err.message);
        }
        eprintln!("warning: `{:?}`: {}; trying again", err.code, err.message);
    }
}

fn model_failed(err: &ErrorMsg) -> bool {
    err.detail.as_ref().and_then(|detail| detail.get("reason")).and_then(|reason| reason.as_str()) == Some("failed")
}

async fn outcome(session: &mut Session) -> anyhow::Result<Outcome> {
    loop {
        match session.next_event().await {
            SessionEvent::Ready { .. } | SessionEvent::Progress { .. } => {}
            SessionEvent::Final(fin) => return Ok(Outcome::Final(fin)),
            SessionEvent::Rejected(err) | SessionEvent::Error(err) => return Ok(Outcome::Refused(err)),
            SessionEvent::Cancelled => bail!("the server cancelled the session"),
            SessionEvent::Lost(reason) => bail!("lost the session: {reason:?}"),
        }
    }
}

async fn live_session(client: &mut Client, opts: StartOpts, samples: &[i16]) -> anyhow::Result<Box<FinalMsg>> {
    let codec = opts.codec;
    let mut session = client.start(opts);
    check_codec(&session, codec)?;
    let frames = session.frames();
    let audio: Vec<AudioFrame> = samples
        .chunks(FRAME_SAMPLES)
        .map(|chunk| {
            let mut frame = [0; FRAME_SAMPLES];
            frame[..chunk.len()].copy_from_slice(chunk);
            frame
        })
        .collect();
    let mut feeder = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(FRAME_MS.into()));
        for frame in audio {
            tick.tick().await;
            frames.try_send(frame).context("cannot queue a live frame")?;
        }
        anyhow::Ok(())
    });
    loop {
        tokio::select! {
            fed = &mut feeder => {
                fed.context("the live feeder panicked")??;
                break;
            }
            event = session.next_event() => match event {
                SessionEvent::Ready { .. } | SessionEvent::Progress { .. } => {}
                other => bail!("the live session ended while recording: {other:?}"),
            },
        }
    }
    session.stop(samples.len() as u64).await?;
    match outcome(&mut session).await? {
        Outcome::Final(fin) => Ok(fin),
        Outcome::Refused(err) => bail!("the server answered `{:?}`: {}", err.code, err.message),
    }
}

pub fn read_flac(path: &Path) -> anyhow::Result<Vec<i16>> {
    let mut reader = claxon::FlacReader::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let info = reader.streaminfo();
    ensure!(
        info.sample_rate == SAMPLE_RATE && info.channels == 1,
        "{} is {} Hz with {} channels; clips are {SAMPLE_RATE} Hz mono",
        path.display(),
        info.sample_rate,
        info.channels
    );
    let bits = info.bits_per_sample;
    ensure!((8..=32).contains(&bits), "{}: unsupported {bits}-bit samples", path.display());
    reader
        .samples()
        .map(|sample| {
            let sample = sample.with_context(|| format!("cannot decode {}", path.display()))?;
            Ok(if bits >= 16 { (sample >> (bits - 16)) as i16 } else { (sample << (16 - bits)) as i16 })
        })
        .collect()
}

pub fn write_wav(path: &Path, samples: &[i16]) -> anyhow::Result<()> {
    let data_len = u32::try_from(samples.len() * 2).context("clip too long")?;
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))
}
