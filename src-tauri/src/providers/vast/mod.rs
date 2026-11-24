use std::{sync::Arc, time::Duration};

use chrono::Utc;
use reqwest::StatusCode;
use serde_json::Value;
use thiserror::Error;

use crate::{
    models::{
        ApiKeyStatus, MetricDefinition, MetricSection, MetricValue, MetricValueKind,
        ProviderDefinition, ProviderErrorKind, ProviderLink, ProviderSnapshot, UsageHistory,
        ValueMetric,
    },
    providers::{
        api_key::ApiKeyStore,
        http::{self, EndpointResponse, TransportError},
        ProviderError, UsageProvider,
    },
};

const ACCOUNT_URL: &str = "https://console.vast.ai/api/v0/users/current/";
const INSTANCES_URL: &str = "https://console.vast.ai/api/v1/instances/?limit=25";
const ENVIRONMENT_NAMES: &[&str] = &["VAST_API_KEY"];
const CONFIG_PATHS: &[&str] = &["~/.config/vastai/vast_api_key", "~/.vast_api_key"];

pub(crate) fn definition() -> ProviderDefinition {
    ProviderDefinition {
        id: "vast".into(),
        display_name: "Vast.ai".into(),
        short_name: "Vast".into(),
        fallback_enabled: false,
        local_usage_source_note: None,
        links: vec![
            ProviderLink::new("Billing", "https://cloud.vast.ai/billing/"),
            ProviderLink::new("API Keys", "https://cloud.vast.ai/manage-keys/"),
            ProviderLink::new("Instances", "https://cloud.vast.ai/instances/"),
        ],
        options: Vec::new(),
        metrics: vec![
            MetricDefinition::value(
                "vast.credit",
                "Credit",
                "credit",
                true,
                MetricSection::AlwaysVisible,
                true,
                "C",
                None,
            ),
            MetricDefinition::value(
                "vast.burn",
                "Hourly Burn",
                "burn",
                true,
                MetricSection::AlwaysVisible,
                false,
                "Burn",
                Some("/h"),
            ),
            MetricDefinition::value(
                "vast.runway",
                "Runway",
                "runway",
                true,
                MetricSection::AlwaysVisible,
                false,
                "Runway",
                Some("h"),
            ),
            MetricDefinition::value(
                "vast.instances",
                "Running Instances",
                "instances",
                true,
                MetricSection::OnDemand,
                false,
                "GPU",
                None,
            ),
        ],
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
enum VastError {
    #[error("Add a Vast.ai read-only API key in Customize or set VAST_API_KEY.")]
    MissingKey,
    #[error("The Vast.ai API key is invalid or cannot read billing information.")]
    InvalidKey,
    #[error("Could not reach Vast.ai. Check your internet connection.")]
    ConnectionFailed,
    #[error("Vast.ai returned unrecognized billing data.")]
    InvalidResponse,
    #[error("Vast.ai request failed (HTTP {0}).")]
    RequestFailed(u16),
    #[error("The Vast.ai API key could not be read or updated.")]
    CredentialStorage,
}

impl From<TransportError> for VastError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::ConnectionFailed => Self::ConnectionFailed,
            TransportError::InvalidResponse => Self::InvalidResponse,
            TransportError::RateLimited => Self::RequestFailed(429),
        }
    }
}

impl From<VastError> for ProviderError {
    fn from(error: VastError) -> Self {
        let kind = match error {
            VastError::MissingKey | VastError::InvalidKey => ProviderErrorKind::Authentication,
            VastError::ConnectionFailed => ProviderErrorKind::Network,
            VastError::RequestFailed(429) => ProviderErrorKind::RateLimited,
            VastError::RequestFailed(401 | 403) => ProviderErrorKind::Authentication,
            VastError::RequestFailed(_) | VastError::InvalidResponse => {
                ProviderErrorKind::InvalidResponse
            }
            VastError::CredentialStorage => ProviderErrorKind::CredentialStorage,
        };
        ProviderError::new(kind, error.to_string())
    }
}

struct VastClient {
    client: reqwest::blocking::Client,
    account_url: String,
    instances_url: String,
}

impl VastClient {
    fn new() -> Result<Self, VastError> {
        Self::with_endpoints(ACCOUNT_URL, INSTANCES_URL, Duration::from_secs(15))
    }

    fn with_endpoints(
        account_url: &str,
        instances_url: &str,
        timeout: Duration,
    ) -> Result<Self, VastError> {
        Ok(Self {
            client: http::client(timeout)?,
            account_url: account_url.to_owned(),
            instances_url: instances_url.to_owned(),
        })
    }

    fn fetch_account(&self, api_key: &str) -> Result<EndpointResponse, VastError> {
        http::get_json(&self.client, &self.account_url, api_key, "vast", "account")
            .map_err(From::from)
    }

    fn fetch_instances(&self, api_key: &str) -> Result<EndpointResponse, VastError> {
        http::get_json(
            &self.client,
            &self.instances_url,
            api_key,
            "vast",
            "instances",
        )
        .map_err(From::from)
    }
}

pub struct VastProvider {
    auth: ApiKeyStore,
    client: Arc<VastClient>,
}

impl VastProvider {
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            auth: ApiKeyStore::new_with_sources("vast", ENVIRONMENT_NAMES, CONFIG_PATHS),
            client: Arc::new(VastClient::new().map_err(ProviderError::from)?),
        })
    }

    #[cfg(test)]
    fn with_endpoints(account_url: &str, instances_url: &str) -> Self {
        Self {
            auth: ApiKeyStore::new_with_sources("vast", &[], &[]),
            client: Arc::new(
                VastClient::with_endpoints(account_url, instances_url, Duration::from_secs(2))
                    .unwrap(),
            ),
        }
    }

    fn refresh_snapshot(&self, api_key: &str) -> Result<ProviderSnapshot, ProviderError> {
        let account = self.client.fetch_account(api_key)?;
        if matches!(
            account.status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            return Err(VastError::InvalidKey.into());
        }
        if !account.status.is_success() {
            return Err(VastError::RequestFailed(account.status.as_u16()).into());
        }
        let credit = number(account.body.get("credit")).ok_or(VastError::InvalidResponse)?;
        let mut values = vec![dollars("credit", "Credit", credit, false)];
        let mut warnings = Vec::new();

        match self.client.fetch_instances(api_key) {
            Ok(response) if response.status.is_success() => {
                if let Some(instances) = response.body.get("instances").and_then(Value::as_array) {
                    let active = instances
                        .iter()
                        .filter(|instance| {
                            instance.get("actual_status").and_then(Value::as_str) == Some("running")
                                || instance.get("intended_status").and_then(Value::as_str)
                                    == Some("running")
                        })
                        .collect::<Vec<_>>();
                    let burn = active
                        .iter()
                        .filter_map(|instance| number(instance.get("dph_total")))
                        .filter(|cost| *cost >= 0.0)
                        .sum::<f64>();
                    values.push(dollars("burn", "Hourly Burn", burn, true));
                    values.push(count("instances", "Running Instances", active.len() as f64));
                    if credit > 0.0 && burn > 0.0 {
                        values.push(count("runway", "Runway", credit / burn));
                    }
                }
            }
            Ok(response)
                if matches!(
                    response.status,
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                ) =>
            {
                warnings.push("This key can read billing credit but not instances; burn and runway are hidden.".into());
            }
            Ok(_) | Err(_) => {
                warnings.push(
                    "Vast instance data is temporarily unavailable; credit is still current."
                        .into(),
                );
            }
        }

        Ok(ProviderSnapshot {
            provider_id: "vast".into(),
            plan: Some("Prepaid GPU credit".into()),
            quotas: Vec::new(),
            value_metrics: values,
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage: UsageHistory::default(),
            warnings,
            refreshed_at: Utc::now(),
        })
    }
}

impl UsageProvider for VastProvider {
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
            .map_err(|_| ProviderError::from(VastError::CredentialStorage))?
            .ok_or_else(|| ProviderError::from(VastError::MissingKey))?;
        self.refresh_snapshot(api_key.as_str())
    }

    fn api_key_status(&self) -> Option<Result<ApiKeyStatus, ProviderError>> {
        Some(
            self.auth
                .status()
                .map_err(|_| ProviderError::from(VastError::CredentialStorage)),
        )
    }

    fn supports_api_key_configuration(&self) -> bool {
        true
    }

    fn save_api_key(&self, value: &str) -> Result<(), ProviderError> {
        self.auth
            .save(value)
            .map_err(|_| ProviderError::from(VastError::CredentialStorage))
    }

    fn delete_api_key(&self) -> Result<(), ProviderError> {
        self.auth
            .delete()
            .map_err(|_| ProviderError::from(VastError::CredentialStorage))
    }
}

fn number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .filter(|value| value.is_finite())
}

fn dollars(id: &str, label: &str, number: f64, estimated: bool) -> ValueMetric {
    ValueMetric {
        id: id.into(),
        label: label.into(),
        values: vec![MetricValue {
            number,
            kind: MetricValueKind::Dollars,
            label: None,
            estimated,
        }],
        expiries_at: Vec::new(),
    }
}

fn count(id: &str, label: &str, number: f64) -> ValueMetric {
    ValueMetric {
        id: id.into(),
        label: label.into(),
        values: vec![MetricValue {
            number,
            kind: MetricValueKind::Count,
            label: None,
            estimated: id == "runway",
        }],
        expiries_at: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use crate::providers::test_http;

    use super::VastProvider;

    #[test]
    fn maps_credit_burn_and_runway() {
        let account_url = test_http::serve_once(200, &[], r#"{"credit":12.0,"balance":0}"#);
        let instances_url = test_http::serve_once(
            200,
            &[],
            r#"{"instances":[{"actual_status":"running","intended_status":"running","dph_total":0.5},{"actual_status":"exited","intended_status":"stopped","dph_total":2.0}]}"#,
        );
        let provider = VastProvider::with_endpoints(&account_url, &instances_url);
        let snapshot = provider.refresh_snapshot("billing-key").unwrap();
        assert_eq!(snapshot.value_metrics[0].values[0].number, 12.0);
        assert_eq!(snapshot.value_metrics[1].values[0].number, 0.5);
        assert_eq!(snapshot.value_metrics[3].values[0].number, 24.0);
    }
}
