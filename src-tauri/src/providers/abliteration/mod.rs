use std::{sync::Arc, time::Duration};

use chrono::Utc;
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

const CREDITS_URL: &str = "https://api.abliteration.ai/v1/credits";
const ORGANIZATION_BALANCE_URL: &str = "https://api.abliteration.ai/v1/organization/balance";
const ENVIRONMENT_NAMES: &[&str] = &["ABLITERATION_API_KEY"];
const CONFIG_PATHS: &[&str] = &["~/.config/abliteration/key.json"];

pub(crate) fn definition() -> ProviderDefinition {
    ProviderDefinition {
        id: "abliteration".into(),
        display_name: "Abliteration.ai".into(),
        short_name: "Abl".into(),
        fallback_enabled: false,
        local_usage_source_note: None,
        links: vec![
            ProviderLink::new("Dashboard", "https://abliteration.ai/dashboard"),
            ProviderLink::new("API Keys", "https://abliteration.ai/settings/api-keys"),
        ],
        options: Vec::new(),
        metrics: vec![
            MetricDefinition::value(
                "abliteration.balance",
                "Balance",
                "balance",
                true,
                MetricSection::AlwaysVisible,
                true,
                "B",
                None,
            ),
            MetricDefinition::quota(
                "abliteration.credits",
                "Credits",
                "credits",
                false,
                true,
                MetricSection::AlwaysVisible,
                false,
                "C",
            ),
            MetricDefinition::value(
                "abliteration.used",
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
enum AbliterationError {
    #[error("Add an Abliteration.ai read-only API key in Customize or set ABLITERATION_API_KEY.")]
    MissingKey,
    #[error("The Abliteration.ai API key is invalid or cannot read billing credits.")]
    InvalidKey,
    #[error("Could not reach Abliteration.ai. Check your internet connection.")]
    ConnectionFailed,
    #[error("Abliteration.ai returned unrecognized credit data.")]
    InvalidResponse,
    #[error("Abliteration.ai request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("The Abliteration.ai API key could not be read or updated.")]
    CredentialStorage,
}

impl From<TransportError> for AbliterationError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::ConnectionFailed => Self::ConnectionFailed,
            TransportError::InvalidResponse => Self::InvalidResponse,
            TransportError::RateLimited => Self::RequestFailed(429),
        }
    }
}

impl From<AbliterationError> for ProviderError {
    fn from(error: AbliterationError) -> Self {
        let kind = match error {
            AbliterationError::MissingKey | AbliterationError::InvalidKey => {
                ProviderErrorKind::Authentication
            }
            AbliterationError::ConnectionFailed => ProviderErrorKind::Network,
            AbliterationError::RequestFailed(429) => ProviderErrorKind::RateLimited,
            AbliterationError::RequestFailed(401 | 403) => ProviderErrorKind::Authentication,
            AbliterationError::RequestFailed(_) | AbliterationError::InvalidResponse => {
                ProviderErrorKind::InvalidResponse
            }
            AbliterationError::CredentialStorage => ProviderErrorKind::CredentialStorage,
        };
        ProviderError::new(kind, error.to_string())
    }
}

struct AbliterationClient {
    client: reqwest::blocking::Client,
    credits_url: String,
    organization_balance_url: String,
}

impl AbliterationClient {
    fn new() -> Result<Self, AbliterationError> {
        Self::with_endpoints(
            CREDITS_URL,
            ORGANIZATION_BALANCE_URL,
            Duration::from_secs(15),
        )
    }

    fn with_endpoints(
        credits_url: &str,
        organization_balance_url: &str,
        timeout: Duration,
    ) -> Result<Self, AbliterationError> {
        Ok(Self {
            client: http::client(timeout)?,
            credits_url: credits_url.to_owned(),
            organization_balance_url: organization_balance_url.to_owned(),
        })
    }

    fn fetch(&self, api_key: &str) -> Result<EndpointResponse, AbliterationError> {
        // Organization Admin keys currently use the `sk_` prefix and expose
        // billing through the administration API. Model API keys use the
        // public credits endpoint instead. Keep the decision local so an
        // Admin key never produces a misleading "invalid key" message.
        let (url, endpoint) = if api_key.starts_with("sk_") {
            (&self.organization_balance_url, "organization balance")
        } else {
            (&self.credits_url, "credits")
        };
        http::get_json(&self.client, url, api_key, "abliteration", endpoint).map_err(From::from)
    }
}

pub struct AbliterationProvider {
    auth: ApiKeyStore,
    client: Arc<AbliterationClient>,
}

impl AbliterationProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            auth: ApiKeyStore::new_with_sources("abliteration", ENVIRONMENT_NAMES, CONFIG_PATHS),
            client: Arc::new(AbliterationClient::new().map_err(ProviderError::from)?),
        })
    }

    #[cfg(test)]
    fn with_endpoint(url: &str) -> Self {
        Self {
            auth: ApiKeyStore::new_with_sources("abliteration", &[], &[]),
            client: Arc::new(
                AbliterationClient::with_endpoints(url, url, Duration::from_secs(2)).unwrap(),
            ),
        }
    }

    fn refresh_snapshot(&self, api_key: &str) -> Result<ProviderSnapshot, ProviderError> {
        let response = self.client.fetch(api_key)?;
        if matches!(
            response.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(AbliterationError::InvalidKey.into());
        }
        if !response.status.is_success() {
            return Err(AbliterationError::RequestFailed(response.status.as_u16()).into());
        }
        if response.body.get("object").and_then(Value::as_str) == Some("organization.balance") {
            return organization_balance_snapshot(&response.body);
        }
        credits_snapshot(&response.body)
    }
}

fn credits_snapshot(body: &Value) -> Result<ProviderSnapshot, ProviderError> {
    let data = body
        .get("data")
        .and_then(Value::as_object)
        .ok_or(AbliterationError::InvalidResponse)?;
    let total =
        nonnegative_number(data.get("total_credits")).ok_or(AbliterationError::InvalidResponse)?;
    let used =
        nonnegative_number(data.get("total_usage")).ok_or(AbliterationError::InvalidResponse)?;
    let balance = (total - used).max(0.0);
    let quotas = (total > 0.0)
        .then(|| QuotaWindow {
            id: "credits".into(),
            label: "Credits".into(),
            used_percent: (used / total * 100.0).clamp(0.0, 100.0),
            resets_at: None,
            period_seconds: 0,
            format: QuotaFormat::Dollars,
            used_value: Some(used),
            limit_value: Some(total),
            unit: None,
            estimated: false,
            source_note: None,
        })
        .into_iter()
        .collect();
    Ok(ProviderSnapshot {
        provider_id: "abliteration".into(),
        plan: Some("Prepaid credits".into()),
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

fn organization_balance_snapshot(body: &Value) -> Result<ProviderSnapshot, ProviderError> {
    let balance =
        nonnegative_number(body.get("available_usd")).ok_or(AbliterationError::InvalidResponse)?;
    let reserved = nonnegative_number(body.get("reserved_usd")).unwrap_or(0.0);
    let owed = nonnegative_number(body.get("owed_usd")).unwrap_or(0.0);
    let mut warnings = Vec::new();
    if owed > 0.0 {
        warnings.push(format!("Abliteration.ai reports ${owed:.2} owed."));
    }
    Ok(ProviderSnapshot {
        provider_id: "abliteration".into(),
        plan: Some("Organization credits".into()),
        quotas: Vec::new(),
        value_metrics: vec![
            dollars("balance", "Balance", balance),
            dollars("used", "Reserved", reserved),
        ],
        status_metrics: Vec::new(),
        notices: Vec::new(),
        usage: UsageHistory::default(),
        warnings,
        refreshed_at: Utc::now(),
    })
}

impl UsageProvider for AbliterationProvider {
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
            .map_err(|_| ProviderError::from(AbliterationError::CredentialStorage))?
            .ok_or_else(|| ProviderError::from(AbliterationError::MissingKey))?;
        self.refresh_snapshot(api_key.as_str())
    }

    fn api_key_status(&self) -> Option<Result<ApiKeyStatus, ProviderError>> {
        Some(
            self.auth
                .status()
                .map_err(|_| ProviderError::from(AbliterationError::CredentialStorage)),
        )
    }

    fn supports_api_key_configuration(&self) -> bool {
        true
    }

    fn save_api_key(&self, value: &str) -> Result<(), ProviderError> {
        self.auth
            .save(value)
            .map_err(|_| ProviderError::from(AbliterationError::CredentialStorage))
    }

    fn delete_api_key(&self) -> Result<(), ProviderError> {
        self.auth
            .delete()
            .map_err(|_| ProviderError::from(AbliterationError::CredentialStorage))
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

    use super::AbliterationProvider;

    #[test]
    fn maps_credit_balance_and_usage() {
        let url = test_http::serve_once(
            200,
            &[],
            r#"{"data":{"total_credits":100,"total_usage":37.5}}"#,
        );
        let snapshot = AbliterationProvider::with_endpoint(&url)
            .refresh_snapshot("read-only-key")
            .unwrap();
        assert_eq!(snapshot.quotas[0].used_value, Some(37.5));
        assert_eq!(snapshot.value_metrics[0].values[0].number, 62.5);
    }

    #[test]
    fn maps_organization_admin_balance() {
        let url = test_http::serve_once(
            200,
            &[],
            r#"{"object":"organization.balance","available_usd":"6.146773","reserved_usd":"0.25","owed_usd":"0","standing_status":"good"}"#,
        );
        let snapshot = AbliterationProvider::with_endpoint(&url)
            .refresh_snapshot("sk_read-only-key")
            .unwrap();
        assert_eq!(snapshot.plan.as_deref(), Some("Organization credits"));
        assert_eq!(snapshot.value_metrics[0].values[0].number, 6.146773);
        assert_eq!(snapshot.value_metrics[1].values[0].number, 0.25);
    }
}
