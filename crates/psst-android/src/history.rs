use psst_client::api::{ApiCallError, ApiClient, Method};
use psst_proto::{
    DeviceId, Endpoint, Token,
    api::{ApiErrorCode, HistoryRow, encode_segment, routes},
};

use crate::{
    HistoryItem,
    connect::{self, Failure},
};

const LIMIT: usize = 20;

pub(crate) async fn fetch(
    endpoint: Endpoint,
    token: Option<Token>,
    device: DeviceId,
) -> Result<Vec<HistoryItem>, String> {
    let api = ApiClient::new(endpoint, token);
    let path = format!("{}?limit={LIMIT}&device={}", routes::HISTORY, encode_segment(&device));
    let rows: Vec<HistoryRow> = api.call(Method::Get, &path, None::<&()>).await.map_err(message)?;
    Ok(items(rows, &device))
}

/// A server that predates `device` answers with every device's rows.
fn items(rows: Vec<HistoryRow>, device: &str) -> Vec<HistoryItem> {
    rows.into_iter()
        .filter(|row| row.device == device)
        .filter_map(|row| {
            let text = row.r#final?.text.trim().to_owned();
            (!text.is_empty()).then(|| HistoryItem { text, created_ms: row.created_at.as_millisecond() })
        })
        .take(LIMIT)
        .collect()
}

fn message(err: ApiCallError) -> String {
    match err {
        ApiCallError::Api { error, .. } if error.code == ApiErrorCode::Unauthorized => connect::no_longer_accepted(),
        ApiCallError::Api { error, .. } => {
            format!(
                "The server could not list recent dictations ({}); try again later.",
                connect::sentence(&error.message)
            )
        }
        ApiCallError::Connect { source, .. } => connect::unreachable(&source).message,
        ApiCallError::Timeout(_) => Failure::no_answer().message,
        ApiCallError::Protocol { .. } => connect::not_psst().message,
    }
}
