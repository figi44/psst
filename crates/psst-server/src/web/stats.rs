use axum::{extract::State, http::StatusCode, response::Response};
use minijinja::{Value, context, value::merge_maps};
use psst_proto::api::{Percentiles, Stats};

use super::{AppRef, Viewer, chrome, message, render};
use crate::api::compute_stats;

/// What the user waits for, as [`Stats`] keys them.
const TILES: &[(&str, &str, &str)] = &[
    ("client.stop_to_dispatch_ms", "Release to text", "until it's in the app"),
    ("server.stop_to_final_ms", "Server", "stop to final text"),
    ("server.tidy_ms", "Post-processing", "part of the server's"),
    ("network_estimate_ms", "Network", "the client's wait less the server's"),
];

/// A dictation's steps in order.
const STEPS: &[(&str, &str)] = &[
    ("client.toggle_to_overlay_ms", "Hotkey to overlay"),
    ("client.toggle_to_first_frame_ms", "Hotkey to first audio"),
    ("server.queue_ms", "Waiting for the engine and the model"),
    ("server.engine_ms", "Speech model"),
    ("server.pipeline_ms", "Dictionary and rules"),
    ("server.tidy_first_token_ms", "Post-processing, first token"),
    ("server.tidy_ms", "Post-processing"),
    ("network_estimate_ms", "Network"),
    ("client.final_to_dispatch_ms", "Inserting the text"),
];

pub async fn show(viewer: Viewer, State(state): State<AppRef>) -> Response {
    let rows = match state.db.timings() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::error!("cannot read the history's timings: {err:#}");
            return message(StatusCode::INTERNAL_SERVER_ERROR, "Stats are unavailable", "See the server's log.", None);
        }
    };
    let days = state.live.server().history_days;
    let ctx = merge_maps([view(&compute_stats(&rows), days), chrome(&state, &viewer, "stats")]);
    render(StatusCode::OK, "stats.html", ctx)
}

fn view(stats: &Stats, days: u32) -> Value {
    let present = |table: &'static [(&'static str, &'static str)]| {
        table.iter().filter_map(|(key, label)| Some((*label, stats.timings.get(*key)?)))
    };
    let tiles: Vec<Value> = TILES
        .iter()
        .filter_map(|&(key, label, hint)| {
            let p = stats.timings.get(key)?;
            Some(context! { label, hint, p50 => duration(p.p50), p95 => duration(p.p95), count => p.count })
        })
        .collect();
    // One scale for every step, so the bars compare.
    let scale = present(STEPS).map(|(_, p)| p.p95).max().unwrap_or(0).max(1);
    let width = |ms: u32| format!("{:.1}%", f64::from(ms) * 100.0 / f64::from(scale));
    let steps: Vec<Value> = present(STEPS)
        .map(|(label, p)| {
            context! {
                label,
                p50 => duration(p.p50),
                p95 => duration(p.p95),
                w50 => width(p.p50),
                w95 => width(p.p95),
                tip => tip(p),
            }
        })
        .collect();
    let audio = stats.timings.get("server.audio_ms").map(|p| duration(p.p50));
    context! { days, sessions => stats.sessions, audio, tiles, steps }
}

fn tip(p: &Percentiles) -> String {
    let dictations = if p.count == 1 { "dictation" } else { "dictations" };
    format!("Median {}, 95th percentile {}, over {} {dictations}", duration(p.p50), duration(p.p95), p.count)
}

fn duration(ms: u32) -> String {
    if ms < 1000 { format!("{ms} ms") } else { format!("{:.1} s", f64::from(ms) / 1000.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(rows: &[(&str, u32, u32)]) -> Stats {
        let timings = rows
            .iter()
            .map(|(key, p50, p95)| ((*key).to_owned(), Percentiles { count: 3, p50: *p50, p95: *p95 }))
            .collect();
        Stats { sessions: 3, timings }
    }

    #[test]
    fn every_step_shares_the_slowest_steps_scale() {
        let view = view(&stats(&[("server.engine_ms", 200, 250), ("server.tidy_ms", 900, 1_000)]), 30);
        let steps: Vec<(String, String, String)> = view
            .get_attr("steps")
            .unwrap()
            .try_iter()
            .unwrap()
            .map(|s| {
                let attr = |name| s.get_attr(name).unwrap().to_string();
                (attr("label"), attr("w50"), attr("w95"))
            })
            .collect();
        assert_eq!(
            steps,
            [
                ("Speech model".to_owned(), "20.0%".to_owned(), "25.0%".to_owned()),
                ("Post-processing".to_owned(), "90.0%".to_owned(), "100.0%".to_owned()),
            ]
        );
    }
}
