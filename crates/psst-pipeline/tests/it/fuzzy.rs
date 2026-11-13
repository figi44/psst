//! Fuzzy matching and never rules, through `run`.
//!
//! `said` has no word probabilities, which is the medium band; `said_p` and `said_words` give the
//! engine's probabilities.

use crate::support;

use psst_proto::Stage;
use support::*;

const HYPRLAND: &str = "[[terms]]\ntext = \"Hyprland\"";
const KUBERNETES: &str = "[[terms]]\ntext = \"Kubernetes\"";

/// A dictionary of terms with only a `text`.
fn terms(texts: &[&str]) -> String {
    texts.iter().map(|t| format!("[[terms]]\ntext = {t:?}\n")).collect()
}

#[test]
fn a_term_heard_as_several_words_is_joined_back() {
    let out = run_with(&said("I switched to hyper land last week."), HYPRLAND);
    assert_eq!(out.text, "I switched to Hyprland last week. ");
    assert_eq!(out.edits[0], fuzzy(14..24, "hyprland", "hyper land", "Hyprland", (1.0 / 9.0 * 0.3) as f32));

    // n-grams of up to four words.
    assert_eq!(text(&said("using Power Shell Core"), &terms(&["PowerShell Core"])), "Using PowerShell Core ");
    assert_eq!(text(&said("use Web G P U for this"), &terms(&["WebGPU"])), "Use WebGPU for this ");
    assert_eq!(text(&said("Git Lab CI runner"), &terms(&["GitLab", "CI"])), "GitLab CI runner ");
    assert_eq!(
        text(&said("we send it with Mail Chim, which lets you email"), &terms(&["Mailchimp"])),
        "We send it with Mailchimp, which lets you email "
    );
    // The term's text is written whatever the spoken case.
    assert_eq!(text(&said("MAIL CHIM is great"), &terms(&["Mailchimp"])), "Mailchimp is great ");
    // A trailing digit is part of the key, so it's never written twice ("Llama-33").
    assert_eq!(text(&said("use Llama3 for this"), &terms(&["Llama-3"])), "Use Llama-3 for this ");
}

#[test]
fn a_span_has_at_most_four_tokens() {
    let dict = terms(&["Zarbleflux"]);
    assert_eq!(text(&said("the zar ble fl ux module"), &dict), "The Zarbleflux module ");
    assert_eq!(text(&said("the zar b le fl ux module"), &dict), "The zar b le fl ux module ");
}

#[test]
fn diacritics_fold_away_in_the_key() {
    let dict = terms(&["Dvořák"]);
    assert_eq!(text(&said("play dvorak tonight"), &dict), "Play Dvořák tonight ");
    assert_eq!(text(&said("play Dvořak tonight"), &dict), "Play Dvořák tonight ");
}

#[test]
fn leading_and_trailing_punctuation_stays() {
    assert_eq!(text(&said("(hyper-land),"), HYPRLAND), "(Hyprland), ");
    assert_eq!(text(&said("...hyper land..."), HYPRLAND), "...Hyprland... ");
    // Punctuation outside ASCII.
    assert_eq!(text(&said("«hyperland»"), HYPRLAND), "«Hyprland» ");
}

#[test]
fn only_letters_digits_hyphens_and_apostrophes_may_sit_inside_a_span() {
    assert_eq!(text(&said("the cuber netes team"), KUBERNETES), "The Kubernetes team ");
    assert_eq!(text(&said("the Cuber-Netes team"), KUBERNETES), "The Kubernetes team ");
    assert_eq!(text(&said("the cuber. Netes team"), KUBERNETES), "The cuber. Netes team ");
    assert_eq!(text(&said_p("not hyper. Land", 0.3), HYPRLAND), "Not hyper. Land ");
    // Punctuation between the words keeps them apart.
    assert_eq!(text(&said("(pipe wire), pipe (wire)."), &terms(&["PipeWire"])), "(PipeWire), pipe (wire). ");
}

#[test]
fn a_suffix_stays_on_when_the_bare_word_matches_better() {
    let out = run_with(&said("cubernetes's pods"), KUBERNETES);
    assert_eq!(out.text, "Kubernetes's pods ");
    assert_eq!(out.edits[0], fuzzy(0..10, "kubernetes", "cubernetes", "Kubernetes", (1.0 / 10.0 * 0.3) as f32));
    assert_eq!(text(&said("cubernetes’s pods"), KUBERNETES), "Kubernetes’s pods ");
    assert_eq!(text(&said("kubernetes's pods"), KUBERNETES), "Kubernetes's pods ");
    assert_eq!(text(&said("two hyprlands"), HYPRLAND), "Two Hyprlands ");
    assert_eq!(text(&said("both hyper lands' configs"), HYPRLAND), "Both Hyprlands' configs ");
    assert_eq!(text(&said("two cachixes"), &terms(&["Cachix"])), "Two Cachixes ");
    // A term that ends in "s" doesn't get a second one.
    assert_eq!(text(&said("on kubernetis"), KUBERNETES), "On Kubernetes ");

    // Possessives.
    let dict = terms(&["PipeWire"]);
    assert_eq!(text(&said("pipe wire's graph, pipe wire’s graph."), &dict), "PipeWire's graph, PipeWire’s graph. ");
    let dict = terms(&["Domino’s", "Earl’s Court"]);
    assert_eq!(text(&said("domino's menu near earl's court"), &dict), "Domino’s menu near Earl’s Court ");
}

#[test]
fn a_possessive_inside_a_span_keeps_its_words_apart() {
    // "pipe’s wire" is not PipeWire.
    assert_eq!(text(&said("pipe’s wire"), &terms(&["PipeWire"])), "Pipe’s wire ");
    let dict = terms(&["Claude Code"]);
    assert_eq!(text(&said("check claude's code"), &dict), "Check claude's code ");
    assert_eq!(text(&said("check cloud code"), &dict), "Check Claude Code ");
}

#[test]
fn the_engines_confidence_sets_how_close_a_span_has_to_be() {
    let dict = terms(&["Docker", "Kafka", "Hyprland"]);
    // "docket" scores 1/6 against Docker: under 0.18, but not under 0.08.
    for (prob, expected) in [
        (Some(0.99), "Push the docket image "),
        (Some(0.85), "Push the docket image "),
        (Some(0.849), "Push the Docker image "),
        (None, "Push the Docker image "),
        (Some(0.3), "Push the Docker image "),
    ] {
        let trace = prob.map_or_else(|| said("push the docket image"), |p| said_p("push the docket image", p));
        assert_eq!(text(&trace, &dict), expected, "{prob:?}");
    }
    // "kafla" scores 1/5 against Kafka: only under the low band's 0.25.
    for (prob, expected) in
        [(Some(0.5), "Start kafla now "), (None, "Start kafla now "), (Some(0.499), "Start Kafka now ")]
    {
        let trace = prob.map_or_else(|| said("start kafla now"), |p| said_p("start kafla now", p));
        assert_eq!(text(&trace, &dict), expected, "{prob:?}");
    }
    assert_eq!(text(&said_p("on hyper land", 0.99), &dict), "On Hyprland ");
}

#[test]
fn a_span_is_as_sure_as_its_least_sure_word() {
    let dict = terms(&["Syncthing"]);
    let unsure = said_words(&[("start", 0.99), ("sync", 0.95), ("fing", 0.3), ("again", 0.99)]);
    assert_eq!(text(&unsure, &dict), "Start Syncthing again ");
    let sure = said_words(&[("start", 0.99), ("sync", 0.95), ("fing", 0.9), ("again", 0.99)]);
    assert_eq!(text(&sure, &dict), "Start sync fing again ");
}

#[test]
fn a_match_that_changes_nothing_is_protected_without_an_edit() {
    // "niris" is the term with its suffix kept, so the capitalisation rule leaves it alone.
    let out = run_with(&said("niris everywhere"), &terms(&["niri"]));
    assert_eq!(out.text, "niris everywhere ");
    assert!(out.edits.iter().all(|e| e.stage != Stage::Dictionary && e.rule != "capitalize"), "{:?}", out.edits);
}

#[test]
fn tokens_an_exact_spelling_accepted_take_no_part() {
    // "wayland" is the exact spelling of Wayland, so "x wayland" never becomes XWayland.
    assert_eq!(text(&said("run x wayland apps"), &terms(&["XWayland", "Wayland"])), "Run x Wayland apps ");
}

#[test]
fn a_term_that_is_a_common_word_never_recases_it_but_the_word_can_match_others() {
    // The exact step leaves "home" alone for the term Home, a common word; so does fuzzy matching,
    // even unsure.
    assert_eq!(text(&said_p("go home now", 0.3), &terms(&["Home"])), "Go home now ");
    // Nor in another form of the word.
    assert_eq!(text(&said_p("go home's way", 0.3), &terms(&["Home"])), "Go home's way ");
    assert_eq!(text(&said_p("play games now", 0.3), &terms(&["Games"])), "Play games now ");
    // "land" may still be part of a longer span, or become another term.
    let dict = terms(&["Hyprland", "Land"]);
    assert_eq!(text(&said("on hyper land"), &dict), "On Hyprland ");
    assert_eq!(text(&said_p("go home now", 0.3), &terms(&["Home", "Homer"])), "Go Homer now ");
}

#[test]
fn a_single_common_word_changes_only_when_the_engine_was_unsure() {
    let dict = terms(&["Ghostty", "Claude Code"]);
    // "ghost" scores 0.086 against Ghostty, under the medium band's 0.18, but it's a common word.
    assert_eq!(text(&said("open a ghost window"), &dict), "Open a ghost window ");
    assert_eq!(text(&said_p("open a ghost window", 0.9), &dict), "Open a ghost window ");
    assert_eq!(text(&said_p("open a ghost window", 0.3), &dict), "Open a Ghostty window ");
    assert_eq!(text(&said_p("open a ghosty window", 0.9), &dict), "Open a Ghostty window ");
    // Spans of several common words aren't held back.
    assert_eq!(text(&said_p("ask cloud code", 0.9), &dict), "Ask Claude Code ");
    // "system's" has the key "systems", which is a common word.
    let dict = terms(&["systemd"]);
    assert_eq!(text(&said("the system's clock"), &dict), "The system's clock ");
    assert_eq!(text(&said("restart system d"), &dict), "Restart systemd ");
}

#[test]
fn function_words_never_start_or_end_a_span() {
    // "is the" sounds like Istio, "to go" like Tokio and "well and" like Wayland, even unsure.
    let dict = terms(&["Istio", "Tokio", "Wayland"]);
    let spoken = "it is the best way to go, as well and good";
    assert_eq!(text(&said_p(spoken, 0.3), &dict), "It is the best way to go, as well and good ");
    assert_eq!(text(&said_p("the way land works", 0.9), &dict), "The Wayland works ");
}

#[test]
fn sounding_alike_only_helps_a_spelling_that_is_already_close() {
    // "Taleskill" sounds like Tailscale, but more than a third of its letters differ.
    let dict = terms(&["Tailscale"]);
    assert_eq!(text(&said_p("restart Taleskill", 0.3), &dict), "Restart Taleskill ");
    assert_eq!(text(&said_p("restart tail scale", 0.9), &dict), "Restart Tailscale ");
    // "rediz" is one letter off; a swap is two edits out of five letters.
    assert_eq!(text(&said("rediz ngnix"), &terms(&["Redis", "Nginx"])), "Redis ngnix ");
}

#[test]
fn keys_need_four_characters_and_a_similar_length() {
    assert_eq!(text(&said_p("run k 3 s here", 0.3), &terms(&["k3s"])), "Run k 3 s here ");
    // AT&T's key "att" is too short, so neither "ATT" nor "A T and T" matches it.
    let dict = terms(&["AT&T"]);
    assert_eq!(text(&said_p("call ATT about it", 0.3), &dict), "Call ATT about it ");
    assert_eq!(text(&said_p("call A T and T about it", 0.3), &dict), "Call A T and T about it ");
    // Keys in other scripts need four characters too: two Han characters are too short.
    assert_eq!(text(&said_p("早安。", 0.3), &terms(&["早案"])), "早安。 ");
    // "hyperolande" sounds like Hyprland with three letters more, more than 2 apart.
    assert_eq!(text(&said_p("on hyperolande", 0.3), HYPRLAND), "On hyperolande ");
    assert_eq!(text(&said_p("on hyper o land", 0.3), HYPRLAND), "On Hyprland ");
}

#[test]
fn two_terms_too_close_to_call_leave_the_span_alone() {
    // "flax" is as close to Flux as to Flox.
    assert_eq!(text(&said_p("the flax tool", 0.3), &terms(&["Flox"])), "The Flox tool ");
    assert_eq!(text(&said_p("the flax tool", 0.3), &terms(&["Flux", "Flox"])), "The flax tool ");
    // Two terms with one match key.
    assert_eq!(text(&said("the pipe wire daemon"), &terms(&["PipeWire"])), "The PipeWire daemon ");
    assert_eq!(text(&said("the pipe wire daemon"), &terms(&["PipeWire", "Pipe-Wire"])), "The pipe wire daemon ");
}

#[test]
fn overlapping_candidates_go_by_score_then_length_then_position() {
    let github = terms(&["GitHub", "GitHub Actions"]);
    // "git hub" is exactly GitHub; "git hub action" is a letter short of GitHub Actions.
    assert_eq!(text(&said("on git hub action"), &github), "On GitHub action ");
    assert_eq!(text(&said("on git hub actions"), &github), "On GitHub Actions ");
    assert_eq!(text(&said("pipe wire guard"), &terms(&["PipeWire", "WireGuard"])), "PipeWire guard ");
}

#[test]
fn a_fuzzy_match_is_protected_from_the_rules() {
    let out = run_with(&said("t3 code is open."), &terms(&["t3code"]));
    assert_eq!(out.text, "t3code is open. ");
    assert!(out.edits.iter().all(|e| e.rule != "capitalize"), "{:?}", out.edits);
}

#[test]
fn only_english_sessions_and_fuzzy_terms_take_part() {
    assert_eq!(text(&said_cs("mám hyper land"), HYPRLAND), "Mám hyper land ");
    let off = "[[terms]]\ntext = \"Hyprland\"\nfuzzy = false";
    assert_eq!(text(&said("on hyper land"), off), "On hyper land ");
    let czech = "[[terms]]\ntext = \"Hyprland\"\nlanguages = \"cs\"";
    assert_eq!(text(&said("on hyper land"), czech), "On hyper land ");
}

#[test]
fn a_never_rule_vetoes_a_term_for_a_span_exactly_and_fuzzily() {
    let dict = "[[terms]]\ntext = \"Hyprland\"\nsounds_like = [\"hyper land\"]";
    assert_eq!(run_with_never(&said("on hyper land"), dict, &[]).text, "On Hyprland ");
    assert_eq!(run_with_never(&said("on hyper land"), dict, &[("Hyprland", "hyper land")]).text, "On hyper land ");
    // Spans are compared by their match key.
    assert_eq!(run_with_never(&said("on hyper land"), dict, &[("hyprland", "Hyper-Land")]).text, "On hyper land ");

    // "Cubernetes" has the same match key as "cuber netes"; "kubernetis" doesn't.
    let spoken = "the cuber netes, cubernetes and kubernetis";
    let out = run_with_never(&said(spoken), KUBERNETES, &[("Kubernetes", "cuber netes")]);
    assert_eq!(out.text, "The cuber netes, cubernetes and Kubernetes ");
    // The span as written, suffix and all.
    let out = run_with_never(&said("cubernetes's pods"), KUBERNETES, &[("Kubernetes", "cubernetes's")]);
    assert_eq!(out.text, "Cubernetes's pods ");
}

#[test]
fn a_vetoed_match_makes_way_only_for_a_clear_next_best() {
    // Once Flax is vetoed, "flax" is as close to Flox as to Flux.
    let dict = terms(&["Flax", "Flox", "Flux"]);
    assert_eq!(run_with_never(&said("the flax tool"), &dict, &[]).text, "The Flax tool ");
    assert_eq!(run_with_never(&said("the flax tool"), &dict, &[("Flax", "flax")]).text, "The flax tool ");
}

#[test]
fn a_vetoed_match_makes_way_for_the_next_best() {
    let dict = terms(&["Claude Code", "Claude"]);
    assert_eq!(
        run_with_never(&said("ask claude code"), &dict, &[("Claude Code", "claude code")]).text,
        "Ask Claude code "
    );
    let dict = terms(&["GitHub", "GitHub Actions"]);
    let out = run_with_never(&said("on git hub actions"), &dict, &[("GitHub Actions", "git hub actions")]);
    assert_eq!(out.text, "On GitHub actions ");
}
