use std::{sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::Value;
use thiserror::Error;

use crate::{
    models::{
        ApiKeyStatus, MetricDefinition, MetricSection, MetricValue, MetricValueKind,
        ProviderDefinition, ProviderErrorKind, ProviderLink, ProviderSnapshot, QuotaFormat,
        QuotaWindow, UsageHistory, ValueMetric,
    },
    providers::{
        api_key::ApiKeyStore,
        http::{self, EndpointResponse, TransportError},
        ProviderError, UsageProvider,
    },
};

const CREDITS_URL: &str = "https://api.experientiallabs.ai/api/v1/credits";
const ENVIRONMENT_NAMES: &[&str] = &["EXPERIENTIAL_API_KEY", "EXPERIENTIAL_LABS_API_KEY"];
const CONFIG_PATHS: &[&str] = &["~/.config/experiential/key.json"];
const DAY_SECONDS: u64 = 24 * 60 * 60;
const MONTH_SECONDS: u64 = 30 * DAY_SECONDS;

pub(crate) fn definition() -> ProviderDefinition {
    ProviderDefinition {
        id: "experiential".into(),
        display_name: "Experiential Labs".into(),
        short_name: "Xpl".into(),
        fallback_enabled: false,
        local_usage_source_note: None,
        links: vec![
            ProviderLink::new("Dashboard", "https://platform.experientiallabs.ai"),
            ProviderLink::new("API Keys", "https://platform.experientiallabs.ai/access"),
        ],
        options: Vec::new(),
        metrics: vec![
            MetricDefinition::value(
                "experiential.balance",
                "Balance",
                "balance",
                true,
                MetricSection::AlwaysVisible,
                true,
                "B",
                None,
            ),
            MetricDefinition::quota(
                "experiential.today",
                "Daily Allowance",
                "today",
                false,
                true,
                MetricSection::AlwaysVisible,
                false,
                "D",
            ),
            MetricDefinition::quota(
                "experiential.cycle",
                "Billing Cycle",
                "cycle",
                false,
                true,
                MetricSection::AlwaysVisible,
                false,
                "M",
            ),
            MetricDefinition::quota(
                "experiential.free",
                "Free Tokens",
                "free",
                false,
                true,
                MetricSection::AlwaysVisible,
                false,
                "F",
            ),
            MetricDefinition::value(
                "experiential.used",
                "Credits Used",
                "used",
                true,
                MetricSection::OnDemand,
                false,
                "U",
                None,
            ),
        ],
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
enum ExperientialError {
    #[error("Add an Experiential Labs API key in Customize or set EXPERIENTIAL_API_KEY.")]
    MissingKey,
    #[error("The Experiential Labs API key is invalid or cannot read credits.")]
    InvalidKey,
    #[error("Could not reach Experiential Labs. Check your internet connection.")]
    ConnectionFailed,
    #[error("Experiential Labs returned unrecognized credit data.")]
    InvalidResponse,
    #[error("Experiential Labs request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("The Experiential Labs API key could not be read or updated.")]
    CredentialStorage,
}

impl From<TransportError> for ExperientialError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::ConnectionFailed => Self::ConnectionFailed,
            TransportError::InvalidResponse => Self::InvalidResponse,
            TransportError::RateLimited => Self::RequestFailed(429),
        }
    }
}

impl From<ExperientialError> for ProviderError {
    fn from(error: ExperientialError) -> Self {
        let kind = match error {
            ExperientialError::MissingKey | ExperientialError::InvalidKey => {
                ProviderErrorKind::Authentication
            }
            ExperientialError::ConnectionFailed => ProviderErrorKind::Network,
            ExperientialError::RequestFailed(429) => ProviderErrorKind::RateLimited,
            ExperientialError::RequestFailed(401 | 403) => ProviderErrorKind::Authentication,
            ExperientialError::RequestFailed(_) | ExperientialError::InvalidResponse => {
                ProviderErrorKind::InvalidResponse
            }
            ExperientialError::CredentialStorage => ProviderErrorKind::CredentialStorage,
        };
        ProviderError::new(kind, error.to_string())
    }
}

struct ExperientialClient {
    client: reqwest::blocking::Client,
    credits_url: String,
}

impl ExperientialClient {
    fn new() -> Result<Self, ExperientialError> {
        Self::with_endpoint(CREDITS_URL, Duration::from_secs(15))
    }

    fn with_endpoint(credits_url: &str, timeout: Duration) -> Result<Self, ExperientialError> {
        Ok(Self {
            client: http::client(timeout)?,
            credits_url: credits_url.to_owned(),
        })
    }

    fn fetch(&self, api_key: &str) -> Result<EndpointResponse, ExperientialError> {
        http::get_json(
            &self.client,
            &self.credits_url,
            api_key,
            "experiential",
            "credits",
        )
        .map_err(From::from)
    }
}

pub struct ExperientialProvider {
    auth: ApiKeyStore,
    client: Arc<ExperientialClient>,
}

impl ExperientialProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            auth: ApiKeyStore::new_with_sources("experiential", ENVIRONMENT_NAMES, CONFIG_PATHS),
            client: Arc::new(ExperientialClient::new().map_err(ProviderError::from)?),
        })
    }

    #[cfg(test)]
    fn with_endpoint(url: &str) -> Self {
        Self {
            auth: ApiKeyStore::new_with_sources("experiential", &[], &[]),
            client: Arc::new(
                ExperientialClient::with_endpoint(url, Duration::from_secs(2)).unwrap(),
            ),
        }
    }

    fn refresh_snapshot(&self, api_key: &str) -> Result<ProviderSnapshot, ProviderError> {
        let response = self.client.fetch(api_key)?;
        if matches!(
            response.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(ExperientialError::InvalidKey.into());
        }
        if !response.status.is_success() {
            return Err(ExperientialError::RequestFailed(response.status.as_u16()).into());
        }
        credits_snapshot(&response.body)
    }
}

fn credits_snapshot(body: &Value) -> Result<ProviderSnapshot, ProviderError> {
    let data = body
        .get("data")
        .and_then(Value::as_object)
        .ok_or(ExperientialError::InvalidResponse)?;
    let total =
        nonnegative_number(data.get("total_credits")).ok_or(ExperientialError::InvalidResponse)?;
    let used =
        nonnegative_number(data.get("total_usage")).ok_or(ExperientialError::InvalidResponse)?;
    let balance = (total - used).max(0.0);

    let plan = data.get("plan").filter(|plan| plan.is_object());
    let tier = plan
        .and_then(|plan| plan.get("tier"))
        .and_then(Value::as_str)
        .map(capitalize);

    let mut quotas = Vec::new();
    if let Some(allowance) = plan.and_then(|plan| plan.get("allowance")) {
        // Plan allowances are denominated in credits, where 1 credit is 1 cent.
        if let Some(window) = credit_window(
            allowance.get("today"),
            "today",
            "Daily Allowance",
            DAY_SECONDS,
        ) {
            quotas.push(window);
        }
        if let Some(window) = credit_window(
            allowance.get("billing_cycle"),
            "cycle",
            "Billing Cycle",
            MONTH_SECONDS,
        ) {
            quotas.push(window);
        }
    }
    if let Some(window) = plan
        .and_then(|plan| plan.get("free_core"))
        .and_then(|free| token_window(free.get("today"), "free", "Free Tokens"))
    {
        quotas.push(window);
    }

    Ok(ProviderSnapshot {
        provider_id: "experiential".into(),
        plan: Some(tier.unwrap_or_else(|| "Credits".into())),
        quotas,
        value_metrics: vec![
            dollars("balance", "Balance", balance),
            dollars("used", "Credits Used", used),
        ],
        status_metrics: Vec::new(),
        notices: Vec::new(),
        usage: UsageHistory::default(),
        warnings: Vec::new(),
        refreshed_at: Utc::now(),
    })
}

fn credit_window(
    window: Option<&Value>,
    id: &str,
    label: &str,
    period_seconds: u64,
) -> Option<QuotaWindow> {
    let window = window?;
    let used = nonnegative_number(window.get("used_credits"))?;
    let limit = nonnegative_number(window.get("limit_credits")).filter(|limit| *limit > 0.0)?;
    Some(QuotaWindow {
        id: id.into(),
        label: label.into(),
        used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
        resets_at: reset_time(window.get("resets_at")),
        period_seconds,
        format: QuotaFormat::Dollars,
        used_value: Some(used / 100.0),
        limit_value: Some(limit / 100.0),
        unit: None,
        estimated: false,
        source_note: None,
    })
}

fn token_window(window: Option<&Value>, id: &str, label: &str) -> Option<QuotaWindow> {
    let window = window?;
    let used = nonnegative_number(window.get("used_tokens"))?;
    let limit = nonnegative_number(window.get("limit_tokens")).filter(|limit| *limit > 0.0)?;
    Some(QuotaWindow {
        id: id.into(),
        label: label.into(),
        used_percent: (used / limit * 100.0).clamp(0.0, 100.0),
        resets_at: reset_time(window.get("resets_at")),
        period_seconds: DAY_SECONDS,
        format: QuotaFormat::Count,
        used_value: Some(used),
        limit_value: Some(limit),
        unit: Some("tokens".into()),
        estimated: false,
        source_note: None,
    })
}

fn reset_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value?.as_str()?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

impl UsageProvider for ExperientialProvider {
    fn definition(&self) -> ProviderDefinition {
        definition()
    }

    fn has_local_credentials(&self) -> bool {
        self.auth.load().is_ok_and(|key| key.is_some())
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        let api_key = self
            .auth
            .load()
            .map_err(|_| ProviderError::from(ExperientialError::CredentialStorage))?
            .ok_or_else(|| ProviderError::from(ExperientialError::MissingKey))?;
        self.refresh_snapshot(api_key.as_str())
    }

    fn api_key_status(&self) -> Option<Result<ApiKeyStatus, ProviderError>> {
        Some(
            self.auth
                .status()
                .map_err(|_| ProviderError::from(ExperientialError::CredentialStorage)),
        )
    }

    fn supports_api_key_configuration(&self) -> bool {
        true
    }

    fn save_api_key(&self, value: &str) -> Result<(), ProviderError> {
        self.auth
            .save(value)
            .map_err(|_| ProviderError::from(ExperientialError::CredentialStorage))
    }

    fn delete_api_key(&self) -> Result<(), ProviderError> {
        self.auth
            .delete()
            .map_err(|_| ProviderError::from(ExperientialError::CredentialStorage))
    }
}

fn nonnegative_number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn dollars(id: &str, label: &str, number: f64) -> ValueMetric {
    ValueMetric {
        id: id.into(),
        label: label.into(),
        values: vec![MetricValue {
            number,
            kind: MetricValueKind::Dollars,
            label: None,
            estimated: false,
        }],
        expiries_at: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::providers::test_http;

    use super::ExperientialProvider;

    #[test]
    fn maps_balance_and_paid_plan_allowances() {
        let url = test_http::serve_once(
            200,
            &[],
            r#"{"data":{"total_credits":45,"total_usage":12.5,"plan":{"tier":"pro","allowance":{"today":{"used_credits":150,"limit_credits":700,"remaining_credits":550,"resets_at":"2026-11-22T00:00:00Z"},"billing_cycle":{"used_credits":1200,"limit_credits":2000,"remaining_credits":800,"resets_at":"2026-12-03T00:00:00Z"}},"included_models":[],"free_core":null,"retiring_benefits":[]}}}"#,
        );
        let snapshot = ExperientialProvider::with_endpoint(&url)
            .refresh_snapshot("xpl_read-only-key")
            .unwrap();
        assert_eq!(snapshot.plan.as_deref(), Some("Pro"));
        assert_eq!(snapshot.value_metrics[0].values[0].number, 32.5);
        assert_eq!(snapshot.quotas.len(), 2);
        assert_eq!(snapshot.quotas[0].id, "today");
        assert_eq!(snapshot.quotas[0].used_value, Some(1.5));
        assert_eq!(snapshot.quotas[0].limit_value, Some(7.0));
        assert!(snapshot.quotas[0].resets_at.is_some());
        assert_eq!(snapshot.quotas[1].used_percent, 60.0);
    }

    #[test]
    fn maps_free_plan_token_window_and_missing_plan() {
        let url = test_http::serve_once(
            200,
            &[],
            r#"{"data":{"total_credits":0,"total_usage":0,"plan":{"tier":"free","allowance":null,"included_models":[],"free_core":{"models":[],"today":{"window":"day","used_tokens":250000,"limit_tokens":1000000,"remaining_tokens":750000,"resets_at":"2026-11-22T00:00:00Z"},"continue_on_credits":false},"retiring_benefits":[]}}}"#,
        );
        let snapshot = ExperientialProvider::with_endpoint(&url)
            .refresh_snapshot("xpl_key")
            .unwrap();
        assert_eq!(snapshot.plan.as_deref(), Some("Free"));
        assert_eq!(snapshot.quotas.len(), 1);
        assert_eq!(snapshot.quotas[0].id, "free");
        assert_eq!(snapshot.quotas[0].used_percent, 25.0);

        let url = test_http::serve_once(
            200,
            &[],
            r#"{"data":{"total_credits":120.0,"total_usage":37.42,"plan":null}}"#,
        );
        let snapshot = ExperientialProvider::with_endpoint(&url)
            .refresh_snapshot("xpl_key")
            .unwrap();
        assert!(snapshot.quotas.is_empty());
        assert!((snapshot.value_metrics[0].values[0].number - 82.58).abs() < 1e-9);
    }

    #[test]
    fn rejects_unauthorized_key() {
        let url = test_http::serve_once(401, &[], r#"{"error":"Unauthorized"}"#);
        assert!(ExperientialProvider::with_endpoint(&url)
            .refresh_snapshot("xpl_bad")
            .is_err());
    }
}
