//! Тесты `ServerAgent` против замоканного сервиса.

use super::*;
use agentcore::config::{Provider, ResponseFormat, SamplingParams};
use serde_json::json;
use wiremock::matchers::{body_json_schema, header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn agent(server: &MockServer, token: &str) -> ServerAgent {
    ServerAgent::new(
        server.uri(),
        token,
        "deepseek-chat",
        Arc::new(ExchangeLog::disabled()),
    )
}

fn history() -> Vec<Message> {
    vec![Message::user("привет")]
}

fn cloud_settings() -> ChatSettings {
    ChatSettings {
        provider: Provider::Cloud,
        ..ChatSettings::default()
    }
}

fn success_body() -> serde_json::Value {
    json!({
        "request_id": "req-1",
        "content": "ответ",
        "reasoning": " рассуждение ",
        "model": "deepseek-reasoner",
        "usage": {
            "prompt_tokens": 11,
            "completion_tokens": 22,
            "total_tokens": 33,
            "reasoning_tokens": 5
        },
        "timing": {
            "duration_ms": 1234,
            "sent_at": 1000,
            "received_at": 1002
        },
        "policy": { "input": [], "output": [], "judge": null }
    })
}

async fn mount_chat(server: &MockServer, status: u16, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path("/v1/chat"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("x-request-id", "req-header")
                .set_body_json(body),
        )
        .mount(server)
        .await;
}

fn error_body(code: &str, message: &str) -> serde_json::Value {
    json!({ "error": { "code": code, "message": message, "request_id": "req-1" } })
}

async fn ask_error(status: u16, body: serde_json::Value) -> anyhow::Error {
    let server = MockServer::start().await;
    mount_chat(&server, status, body).await;
    agent(&server, "t")
        .with_unauthorized_hint("выполните: agentcli config set-token")
        .ask(&history(), &cloud_settings())
        .await
        .expect_err("ожидалась ошибка сервиса")
}

#[tokio::test]
async fn success_is_parsed_with_usage_and_timing() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;

    let reply = agent(&server, "token")
        .ask(&history(), &cloud_settings())
        .await
        .expect("ответ сервиса");

    assert_eq!(reply.content, "ответ");
    assert_eq!(reply.reasoning.as_deref(), Some("рассуждение"));
    // Показывается модель из ответа, а не запрошенная.
    assert_eq!(reply.model.as_deref(), Some("deepseek-reasoner"));
    assert_eq!(reply.meta.prompt_tokens, Some(11));
    assert_eq!(reply.meta.completion_tokens, Some(22));
    assert_eq!(reply.meta.total_tokens, Some(33));
    assert_eq!(reply.meta.reasoning_tokens, Some(5));
    assert_eq!(reply.meta.duration_ms, Some(1234));
    assert_eq!(reply.meta.sent_at, Some(1000));
    assert_eq!(reply.meta.received_at, Some(1002));
    assert!(reply.policy.is_some(), "блок policy должен разбираться");
}

#[tokio::test]
async fn unauthorized_carries_hint_and_request_id() {
    let err = ask_error(401, error_body("unauthorized", "нужен токен")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Unauthorized { hint, request_id }) => {
            assert_eq!(hint.as_deref(), Some("выполните: agentcli config set-token"));
            assert_eq!(request_id.as_deref(), Some("req-1"));
        }
        other => panic!("ожидался Unauthorized, получено: {other:?}"),
    }
    let text = format!("{err}");
    assert!(text.contains("agentcli config set-token"), "текст: {text}");
    assert!(text.contains("req-1"), "текст: {text}");
}

#[tokio::test]
async fn invalid_request_is_typed() {
    let err = ask_error(400, error_body("invalid_request", "пустая история")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::InvalidRequest {
            message,
            request_id,
        }) => {
            assert_eq!(message, "пустая история");
            assert_eq!(request_id.as_deref(), Some("req-1"));
        }
        other => panic!("ожидался InvalidRequest, получено: {other:?}"),
    }
}

#[tokio::test]
async fn policy_rejection_is_typed() {
    let err = ask_error(422, error_body("policy_rejected", "banned_words: мат")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::PolicyRejected {
            code,
            reason,
            request_id,
        }) => {
            assert_eq!(code, "policy_rejected");
            assert_eq!(reason, "banned_words: мат");
            assert_eq!(request_id.as_deref(), Some("req-1"));
        }
        other => panic!("ожидался PolicyRejected, получено: {other:?}"),
    }
}

#[tokio::test]
async fn rate_limit_is_typed() {
    let err = ask_error(429, error_body("rate_limited", "слишком часто")).await;
    assert!(
        matches!(
            err.downcast_ref::<AgentError>(),
            Some(AgentError::RateLimited { .. })
        ),
        "получено: {err:#}"
    );
}

#[tokio::test]
async fn upstream_error_is_provider_error() {
    let err = ask_error(502, error_body("upstream_error", "провайдер недоступен")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Provider {
            status,
            message,
            request_id,
        }) => {
            assert_eq!(*status, 502);
            assert_eq!(message, "провайдер недоступен");
            assert_eq!(request_id.as_deref(), Some("req-1"));
        }
        other => panic!("ожидался Provider, получено: {other:?}"),
    }
}

#[tokio::test]
async fn upstream_timeout_is_typed() {
    let err = ask_error(504, error_body("upstream_timeout", "нет ответа")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Timeout { request_id }) => {
            assert_eq!(request_id.as_deref(), Some("req-1"));
        }
        other => panic!("ожидался Timeout, получено: {other:?}"),
    }
}

#[tokio::test]
async fn request_id_comes_from_header_when_body_has_none() {
    let server = MockServer::start().await;
    mount_chat(&server, 502, json!({})).await;
    let err = agent(&server, "t")
        .ask(&history(), &cloud_settings())
        .await
        .expect_err("ожидалась ошибка");
    assert!(
        format!("{err}").contains("req-header"),
        "идентификатор из заголовка должен попадать в текст: {err:#}"
    );
}

/// Адрес, на котором заведомо никто не слушает: порт 1 зарезервирован.
const UNREACHABLE_URL: &str = "http://127.0.0.1:1";

#[tokio::test]
async fn unreachable_server_is_transport_error() {
    let uri = UNREACHABLE_URL.to_string();

    let err = ServerAgent::new(uri.clone(), "", "", Arc::new(ExchangeLog::disabled()))
        .ask(&history(), &cloud_settings())
        .await
        .expect_err("ожидалась транспортная ошибка");

    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Transport(message)) => {
            assert!(message.contains(&uri), "адрес сервиса в тексте: {message}");
        }
        other => panic!("ожидался Transport, получено: {other:?}"),
    }
}

#[tokio::test]
async fn request_body_has_no_api_key_and_carries_history() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat"))
        .and(body_json_schema::<serde_json::Value>)
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&server)
        .await;

    let mut settings = cloud_settings();
    settings.reasoning = agentcore::config::ReasoningMode::StepByStep;
    settings.custom_response_mode = true;
    settings.response_format = ResponseFormat {
        description: Some("списком".into()),
        max_length: Some(500),
        stop: None,
        stop_instruction: None,
    };
    settings.sampling = SamplingParams {
        temperature: Some(0.4),
        ..SamplingParams::default()
    };

    agent(&server, "token")
        .ask(&history(), &settings)
        .await
        .expect("ответ сервиса");

    let requests = server.received_requests().await.expect("запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело запроса");

    assert!(
        !body.to_string().contains("api_key"),
        "поле api_key не отправляется: {body}"
    );
    let messages = body["messages"].as_array().expect("messages");
    // Ролей, кроме user и assistant, контракт `/v1` не знает: системный
    // промпт собирает сервис из настроек.
    assert_eq!(messages.len(), 1, "история уходит как есть: {body}");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "привет");
    assert_eq!(body["settings"]["model"], "deepseek-chat");
    assert_eq!(body["settings"]["provider"], "cloud");
    assert_eq!(body["settings"]["temperature"], 0.4);
    assert_eq!(body["settings"]["response_format"]["max_length"], 500);
    assert_eq!(body["settings"]["reasoning"], "step-by-step");
}

#[tokio::test]
async fn authorization_header_follows_token() {
    let with_token = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat"))
        .and(header("authorization", "Bearer secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&with_token)
        .await;
    agent(&with_token, "secret")
        .ask(&history(), &cloud_settings())
        .await
        .expect("запрос с токеном");

    let without_token = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&without_token)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
        .mount(&without_token)
        .await;
    agent(&without_token, "")
        .ask(&history(), &cloud_settings())
        .await
        .expect("запрос без токена");

    let requests = without_token.received_requests().await.expect("запросы");
    assert!(
        requests[0].headers.get("authorization").is_none(),
        "при пустом токене заголовок не отправляется"
    );
}

#[tokio::test]
async fn models_come_from_service() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "models": ["a", "b"] })),
        )
        .mount(&server)
        .await;

    let models = list_models(&server.uri(), "token").await.expect("модели");
    assert_eq!(models, vec!["a".to_string(), "b".to_string()]);
}

#[tokio::test]
async fn models_fail_when_service_is_down() {
    let uri = UNREACHABLE_URL.to_string();

    let err = list_models(&uri, "").await.expect_err("ожидалась ошибка");
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Transport(message)) => {
            assert!(message.contains(&uri), "адрес сервиса в тексте: {message}");
        }
        other => panic!("ожидался Transport, получено: {other:?}"),
    }
}
