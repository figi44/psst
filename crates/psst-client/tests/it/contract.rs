//! Compile-time guarantees the daemon relies on (plan 5.7).

use psst_client::{FRAME_BUFFER, FrameSender, Session};
use psst_proto::{FRAME_MS, FRAME_SAMPLES};

fn send<T: Send>() {}
fn clone_send<T: Clone + Send>() {}

#[test]
fn sessions_move_between_tasks_and_frame_senders_are_cloneable() {
    send::<Session>();
    clone_send::<FrameSender>();
}

#[test]
fn the_frame_buffer_holds_thirty_seconds() {
    assert_eq!(FRAME_SAMPLES, 320);
    assert!(FRAME_BUFFER as u32 * FRAME_MS >= 30_000);
}
