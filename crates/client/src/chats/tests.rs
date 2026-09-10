//! Тесты `ChatsClient` против замоканного сервиса.

use super::*;
use agentcore::config::{ResponseFormat, SamplingParams};
use serde_json::json;
use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn chats(server: &MockServer, token: &str) -> ChatsClient {
    ChatsClient::new(server.uri(), token, Arc::new(ExchangeLog::disabled()))
}

fn chat_body(id: &str, title: &str) -> serde_json::Value {
    json!({
        "id": id,
        "title": title,
        "settings": { "provider": "cloud", "model": "model-a" },
        "created_at": 1000,
        "updated_at": 2000,
        "message_count": 0
    })
}

fn error_body(code: &str, message: &str) -> serde_json::Value {
    json!({ "error": { "code": code, "message": message, "request_id": "req-1" } })
}

/// Тела запросов, дошедшие до мока: `wiremock` хранит их у сервера.
async fn received_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .expect("записанные запросы")
        .iter()
        .map(|request: &Request| {
            serde_json::from_slice(&request.body).unwrap_or(serde_json::Value::Null)
        })
        .collect()
}

// --- 2.1 Аутентификация ---

#[tokio::test]
async fn request_with_token_carries_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/chats"))
        .and(header("authorization", "Bearer token-a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "chats": [], "next_cursor": null })))
        .mount(&server)
        .await;

    chats(&server, "token-a").list().await.expect("список чатов");
}

#[tokio::test]
async fn request_without_token_has_no_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/chats"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/chats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "chats": [], "next_cursor": null })))
        .mount(&server)
        .await;

    // Пустой токен: сервис с выключенной аутентификацией принимает запрос
    // без заголовка, и добавлять его нельзя.
    chats(&server, "   ").list().await.expect("список чатов");
}

// --- 2.2 Список чатов постранично ---

#[tokio::test]
async fn list_reads_all_pages_in_service_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/chats"))
        .and(query_param("cursor", "next-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chats": [chat_body("chat-3", "Третий")],
            "next_cursor": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/chats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chats": [chat_body("chat-1", "Первый"), chat_body("chat-2", "Второй")],
            "next_cursor": "next-1"
        })))
        .mount(&server)
        .await;

    let list = chats(&server, "token-a").list().await.expect("список чатов");

    assert_eq!(
        list.iter().map(|chat| chat.id.as_str()).collect::<Vec<_>>(),
        vec!["chat-1", "chat-2", "chat-3"]
    );
    assert_eq!(list[0].title, "Первый");
    assert_eq!(list[0].settings.model.as_deref(), Some("model-a"));
}

// --- 2.3 История чата постранично ---

#[tokio::test]
async fn load_reads_all_message_pages_in_seq_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/chats/chat-1"))
        .and(query_param("after", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chat-1",
            "title": "Чат",
            "settings": { "provider": "ollama", "model": "llama3" },
            "created_at": 1000,
            "updated_at": 2000,
            "message_count": 3,
            "messages": [
                { "seq": 3, "role": "user", "content": "третье", "created_at": 1003 }
            ],
            "next_after": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/chats/chat-1"))
        .and(query_param("after", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chat-1",
            "title": "Чат",
            "settings": { "provider": "ollama", "model": "llama3" },
            "created_at": 1000,
            "updated_at": 2000,
            "message_count": 3,
            "messages": [
                { "seq": 1, "role": "user", "content": "первое", "created_at": 1001 },
                {
                    "seq": 2,
                    "role": "assistant",
                    "content": "второе",
                    "created_at": 1002,
                    "reasoning": "рассуждение",
                    "model": "llama3",
                    "usage": { "prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18 },
                    "timing": { "duration_ms": 120, "sent_at": 1000, "received_at": 1002 }
                }
            ],
            "next_after": 2
        })))
        .mount(&server)
        .await;

    let history = chats(&server, "token-a").load("chat-1").await.expect("история");

    assert_eq!(history.chat.id, "chat-1");
    assert_eq!(history.chat.settings.provider, Provider::Ollama);
    assert_eq!(
        history.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let second = &history.messages[1].message;
    assert!(matches!(second.role, Role::Assistant));
    assert_eq!(second.reasoning.as_deref(), Some("рассуждение"));
    let meta = second.meta.as_ref().expect("телеметрия ответа модели");
    assert_eq!(meta.total_tokens, Some(18));
    assert_eq!(meta.duration_ms, Some(120));
    assert_eq!(meta.model.as_deref(), Some("llama3"));
    // У реплики пользователя телеметрии нет: пустой объект не создаётся.
    assert!(history.messages[0].message.meta.is_none());
}

// --- 2.4 Создание, изменение и удаление ---

fn settings_with(provider: Provider, model: &str) -> ChatSettings {
    ChatSettings {
        provider,
        model: Some(model.to_string()),
        reasoning: ReasoningMode::StepByStep,
        thinking: ThinkingMode::Enabled,
        experts: vec!["аналитик".to_string()],
        custom_response_mode: true,
        response_format: ResponseFormat {
            description: Some("только JSON".to_string()),
            max_length: Some(500),
            ..ResponseFormat::default()
        },
        sampling: SamplingParams {
            temperature: Some(0.3),
            ..SamplingParams::default()
        },
    }
}

#[tokio::test]
async fn create_sends_title_and_settings() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chats"))
        .respond_with(ResponseTemplate::new(201).set_body_json(chat_body("chat-1", "Новый чат")))
        .mount(&server)
        .await;

    let chat = chats(&server, "token-a")
        .create(Some("  Мой чат  "), &settings_with(Provider::Ollama, "llama3"))
        .await
        .expect("созданный чат");

    assert_eq!(chat.id, "chat-1");
    let body = received_bodies(&server).await.remove(0);
    assert_eq!(body["title"], "Мой чат");
    assert_eq!(body["settings"]["provider"], "ollama");
    assert_eq!(body["settings"]["model"], "llama3");
    assert_eq!(body["settings"]["reasoning"], "step-by-step");
    assert_eq!(body["settings"]["thinking"], "enabled");
    assert_eq!(body["settings"]["experts"], json!(["аналитик"]));
    assert_eq!(body["settings"]["custom_response_mode"], true);
    assert_eq!(body["settings"]["response_format"]["max_length"], 500);
    let temperature = body["settings"]["temperature"].as_f64().expect("температура");
    assert!((temperature - 0.3).abs() < 1e-6, "температура искажена: {temperature}");
}

#[tokio::test]
async fn create_without_title_omits_field() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chats"))
        .respond_with(ResponseTemplate::new(201).set_body_json(chat_body("chat-1", "Новый чат")))
        .mount(&server)
        .await;

    chats(&server, "token-a")
        .create(None, &ChatSettings::default())
        .await
        .expect("созданный чат");

    let body = received_bodies(&server).await.remove(0);
    assert!(body.get("title").is_none(), "заголовок не должен отправляться: {body}");
}

#[tokio::test]
async fn update_sends_only_given_fields_and_nulls_cleared_sampling() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/v1/chats/chat-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_body("chat-1", "Переименован")))
        .mount(&server)
        .await;

    let client = chats(&server, "token-a");
    let renamed = client
        .update("chat-1", Some("Переименован"), None)
        .await
        .expect("переименование");
    assert_eq!(renamed.title, "Переименован");

    // Настройки без температуры: она уходит как null и снимается сервисом.
    client
        .update("chat-1", None, Some(&ChatSettings::default()))
        .await
        .expect("изменение настроек");

    let bodies = received_bodies(&server).await;
    assert_eq!(bodies[0]["title"], "Переименован");
    assert!(bodies[0].get("settings").is_none());
    assert!(bodies[1].get("title").is_none());
    assert!(
        bodies[1]["settings"]["temperature"].is_null(),
        "сброс параметра должен уходить явным null: {}",
        bodies[1]
    );
    assert_eq!(bodies[1]["settings"]["custom_response_mode"], false);
}

#[tokio::test]
async fn delete_accepts_empty_response_body() {
    let server = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/v1/chats/chat-1"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    chats(&server, "token-a").delete("chat-1").await.expect("удаление");
}

// --- 2.5 Дозапись обмена ---

#[tokio::test]
async fn append_sends_both_replies_in_one_request_with_telemetry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chats/chat-1/messages"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "request_id": "req-1",
            "chat_id": "chat-1",
            "seqs": [1, 2]
        })))
        .mount(&server)
        .await;

    let assistant = Message {
        role: Role::Assistant,
        content: "ответ".to_string(),
        reasoning: Some("рассуждение".to_string()),
        meta: Some(MessageMeta {
            completion_tokens: Some(7),
            total_tokens: Some(18),
            duration_ms: Some(120),
            model: Some("llama3".to_string()),
            ..MessageMeta::default()
        }),
    };
    let seqs = chats(&server, "token-a")
        .append("chat-1", &[Message::user("вопрос"), assistant])
        .await
        .expect("дозапись");

    assert_eq!(seqs, vec![1, 2]);
    let bodies = received_bodies(&server).await;
    assert_eq!(bodies.len(), 1, "обе реплики уходят одним запросом");
    let messages = bodies[0]["messages"].as_array().expect("сообщения");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "вопрос");
    assert!(
        messages[0].get("usage").is_none(),
        "у реплики пользователя нет счётчиков токенов: {}",
        messages[0]
    );
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["reasoning"], "рассуждение");
    assert_eq!(messages[1]["model"], "llama3");
    assert_eq!(messages[1]["usage"]["total_tokens"], 18);
    assert_eq!(messages[1]["timing"]["duration_ms"], 120);
    assert!(
        !bodies[0].to_string().contains("api_key"),
        "поле api_key не должно уходить в теле: {}",
        bodies[0]
    );
}

// --- 2.6 Ошибки различимы по причине ---

async fn list_error(status: u16, body: serde_json::Value) -> anyhow::Error {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/chats"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("x-request-id", "req-header")
                .set_body_json(body),
        )
        .mount(&server)
        .await;

    chats(&server, "token-a")
        .with_unauthorized_hint("выполните: agentcli config set-token")
        .list()
        .await
        .expect_err("ожидалась ошибка сервиса")
}

#[tokio::test]
async fn unauthorized_carries_hint_and_request_id() {
    let err = list_error(401, error_body("unauthorized", "нужен токен")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Unauthorized { hint, request_id }) => {
            assert_eq!(hint.as_deref(), Some("выполните: agentcli config set-token"));
            assert_eq!(request_id.as_deref(), Some("req-1"));
        }
        other => panic!("ожидался Unauthorized, получено: {other:?}"),
    }
    assert!(format!("{err}").contains("req-1"), "идентификатор запроса не виден");
}

#[tokio::test]
async fn invalid_request_is_distinguished() {
    let err = list_error(400, error_body("invalid_request", "плохой курсор")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::InvalidRequest { message, .. }) => {
            assert!(message.contains("курсор"), "текст причины потерян: {message}");
        }
        other => panic!("ожидался InvalidRequest, получено: {other:?}"),
    }
}

#[tokio::test]
async fn missing_chat_is_reported_as_404() {
    let err = list_error(404, error_body("chat_not_found", "чат не найден")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Provider { status, message, .. }) => {
            assert_eq!(*status, 404);
            assert!(message.contains("не найден"), "текст причины потерян: {message}");
        }
        other => panic!("ожидался Provider со статусом 404, получено: {other:?}"),
    }
}

#[tokio::test]
async fn rate_limited_is_distinguished() {
    let err = list_error(429, error_body("rate_limited", "слишком много запросов")).await;
    assert!(
        matches!(err.downcast_ref::<AgentError>(), Some(AgentError::RateLimited { .. })),
        "ожидался RateLimited"
    );
}

#[tokio::test]
async fn provider_failure_keeps_status() {
    let err = list_error(502, error_body("provider_error", "провайдер отказал")).await;
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Provider { status, .. }) => assert_eq!(*status, 502),
        other => panic!("ожидался Provider, получено: {other:?}"),
    }
}

#[tokio::test]
async fn request_id_falls_back_to_header() {
    let err = list_error(404, json!({ "error": { "code": "chat_not_found", "message": "нет чата" } })).await;
    let text = format!("{err}");
    assert!(text.contains("req-header"), "идентификатор из заголовка потерян: {text}");
}

#[tokio::test]
async fn unreachable_service_is_transport_error_with_address() {
    // Порт 1 на localhost не слушает никто: соединение не устанавливается.
    let client = ChatsClient::new("http://127.0.0.1:1", "token-a", Arc::new(ExchangeLog::disabled()));
    let err = client.list().await.expect_err("ожидалась ошибка транспорта");
    match err.downcast_ref::<AgentError>() {
        Some(AgentError::Transport(message)) => {
            assert!(
                message.contains("127.0.0.1:1"),
                "адрес сервиса не назван: {message}"
            );
        }
        other => panic!("ожидался Transport, получено: {other:?}"),
    }
}
