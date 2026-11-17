use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_proto::{
    MAX_SELECTION_CHARS,
    api::{ApiErrorCode, Rewrite, RewriteRequest, Rewritten, routes},
};

use super::harness::*;

#[tokio::test]
async fn a_rewrite_that_cannot_run_says_why() {
    let server = default_server();
    let api = ApiClient::new(server.endpoint(), None);
    let fix = || Rewrite::Instruction("fix the spelling".into());
    let too_long = "x".repeat(MAX_SELECTION_CHARS + 1);
    for (selection, how, why) in [
        (" \n", fix(), "select the text first"),
        (too_long.as_str(), fix(), "select at most"),
        ("hey can u", Rewrite::Instruction("  ".into()), "say what to do"),
        ("hey can u", Rewrite::Style("shouty".into()), "no style \"shouty\""),
        ("hey can u", fix(), "connect ChatGPT in the web UI"),
        ("hey can u", Rewrite::Style("polished".into()), "connect ChatGPT in the web UI"),
    ] {
        let request = RewriteRequest { selection: selection.into(), how };
        let refused = api.call::<Rewritten>(Method::Post, routes::REWRITE, Some(&request)).await;
        let Err(ApiCallError::Api { error, .. }) = refused else { panic!("{refused:?}") };
        assert_eq!(error.code, ApiErrorCode::BadRequest);
        assert!(error.message.contains(why), "{error:?}");
    }
}
