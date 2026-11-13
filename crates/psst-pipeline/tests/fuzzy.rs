//! Fuzzy matching (plan 7.2 steps 3–7) and never rules, through `run`.
//!
//! Cases marked "Handy" are adapted from Handy's `src-tauri/src/audio_toolkit/text.rs`
//! (github.com/cjpais/Handy, MIT), and cases marked "t3code" from t3code's
//! `apps/server/src/speech/SpeechService.test.ts` (github.com/pingdotgg/t3code, MIT). Where psst's
//! rules give a different result, the comment says why.
//!
//! `said` has no word probabilities, which is the medium band; `said_p` and `said_words` give the
//! engine's probabilities.

mod support;

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

    // Handy: n-grams of up to four words.
    assert_eq!(text(&said("using Mac Book Pro"), &terms(&["MacBook Pro"])), "Using MacBook Pro ");
    assert_eq!(text(&said("use Chat G P T for this"), &terms(&["ChatGPT"])), "Use ChatGPT for this ");
    assert_eq!(text(&said("Open AI GPT model"), &terms(&["OpenAI", "GPT"])), "OpenAI GPT model ");
    assert_eq!(
        text(&said("its name is Charge B, which lets you bill"), &terms(&["ChargeBee"])),
        "Its name is ChargeBee, which lets you bill "
    );
    // Handy keeps the spoken case ("CHARGEBEE"); psst always writes the term's text.
    assert_eq!(text(&said("CHARGE B is great"), &terms(&["ChargeBee"])), "ChargeBee is great ");
    // Handy: a trailing digit is part of the key, so it's never written twice ("GPT-44").
    assert_eq!(text(&said("use GPT4 for this"), &terms(&["GPT-4"])), "Use GPT-4 for this ");
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
    // Handy: punctuation outside ASCII.
    assert_eq!(text(&said("「Handee。」"), &terms(&["Handy"])), "「Handy。」 ");
}

#[test]
fn only_letters_digits_hyphens_and_apostrophes_may_sit_inside_a_span() {
    assert_eq!(text(&said("the cuber netes team"), KUBERNETES), "The Kubernetes team ");
    assert_eq!(text(&said("the Cuber-Netes team"), KUBERNETES), "The Kubernetes team ");
    assert_eq!(text(&said("the cuber. Netes team"), KUBERNETES), "The cuber. Netes team ");
    assert_eq!(text(&said_p("not hyper. Land", 0.3), HYPRLAND), "Not hyper. Land ");
    // t3code: punctuation between the words keeps them apart.
    assert_eq!(text(&said("(git hub), git (hub)."), &terms(&["GitHub"])), "(GitHub), git (hub). ");
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

    // t3code: possessives.
    assert_eq!(text(&said("git hub's API, git hub’s API."), &terms(&["GitHub"])), "GitHub's API, GitHub’s API. ");
    let dict = terms(&["McDonald’s", "King’s Cross"]);
    assert_eq!(text(&said("mac donald's menu at king's cross"), &dict), "McDonald’s menu at King’s Cross ");
}

#[test]
fn a_possessive_inside_a_span_keeps_its_words_apart() {
    // t3code: "git’s hub" is not GitHub.
    assert_eq!(text(&said("git’s hub"), &terms(&["GitHub"])), "Git’s hub ");
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
    // Handy: "helo" is one letter off. Handy also fixes "wrold", but a swap is two edits out of
    // five letters.
    assert_eq!(text(&said("helo wrold"), &terms(&["hello", "world"])), "hello wrold ");
}

#[test]
fn keys_need_four_characters_and_a_similar_length() {
    assert_eq!(text(&said_p("run k 3 s here", 0.3), &terms(&["k3s"])), "Run k 3 s here ");
    // Handy matches R&D to "RD" and "R and D". Here "rd" is too short, and "rand" two letters off.
    let dict = terms(&["R&D"]);
    assert_eq!(text(&said_p("send it to RD for review", 0.3), &dict), "Send it to RD for review ");
    assert_eq!(text(&said_p("send it to R and D for review", 0.3), &dict), "Send it to R and D for review ");
    // Handy: other scripts get no fuzzy matching here either (and the key is too short).
    assert_eq!(text(&said_p("你好。", 0.3), &terms(&["你号"])), "你好。 ");
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
