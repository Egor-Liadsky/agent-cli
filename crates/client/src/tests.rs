//! Тесты `ServerAgent` против замоканного сервиса.

use super::*;
use agentcore::config::{ContextStrategy, Provider, ResponseFormat, SamplingParams};
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

fn cloud_settings_with_context_limit(max_context_tokens: u32) -> ChatSettings {
    ChatSettings {
        max_context_tokens: Some(max_context_tokens),
        ..cloud_settings()
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
async fn context_block_is_parsed_with_only_meaningful_fields() {
    let server = MockServer::start().await;
    let mut body = success_body();
    body["context"] = json!({
        "strategy": "sliding_window",
        "sent_messages": 6,
        "dropped_messages": 14
    });
    mount_chat(&server, 200, body).await;

    let reply = agent(&server, "token")
        .ask(&history(), &cloud_settings())
        .await
        .expect("ответ сервиса");

    let context = reply.context.expect("блок наблюдаемости стратегии");
    assert_eq!(context.strategy, Some(ContextStrategy::SlidingWindow));
    assert_eq!(context.sent_messages, Some(6));
    assert_eq!(context.dropped_messages, Some(14));
    // Поля чужой стратегии не заявлены сервисом и разбираются как None.
    assert_eq!(context.facts_applied, None);
    assert_eq!(context.branch_id, None);
}

#[tokio::test]
async fn context_strategy_override_is_sent_only_when_set() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;

    agent(&server, "token")
        .ask(&history(), &cloud_settings())
        .await
        .expect("ответ сервиса без переопределения стратегии");
    agent(&server, "token")
        .ask(
            &history(),
            &ChatSettings {
                context_strategy: Some(ContextStrategy::Facts),
                context_window_messages: Some(6),
                ..cloud_settings()
            },
        )
        .await
        .expect("ответ сервиса с переопределением стратегии");

    let requests = server.received_requests().await.expect("запросы");
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap_or(serde_json::Value::Null))
        .collect();
    assert!(bodies[0]["settings"]["context_strategy"].is_null());
    assert!(bodies[0]["settings"]["context_window_messages"].is_null());
    assert_eq!(bodies[1]["settings"]["context_strategy"], "facts");
    assert_eq!(bodies[1]["settings"]["context_window_messages"], 6);
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
    // Асимметрия с ChatSettingsUpdate (chats.rs) намеренная: POST /v1/chat
    // опускает незаданный лимит, чтобы не снять сохранённый на сервисе,
    // а POST /v1/chats и PATCH /v1/chats/{id} шлют его всегда, включая null
    // (specs/chat-context-limit, «Отправка лимита клиентом»;
    // chats/tests.rs::update_sends_max_context_tokens_as_null_when_absent_and_number_when_set).
    assert!(
        body["settings"].get("max_context_tokens").is_none(),
        "лимит не задан — поле не отправляется: {body}"
    );
}

#[tokio::test]
async fn max_context_tokens_is_sent_when_configured() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;

    agent(&server, "token")
        .ask(&history(), &cloud_settings_with_context_limit(4000))
        .await
        .expect("ответ сервиса");

    let requests = server.received_requests().await.expect("запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело запроса");
    assert_eq!(body["settings"]["max_context_tokens"], 4000);
}

#[tokio::test]
async fn max_context_tokens_is_sent_in_ask_in_chat_when_configured() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;

    agent(&server, "token")
        .ask_in_chat("chat-1", "привет", &cloud_settings_with_context_limit(4000), &[])
        .await
        .expect("ответ сервиса");

    let requests = server.received_requests().await.expect("запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело запроса");
    assert_eq!(body["settings"]["max_context_tokens"], 4000);
}

#[tokio::test]
async fn summary_settings_are_omitted_when_not_configured_and_sent_when_set() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;

    agent(&server, "token")
        .ask(&history(), &cloud_settings())
        .await
        .expect("ответ сервиса");
    let requests = server.received_requests().await.expect("запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело запроса");
    assert!(body["settings"].get("summary_enabled").is_none());
    assert!(body["settings"].get("summary_keep_messages").is_none());
    assert!(body["settings"].get("summary_step_messages").is_none());

    let settings = ChatSettings {
        summary_enabled: Some(true),
        summary_keep_messages: Some(20),
        summary_step_messages: Some(10),
        ..cloud_settings()
    };
    agent(&server, "token")
        .ask(&history(), &settings)
        .await
        .expect("ответ сервиса");
    let requests = server.received_requests().await.expect("запросы");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[1].body).expect("тело запроса");
    assert_eq!(body["settings"]["summary_enabled"], true);
    assert_eq!(body["settings"]["summary_keep_messages"], 20);
    assert_eq!(body["settings"]["summary_step_messages"], 10);
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

// --- Диалог в чате сервиса (5.1) ---

#[tokio::test]
async fn ask_in_chat_sends_chat_id_and_prompt_only() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;

    let reply = agent(&server, "token")
        .ask_in_chat("chat-1", "привет", &cloud_settings(), &[])
        .await
        .expect("ответ сервиса");
    assert_eq!(reply.content, "ответ");

    let requests = server.received_requests().await.expect("записанные запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело");
    assert_eq!(body["chat_id"], "chat-1");
    assert_eq!(body["prompt"], "привет");
    assert!(
        body.get("messages").is_none(),
        "вместе с chat_id история не отправляется: {body}"
    );
    assert_eq!(body["settings"]["provider"], "cloud");
}

#[tokio::test]
async fn ask_without_chat_id_still_sends_history() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;

    agent(&server, "token")
        .ask(&history(), &cloud_settings())
        .await
        .expect("ответ сервиса");

    let requests = server.received_requests().await.expect("записанные запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело");
    assert!(body.get("chat_id").is_none(), "разовый запрос без чата: {body}");
    assert_eq!(body["messages"][0]["content"], "привет");
}

// --- Вызов инструментов ---

fn git_status_spec() -> ToolSpec {
    ToolSpec {
        name: "git_status".into(),
        description: Some("Shows the working tree status".into()),
        parameters: json!({ "type": "object", "properties": {} }),
    }
}

fn tool_calls_body() -> serde_json::Value {
    let mut body = success_body();
    body["content"] = json!("");
    body["tool_calls"] = json!([{ "id": "call_0", "name": "git_status", "arguments": {} }]);
    body
}

#[tokio::test]
async fn ask_in_chat_sends_tools_and_parses_tool_calls() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, tool_calls_body()).await;

    let reply = agent(&server, "token")
        .ask_in_chat("chat-1", "статус?", &cloud_settings(), &[git_status_spec()])
        .await
        .expect("ответ сервиса");
    assert_eq!(reply.content, "");
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(reply.tool_calls[0].id, "call_0");
    assert_eq!(reply.tool_calls[0].name, "git_status");

    let requests = server.received_requests().await.expect("записанные запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело");
    assert_eq!(body["tools"][0]["name"], "git_status");
    assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    assert!(body.get("tool_results").is_none());
}

#[tokio::test]
async fn continue_in_chat_sends_tool_results_instead_of_prompt() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;

    let results = [Message::tool_result("call_0", "git_status", "clean")];
    let reply = agent(&server, "token")
        .continue_in_chat("chat-1", &results, &cloud_settings(), &[git_status_spec()])
        .await
        .expect("ответ сервиса");
    assert!(reply.tool_calls.is_empty(), "старый ответ без поля — окончательный");

    let requests = server.received_requests().await.expect("записанные запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело");
    assert_eq!(body["chat_id"], "chat-1");
    assert!(body.get("prompt").is_none(), "тело: {body}");
    assert_eq!(
        body["tool_results"],
        json!([{ "tool_call_id": "call_0", "name": "git_status", "content": "clean" }])
    );
    assert_eq!(body["tools"][0]["name"], "git_status");
}

#[tokio::test]
async fn request_without_tools_has_no_tool_fields() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;
    agent(&server, "token")
        .ask(&history(), &cloud_settings())
        .await
        .expect("ответ сервиса");
    let requests = server.received_requests().await.expect("записанные запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело");
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_results").is_none());
    assert!(body["messages"][0].get("tool_calls").is_none());
}

#[tokio::test]
async fn history_with_tool_messages_is_sent_with_links() {
    let server = MockServer::start().await;
    mount_chat(&server, 200, success_body()).await;
    let history = vec![
        Message::user("статус?"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "call_0".into(),
                name: "git_status".into(),
                arguments: json!({}),
            }],
        ),
        Message::tool_result("call_0", "git_status", "clean"),
    ];
    agent(&server, "token")
        .ask_with_tools(&history, &cloud_settings(), &[git_status_spec()])
        .await
        .expect("ответ сервиса");
    let requests = server.received_requests().await.expect("записанные запросы");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("тело");
    assert_eq!(body["messages"][1]["tool_calls"][0]["id"], "call_0");
    assert_eq!(body["messages"][2]["role"], "tool");
    assert_eq!(body["messages"][2]["tool_call_id"], "call_0");
    assert_eq!(body["messages"][2]["tool_name"], "git_status");
}

#[tokio::test]
async fn tools_unsupported_code_is_typed() {
    let err = ask_error(400, error_body("tools_unsupported", "модель не поддерживает вызов инструментов")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::ToolsUnsupported { request_id, .. }) => {
            assert_eq!(request_id.as_deref(), Some("req-1"));
        }
        other => panic!("ожидался ToolsUnsupported, получено: {other:?}"),
    }
    // Прочие 400 остаются неверным запросом.
    let err = ask_error(400, error_body("tools_invalid", "плохое имя")).await;
    assert!(matches!(
        err.downcast_ref::<AgentError>(),
        Some(AgentError::InvalidRequest { .. })
    ));
}
