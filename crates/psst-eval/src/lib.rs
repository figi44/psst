//! `psst-eval`: the replay, stress and audio runners of plan 7.3.
//!
//! - [`metrics`]: term recall, WER and false corrections, counted per output.
//! - [`text`]: normalisation and exact term occurrences.
//! - [`align`]: word alignment.

pub mod align;
pub mod metrics;
pub mod text;
