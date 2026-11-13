//! `io.github.figi44.psst.Native`: one core per process. Every function catches panics (`with_env`), and
//! only `nextEvent` turns a failure into a Java exception; the rest log it and return.

use std::sync::OnceLock;

use jni::{
    EnvUnowned, Outcome,
    errors::ThrowRuntimeExAndDefault,
    objects::{JClass, JShortArray, JString},
    sys::jint,
};
use tracing::{error, warn};

use crate::{Command, Core, logcat};

static CORE: OnceLock<Core> = OnceLock::new();

/// The panic hook has logged a panic already.
fn logged<T: Default>(function: &str, outcome: Outcome<T, jni::errors::Error>) -> T {
    match outcome {
        Outcome::Ok(value) => value,
        Outcome::Err(err) => {
            error!("Native.{function}: {err}");
            T::default()
        }
        Outcome::Panic(payload) => {
            // Dropping a payload can panic in turn.
            std::mem::forget(payload);
            T::default()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_figi44_psst_Native_start<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    data_dir: JString<'caller>,
    device_name: JString<'caller>,
) {
    let outcome = env.with_env(|env| -> jni::errors::Result<()> {
        logcat::init();
        let data_dir = data_dir.try_to_string(env)?;
        let device_name = device_name.try_to_string(env)?;
        CORE.get_or_init(|| Core::start(data_dir, device_name));
        Ok(())
    });
    logged("start", outcome.into_outcome())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_figi44_psst_Native_send<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    command: JString<'caller>,
) {
    let outcome = env.with_env(|env| -> jni::errors::Result<()> {
        let json = command.try_to_string(env)?;
        // Not the JSON itself: it may hold a pairing code.
        match (serde_json::from_str::<Command>(&json), CORE.get()) {
            (Ok(command), Some(core)) => core.send(command),
            (Ok(_), None) => warn!("ignoring a command sent before start"),
            (Err(err), _) => warn!("ignoring a malformed command: {err}"),
        }
        Ok(())
    });
    logged("send", outcome.into_outcome())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_figi44_psst_Native_audio<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    samples: JShortArray<'caller>,
    count: jint,
) {
    let outcome = env.with_env(|env| -> jni::errors::Result<()> {
        let Some(core) = CORE.get() else { return Ok(()) };
        let count = usize::try_from(count).unwrap_or(0).min(samples.len(env)?);
        let mut buf = vec![0; count];
        samples.get_region(env, 0, &mut buf)?;
        core.audio(buf);
        Ok(())
    });
    logged("audio", outcome.into_outcome())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_figi44_psst_Native_nextEvent<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| {
        let Some(event) = CORE.wait().next_event() else {
            error!("the core has stopped: no more events");
            loop {
                std::thread::park();
            }
        };
        JString::from_str(env, serde_json::to_string(&event).unwrap_or_default())
    })
    .resolve::<ThrowRuntimeExAndDefault>()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_figi44_psst_Native_version<'caller>(
    mut env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    env.with_env(|env| JString::from_str(env, env!("CARGO_PKG_VERSION"))).resolve::<ThrowRuntimeExAndDefault>()
}
