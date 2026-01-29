//! Integration tests for the embedded chat UI and WebSocket handshake.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use moltis_gateway::auth;
use moltis_gateway::chat::{LiveChatService, LiveModelService};
use moltis_gateway::methods::MethodRegistry;
use moltis_gateway::server::build_gateway_app;
use moltis_gateway::services::GatewayServices;
use moltis_gateway::state::GatewayState;

use moltis_agents::providers::ProviderRegistry;

/// Spin up a test gateway on an ephemeral port, return the bound address.
async fn start_test_server() -> SocketAddr {
    let resolved_auth = auth::resolve_auth(None, None);
    let services = GatewayServices::noop();
    let state = GatewayState::new(resolved_auth, services);
    let methods = Arc::new(MethodRegistry::new());
    let app = build_gateway_app(state, methods);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

#[tokio::test]
async fn root_serves_chat_ui_html() {
    let addr = start_test_server().await;
    let resp = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<title>moltis</title>"));
    assert!(body.contains("id=\"chatInput\""));
    assert!(body.contains("Method Explorer"));
}

#[tokio::test]
async fn health_endpoint_returns_json() {
    let addr = start_test_server().await;
    let resp = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["protocol"], 3);
}

#[tokio::test]
async fn ws_handshake_returns_hello_ok() {
    let addr = start_test_server().await;
    let (mut ws, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect failed");

    // Send connect handshake.
    let connect_frame = serde_json::json!({
        "type": "req",
        "id": "test-1",
        "method": "connect",
        "params": {
            "minProtocol": 3,
            "maxProtocol": 3,
            "client": {
                "id": "test-client",
                "version": "0.0.1",
                "platform": "test",
                "mode": "operator"
            }
        }
    });
    ws.send(Message::Text(connect_frame.to_string().into()))
        .await
        .unwrap();

    // Read the response — should be a res frame wrapping hello-ok.
    let msg = ws.next().await.unwrap().unwrap();
    let frame: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(frame["type"], "res");
    assert_eq!(frame["id"], "test-1");
    assert_eq!(frame["ok"], true);
    assert_eq!(frame["payload"]["type"], "hello-ok");
    assert_eq!(frame["payload"]["protocol"], 3);
    assert!(frame["payload"]["server"]["version"].is_string());
    assert!(frame["payload"]["features"]["methods"].is_array());

    ws.close(None).await.ok();
}

#[tokio::test]
async fn ws_health_method_after_handshake() {
    let addr = start_test_server().await;
    let (mut ws, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect failed");

    // Handshake first.
    let connect_frame = serde_json::json!({
        "type": "req",
        "id": "hs-1",
        "method": "connect",
        "params": {
            "minProtocol": 3,
            "maxProtocol": 3,
            "client": {
                "id": "test-client-2",
                "version": "0.0.1",
                "platform": "test",
                "mode": "operator"
            }
        }
    });
    ws.send(Message::Text(connect_frame.to_string().into()))
        .await
        .unwrap();
    // Consume hello-ok.
    let _ = ws.next().await.unwrap().unwrap();

    // Call health method via RPC.
    let health_req = serde_json::json!({
        "type": "req",
        "id": "h-1",
        "method": "health"
    });
    ws.send(Message::Text(health_req.to_string().into()))
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let frame: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(frame["type"], "res");
    assert_eq!(frame["id"], "h-1");
    assert_eq!(frame["ok"], true);
    assert_eq!(frame["payload"]["status"], "ok");

    ws.close(None).await.ok();
}

#[tokio::test]
async fn ws_system_presence_shows_connected_client() {
    let addr = start_test_server().await;
    let (mut ws, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect failed");

    // Handshake.
    let connect_frame = serde_json::json!({
        "type": "req",
        "id": "hs-2",
        "method": "connect",
        "params": {
            "minProtocol": 3,
            "maxProtocol": 3,
            "client": {
                "id": "presence-test",
                "version": "0.0.1",
                "platform": "test",
                "mode": "operator"
            }
        }
    });
    ws.send(Message::Text(connect_frame.to_string().into()))
        .await
        .unwrap();
    let _ = ws.next().await.unwrap().unwrap();

    // Call system-presence.
    let req = serde_json::json!({
        "type": "req",
        "id": "sp-1",
        "method": "system-presence"
    });
    ws.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();

    let msg = ws.next().await.unwrap().unwrap();
    let frame: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(frame["type"], "res");
    assert_eq!(frame["ok"], true);
    // Should have at least one connected client (ourselves).
    let clients = frame["payload"]["clients"].as_array().unwrap();
    assert!(!clients.is_empty());
    let us = clients
        .iter()
        .find(|c| c["clientId"] == "presence-test")
        .expect("our client should appear in presence");
    assert_eq!(us["platform"], "test");

    ws.close(None).await.ok();
}

/// Reproduce the full `start_gateway` init sequence (with provider wiring)
/// inside an async runtime. This catches panics like "Cannot block the current
/// thread from within a runtime" that only surface at run time.
#[tokio::test]
async fn gateway_startup_with_llm_wiring_does_not_block() {
    let resolved_auth = auth::resolve_auth(None, None);
    let registry = Arc::new(ProviderRegistry::from_env());

    let mut services = GatewayServices::noop();
    if !registry.is_empty() {
        services = services.with_model(Arc::new(LiveModelService::new(Arc::clone(&registry))));
    }

    let state = GatewayState::new(resolved_auth, services);

    // This is the call that used to panic with blocking_write inside async.
    if !registry.is_empty() {
        state
            .set_chat(Arc::new(LiveChatService::new(
                Arc::clone(&registry),
                Arc::clone(&state),
            )))
            .await;
    }

    // Even without real API keys the override path must work.
    // Force it with an empty registry to exercise set_chat unconditionally.
    let resolved_auth2 = auth::resolve_auth(None, None);
    let registry2 = Arc::new(ProviderRegistry::from_env());
    let state2 = GatewayState::new(resolved_auth2, GatewayServices::noop());
    state2
        .set_chat(Arc::new(LiveChatService::new(
            Arc::clone(&registry2),
            Arc::clone(&state2),
        )))
        .await;

    // Verify chat override is active — chat.send should return an error about
    // missing providers rather than "chat not configured" (the noop response).
    let chat = state2.chat().await;
    let result = chat.send(serde_json::json!({ "text": "hello" })).await;
    match result {
        Err(e) => assert!(
            e.contains("no LLM providers configured"),
            "expected provider error, got: {e}"
        ),
        Ok(_) => panic!("expected error from LiveChatService without providers"),
    }
}
