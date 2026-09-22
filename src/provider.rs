use std::collections::{HashMap, HashSet};
use std::env;
use std::io::Read;
use std::thread;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde_json::{Map, Value, json};

use crate::credentials::{Credentials, NO_KEY_HINT, find_credentials};
use crate::error::{ErrorCode, OrdainError, Result};
use crate::model::{Question, Rule, Thresholds, Usage, Verdict, band_for};

const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const JEV_USD_PER_INPUT_TOKEN: f64 = 0.042 / 1_000_000.0;

#[derive(Debug, Clone)]
pub struct CheckState {
    pub task: Option<String>,
    pub file: Option<String>,
    pub files: Option<Vec<String>>,
    pub diff: String,
    pub sources: Vec<crate::evidence::SourceEvidence>,
}

impl CheckState {
    fn json(&self) -> Value {
        let mut state = Map::new();
        if let Some(task) = &self.task {
            state.insert("task".into(), Value::String(task.clone()));
        }
        if let Some(file) = &self.file {
            state.insert("file".into(), Value::String(file.clone()));
        }
        if let Some(files) = &self.files {
            state.insert("files".into(), json!(files));
        }
        state.insert("diff".into(), Value::String(self.diff.clone()));
        if !self.sources.is_empty() {
            state.insert("source_evidence".into(), json!(self.sources));
        }
        Value::Object(state)
    }
}

#[derive(Debug, Clone)]
pub struct ModelCheckResult {
    pub verdicts: Vec<Verdict>,
    pub usage: Usage,
    pub latency_ms: u64,
    pub attempts: usize,
}

#[derive(Debug)]
pub struct ProviderFailure {
    pub error: OrdainError,
    pub attempts: usize,
}

enum Endpoint {
    TypeSafe,
    Gateway,
}

pub struct ProviderClient {
    client: Client,
    endpoint: Endpoint,
    key: String,
    url: String,
}

impl ProviderClient {
    pub fn new(root: &std::path::Path) -> Result<Self> {
        let credentials = find_credentials(root);
        let (endpoint, key, base) = match credentials {
            Credentials::TypeSafe { key, .. } => (
                Endpoint::TypeSafe,
                key,
                env::var("ORDAIN_TYPESAFE_BASE_URL")
                    .unwrap_or_else(|_| "https://api.typesafe.ai/v1".into()),
            ),
            Credentials::Gateway { key, .. } => (
                Endpoint::Gateway,
                key,
                env::var("ORDAIN_GATEWAY_BASE_URL")
                    .unwrap_or_else(|_| "https://ai-gateway.vercel.sh/v4/ai".into()),
            ),
            Credentials::None => return Err(OrdainError::new(ErrorCode::NoApiKey, NO_KEY_HINT)),
        };
        let suffix = match endpoint {
            Endpoint::TypeSafe => "systemone",
            Endpoint::Gateway => "evaluation-model",
        };
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(|error| failed(error.to_string()))?;
        Ok(Self {
            client,
            endpoint,
            key,
            url: format!("{}/{suffix}", base.trim_end_matches('/')),
        })
    }

    pub fn request_body(&self, rules: &[&Rule], state: &CheckState) -> Value {
        let questions = questions_json(rules, matches!(self.endpoint, Endpoint::TypeSafe));
        let mut state = state.json();
        state["rules"] = json!(
            rules
                .iter()
                .map(|r| json!({"id":r.id,"text":r.text,"source":r.source}))
                .collect::<Vec<_>>()
        );
        match self.endpoint {
            Endpoint::TypeSafe => {
                json!({"model":"jev-latest", "state":state, "questions":questions})
            }
            Endpoint::Gateway => {
                json!({"state":state, "questions":questions, "providerOptions":{"gateway":{"zeroDataRetention":true}}})
            }
        }
    }

    pub fn check(
        &self,
        rules: &[&Rule],
        state: &CheckState,
        thresholds: Thresholds,
        deadline: Instant,
        retries: usize,
    ) -> std::result::Result<ModelCheckResult, ProviderFailure> {
        if rules.is_empty() {
            return Ok(ModelCheckResult {
                verdicts: Vec::new(),
                usage: Usage::default(),
                latency_ms: 0,
                attempts: 0,
            });
        }
        let started = Instant::now();
        let mut last_error = None;
        let mut attempts = 0;
        for attempt in 0..=retries {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(ProviderFailure {
                    error: OrdainError::new(
                        ErrorCode::CheckTimeout,
                        "the gateway did not answer within the time allowed",
                    ),
                    attempts,
                });
            };
            attempts += 1;
            match self.request_once(rules, state, thresholds, remaining) {
                Ok(mut result) => {
                    result.latency_ms = elapsed_ms(started);
                    result.attempts = attempts;
                    return Ok(result);
                }
                Err((error, transient)) => {
                    last_error = Some(error);
                    if !transient || attempt == retries {
                        break;
                    }
                    let delay = Duration::from_millis(50 * (attempt as u64 + 1));
                    if Instant::now() + delay >= deadline {
                        break;
                    }
                    thread::sleep(delay);
                }
            }
        }
        Err(ProviderFailure {
            error: last_error.unwrap_or_else(|| failed("the gateway request failed")),
            attempts,
        })
    }

    fn request_once(
        &self,
        rules: &[&Rule],
        state: &CheckState,
        thresholds: Thresholds,
        timeout: Duration,
    ) -> std::result::Result<ModelCheckResult, (OrdainError, bool)> {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.key))
                .map_err(|_| (failed("credential could not be represented"), false))?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(USER_AGENT, HeaderValue::from_static("ordain/0.1.0"));

        if matches!(self.endpoint, Endpoint::Gateway) {
            headers.insert(
                HeaderName::from_static("ai-gateway-protocol-version"),
                HeaderValue::from_static("0.0.1"),
            );
            headers.insert(
                HeaderName::from_static("ai-gateway-auth-method"),
                HeaderValue::from_static("api-key"),
            );
            headers.insert(
                HeaderName::from_static("ai-evaluation-model-specification-version"),
                HeaderValue::from_static("4"),
            );
            headers.insert(
                HeaderName::from_static("ai-model-id"),
                HeaderValue::from_static("typesafe-ai/jev"),
            );
        }
        let body = self.request_body(rules, state);
        let response = self
            .client
            .post(&self.url)
            .headers(headers)
            .timeout(timeout)
            .json(&body)
            .send()
            .map_err(|error| {
                if error.is_timeout() {
                    (
                        OrdainError::new(
                            ErrorCode::CheckTimeout,
                            "the gateway did not answer within the time allowed",
                        ),
                        true,
                    )
                } else {
                    (failed(error.to_string()), true)
                }
            })?;
        parse_response(&self.endpoint, response, rules, thresholds, &self.key)
    }
}

fn questions_json(rules: &[&Rule], direct: bool) -> Value {
    let mut questions = Map::new();
    for rule in rules {
        let crate::model::Check::Model { question, .. } = &rule.check else {
            continue;
        };
        let value = match question {
            Question::Boolean {
                instructions,
                criteria,
            } => json!({
                "type": if direct { "noul" } else { "boolean" },
                "instructions": instructions,
                "criteria": criteria
            }),
            Question::Choice {
                instructions,
                criteria,
                ..
            } => json!({"type":"choice", "instructions":instructions, "criteria":criteria}),
            Question::Score {
                instructions,
                criteria,
                ..
            } => json!({"type":"score", "instructions":instructions, "criteria":criteria}),
        };
        questions.insert(rule.id.clone(), strip_nulls(value));
    }
    Value::Object(questions)
}

fn strip_nulls(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        map.retain(|_, value| !value.is_null());
    }
    value
}

fn parse_response(
    endpoint: &Endpoint,
    mut response: Response,
    rules: &[&Rule],
    thresholds: Thresholds,
    key: &str,
) -> std::result::Result<ModelCheckResult, (OrdainError, bool)> {
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| (failed(error.to_string()), true))?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err((failed("gateway response exceeded 1 MiB"), false));
    }
    if !status.is_success() {
        let message = safe_error_message(&bytes, key);
        let transient = matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
        ) || status.is_server_error();
        return Err((
            failed(format!(
                "the gateway answered {}{}",
                status.as_u16(),
                message.map_or(String::new(), |message| format!(": {message}"))
            )),
            transient,
        ));
    }
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|_| (failed("the gateway returned invalid JSON"), false))?;
    let answers = body
        .get("answers")
        .and_then(Value::as_object)
        .ok_or_else(|| (failed("the gateway response has no answer map"), false))?;
    let expected: HashSet<&str> = rules.iter().map(|rule| rule.id.as_str()).collect();
    let actual: HashSet<&str> = answers.keys().map(String::as_str).collect();
    if actual != expected {
        return Err((
            failed("the gateway must return exactly one answer for every question"),
            false,
        ));
    }
    let (probability_decimals, score_decimals) = match endpoint {
        Endpoint::TypeSafe => (Some(2), Some(2)),
        Endpoint::Gateway => (
            body.get("rounding")
                .and_then(|value| value.get("probabilityDecimals"))
                .and_then(Value::as_u64)
                .map(|value| value as u32),
            body.get("rounding")
                .and_then(|value| value.get("scoreDecimals"))
                .and_then(Value::as_u64)
                .map(|value| value as u32),
        ),
    };
    let mut verdicts = Vec::with_capacity(rules.len());
    for rule in rules {
        let crate::model::Check::Model { question, .. } = &rule.check else {
            continue;
        };
        let answer = &answers[&rule.id];
        let (probability, selected) = answer_probability(
            endpoint,
            question,
            answer,
            probability_decimals,
            score_decimals,
            &rule.id,
        )
        .map_err(|message| (failed(message), false))?;
        let probability = probability.clamp(0.0, 1.0);
        verdicts.push(Verdict {
            rule_id: rule.id.clone(),
            action: crate::config::Action::Record,
            policy_revision: String::new(),
            evidence: None,
            probability,
            band: band_for(probability, thresholds),
            answer: selected,
        });
    }
    let usage_value = body.get("usage");
    let direct = matches!(endpoint, Endpoint::TypeSafe);
    let input_tokens = usage_value
        .and_then(|usage| {
            usage.get(if direct {
                "input_tokens"
            } else {
                "inputTokens"
            })
        })
        .and_then(Value::as_u64);
    let output_tokens = usage_value
        .and_then(|usage| {
            usage.get(if direct {
                "output_tokens"
            } else {
                "outputTokens"
            })
        })
        .and_then(Value::as_u64);
    Ok(ModelCheckResult {
        verdicts,
        usage: Usage {
            input_tokens,
            output_tokens,
            cost_usd: input_tokens.map(|tokens| tokens as f64 * JEV_USD_PER_INPUT_TOKEN),
        },
        latency_ms: 0,
        attempts: 0,
    })
}

fn answer_probability(
    endpoint: &Endpoint,
    question: &Question,
    answer: &Value,
    probability_decimals: Option<u32>,
    score_decimals: Option<u32>,
    id: &str,
) -> std::result::Result<(f64, Option<String>), String> {
    let object = answer
        .as_object()
        .ok_or_else(|| format!("question {id:?} returned a non-object answer"))?;
    match question {
        Question::Boolean { .. } => {
            let expected = if matches!(endpoint, Endpoint::TypeSafe) {
                "noul"
            } else {
                "boolean"
            };
            if object.get("type").and_then(Value::as_str) != Some(expected) {
                return Err(format!("question {id:?} returned the wrong answer type"));
            }
            let key = if matches!(endpoint, Endpoint::TypeSafe) {
                "noul"
            } else {
                "probability"
            };
            let probability = finite_probability(object.get(key), id)?;
            Ok((probability, None))
        }
        Question::Choice {
            criteria,
            violating,
            ..
        } => {
            if object.get("type").and_then(Value::as_str) != Some("choice") {
                return Err(format!("question {id:?} returned the wrong answer type"));
            }
            let selected = object
                .get("choice")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("question {id:?} did not select an option"))?;
            if !criteria.contains_key(selected) {
                return Err(format!("question {id:?} selected an unknown option"));
            }
            if let Some(distribution) = object.get("probabilities") {
                let distribution = validate_distribution(
                    distribution,
                    criteria.keys().map(String::as_str),
                    probability_decimals,
                    id,
                )?;
                let selected_probability = distribution[selected];
                if distribution
                    .values()
                    .any(|probability| *probability > selected_probability + 1e-6)
                {
                    return Err(format!(
                        "question {id:?} did not select a highest-probability option"
                    ));
                }
                Ok((
                    violating
                        .iter()
                        .map(|name| distribution[name.as_str()])
                        .sum(),
                    Some(selected.into()),
                ))
            } else {
                Ok((
                    f64::from(violating.iter().any(|name| name == selected)),
                    Some(selected.into()),
                ))
            }
        }
        Question::Score {
            criteria,
            violating_from,
            ..
        } => {
            if object.get("type").and_then(Value::as_str) != Some("score") {
                return Err(format!("question {id:?} returned the wrong answer type"));
            }
            let score = object
                .get("score")
                .and_then(Value::as_f64)
                .filter(|value| {
                    value.is_finite() && *value >= 0.0 && *value <= criteria.len() as f64 - 1.0
                })
                .ok_or_else(|| format!("question {id:?} returned an invalid score"))?;
            let rounded = score.round() as usize;
            let label = criteria
                .get(rounded)
                .cloned()
                .unwrap_or_else(|| rounded.to_string());
            if let Some(distribution) = object.get("probabilities") {
                let keys = (0..criteria.len())
                    .map(|index| index.to_string())
                    .collect::<Vec<_>>();
                let distribution = validate_distribution(
                    distribution,
                    keys.iter().map(String::as_str),
                    probability_decimals,
                    id,
                )?;
                let mean = distribution
                    .iter()
                    .try_fold(0.0, |total, (index, probability)| {
                        index
                            .parse::<usize>()
                            .ok()
                            .map(|index| total + index as f64 * probability)
                    })
                    .ok_or_else(|| {
                        format!("question {id:?} returned an invalid score distribution")
                    })?;
                let probability_error =
                    probability_decimals.map_or(0.0, |digits| 0.5 * 10f64.powi(-(digits as i32)));
                let mean_error = (0..criteria.len())
                    .map(|index| index as f64 * probability_error)
                    .sum::<f64>();
                let score_error =
                    score_decimals.map_or(0.0, |digits| 0.5 * 10f64.powi(-(digits as i32)));
                if (mean - score).abs() > 1e-6 + mean_error + score_error {
                    return Err(format!(
                        "question {id:?} score does not match its probability distribution"
                    ));
                }
                let probability = distribution
                    .iter()
                    .filter(|(index, _)| {
                        index
                            .parse::<usize>()
                            .is_ok_and(|index| index >= *violating_from)
                    })
                    .map(|(_, probability)| probability)
                    .sum();
                Ok((probability, Some(label)))
            } else {
                Ok((f64::from(score >= *violating_from as f64), Some(label)))
            }
        }
    }
}

fn finite_probability(value: Option<&Value>, id: &str) -> std::result::Result<f64, String> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
        .ok_or_else(|| format!("question {id:?} returned an invalid probability"))
}

fn validate_distribution<'a>(
    value: &Value,
    keys: impl Iterator<Item = &'a str>,
    decimals: Option<u32>,
    id: &str,
) -> std::result::Result<HashMap<String, f64>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("question {id:?} returned an invalid probability distribution"))?;
    let expected: HashSet<&str> = keys.collect();
    let actual: HashSet<&str> = object.keys().map(String::as_str).collect();
    if expected != actual {
        return Err(format!(
            "question {id:?} returned an incomplete probability distribution"
        ));
    }
    let mut values = HashMap::new();
    for (key, value) in object {
        values.insert(key.clone(), finite_probability(Some(value), id)?);
    }
    let sum: f64 = values.values().sum();
    let rounding_error = decimals.map_or(0.0, |digits| 0.5 * 10f64.powi(-(digits as i32)));
    if (sum - 1.0).abs() > 1e-6 + expected.len() as f64 * rounding_error {
        return Err(format!("question {id:?} probabilities do not sum to one"));
    }
    Ok(values)
}

fn safe_error_message(bytes: &[u8], key: &str) -> Option<String> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let text = value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .or_else(|| value.get("error")?.get("message")?.as_str())
        .or_else(|| value.get("detail").and_then(Value::as_str))?;
    let text = text.chars().take(160).collect::<String>();
    Some(if key.is_empty() {
        text
    } else {
        text.replace(key, "[redacted]")
    })
}

fn failed(message: impl Into<String>) -> OrdainError {
    OrdainError::new(ErrorCode::CheckFailed, message)
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

pub fn sdk_questions_for_test(rules: &[Rule], direct: bool) -> Value {
    questions_json(&rules.iter().collect::<Vec<_>>(), direct)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::model::Check;

    use super::*;

    fn choice_rule() -> Rule {
        Rule {
            id: "shape".into(),
            text: "shape".into(),
            source: crate::model::RuleSource {
                path: "AGENTS.md".into(),
                line: None,
            },
            scope: None,
            when: Some(crate::model::Phase::Edit),
            check: Check::Model {
                question: Question::Choice {
                    instructions: "shape?".into(),
                    criteria: BTreeMap::from([
                        ("good".into(), "g".into()),
                        ("bad".into(), "b".into()),
                    ]),
                    violating: vec!["bad".into()],
                },
                overlaps: None,
            },
            status: crate::model::RuleStatus::Active,
            calibration: None,
            origin: None,
        }
    }

    #[test]
    fn direct_questions_use_noul_and_omit_local_violation_metadata() {
        let boolean = Rule {
            id: "b".into(),
            check: Check::Model {
                question: Question::Boolean {
                    instructions: "?".into(),
                    criteria: None,
                },
                overlaps: None,
            },
            ..choice_rule()
        };
        let value = sdk_questions_for_test(&[boolean, choice_rule()], true);
        assert_eq!(value["b"]["type"], "noul");
        assert!(value["shape"].get("violating").is_none());
    }

    #[test]
    fn choice_uses_violating_probability_mass() {
        let rule = choice_rule();
        let question = match &rule.check {
            Check::Model { question, .. } => question,
            _ => unreachable!(),
        };
        let (probability, selected) = answer_probability(
            &Endpoint::Gateway,
            question,
            &json!({"type":"choice","choice":"good","probabilities":{"good":0.6,"bad":0.4}}),
            None,
            None,
            "shape",
        )
        .unwrap();
        assert_eq!(probability, 0.4);
        assert_eq!(selected.as_deref(), Some("good"));
    }

    #[test]
    fn provider_error_messages_redact_the_active_key() {
        let message = safe_error_message(
            br#"{"message":"request for secret-fixture-key was rejected"}"#,
            "secret-fixture-key",
        )
        .unwrap();
        assert_eq!(message, "request for [redacted] was rejected");
    }
}
