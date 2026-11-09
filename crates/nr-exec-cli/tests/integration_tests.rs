//! Integration tests for nr-exec-cli executors.
//!
//! Tests: devin-cli (subprocess + ACP, see `devin_acp.rs`), grok-cli (Responses
//! API over the relay transport), perchai (plan gates + turn ticket + stream
//! transform). The grok/perch cases run against a local HTTP mock, so nothing
//! here needs network access.

#[cfg(test)]
mod executor_tests {
    use serde_json::json;

    #[test]
    fn test_grok_cli_user_turn_counting() {
        let input = json!([
            {"type": "message", "role": "user", "content": "Hello"},
            {"type": "message", "role": "assistant", "content": "Hi there"},
            {"type": "message", "role": "user", "content": "How are you?"},
        ]);
        assert_eq!(nr_exec_cli::grok_cli::count_user_turns(Some(&input)), 2);
    }

    #[test]
    fn test_grok_cli_reasoning_effort_normalization() {
        use nr_core::Credentials;
        use nr_exec_base::BaseHooks;

        let exec = nr_exec_cli::grok_cli::GrokCliExecutor::new();
        let cases = [
            ("max", "xhigh"),
            ("xhigh", "xhigh"),
            ("high", "high"),
            ("medium", "medium"),
            ("low", "low"),
            ("bogus", "high"),
        ];
        for (input, expected) in cases {
            let body = json!({ "reasoning_effort": input, "input": [] });
            let out = BaseHooks::transform_request(&exec, "grok-4.5", &body, true, &Credentials::new()).unwrap();
            assert_eq!(out["reasoning"]["effort"], json!(expected), "effort {input}");
        }
        // Models without effort support drop the field but keep the summary.
        let out = BaseHooks::transform_request(
            &exec,
            "grok-3",
            &json!({ "reasoning_effort": "high", "input": [] }),
            true,
            &Credentials::new(),
        )
        .unwrap();
        assert!(out["reasoning"].get("effort").is_none(), "{out}");
        assert_eq!(out["reasoning"]["summary"], json!("concise"));
    }

    #[test]
    fn test_grok_cli_model_effort_support() {
        // Only grok-4.5* models carry a reasoning effort.
        let exec = nr_exec_cli::grok_cli::GrokCliExecutor::new();
        use nr_core::Credentials;
        use nr_exec_base::BaseHooks;
        for (model, supports) in [("grok-4.5", true), ("grok-4.5-preview", true), ("grok-3", false)] {
            let out = BaseHooks::transform_request(
                &exec,
                model,
                &json!({ "reasoning_effort": "high", "input": [] }),
                true,
                &Credentials::new(),
            )
            .unwrap();
            assert_eq!(
                out["reasoning"].get("effort"),
                supports.then(|| json!("high")).as_ref(),
                "model {model}"
            );
        }
    }

    #[test]
    fn test_perchai_url_building() {
        use nr_core::Credentials;
        use nr_exec_base::BaseHooks;

        let exec = nr_exec_cli::perchai::PerchAiExecutor::new("perchai");
        let default = BaseHooks::build_url(&exec, "standard", true, 0, &Credentials::new()).unwrap();
        assert_eq!(default, "https://app.perchai.app/api/perch-terminal/model-call");

        let mut creds = Credentials::new();
        creds.set("providerSpecificData", json!({ "appUrl": "https://custom.perchai.app/" }));
        let custom = BaseHooks::build_url(&exec, "standard", true, 0, &creds).unwrap();
        assert_eq!(custom, "https://custom.perchai.app/api/perch-terminal/model-call");
    }

    #[test]
    fn test_perchai_plan_gate_detection() {
        let gate = r#"{"error": "Plan not selected", "errorCode": "tier_selection_required"}"#;
        assert!(nr_exec_cli::perchai::is_tier_gate(403, gate));
        assert!(!nr_exec_cli::perchai::is_tier_gate(401, gate));
        assert!(!nr_exec_cli::perchai::is_tier_gate(403, ""));
    }

    #[test]
    fn test_perchai_plan_gate_body_shape() {
        let body = nr_exec_cli::perchai::build_tier_gate_body(
            Some("pick a plan"),
            Some(&json!({ "id": "starter", "name": "Starter" })),
        );
        assert_eq!(body["errorCode"], json!("tier_selection_required"));
        assert_eq!(body["error"], json!("pick a plan"));
        // The plan rides in the generic `limit` envelope, like the JS body.
        assert_eq!(body["limit"]["plan"]["id"], json!("starter"));
        assert_eq!(body["limit"]["url"], json!("https://chat.perchai.app"));
    }
}

#[cfg(test)]
mod mock_upstream_tests {
    //! End-to-end executor tests over a local HTTP server.
    //!
    //! grok-cli is exercised through the existing `vercelRelayUrl` transport
    //! option: the executor builds the real upstream URL and headers, and
    //! `proxy_fetch` forwards the request to the relay with `x-relay-*` headers.
    //! perchai takes `appUrl` from the credential, so it points straight at the
    //! mock.

    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use nr_core::{Credentials, ExecRequest, Executor, Json};
    use nr_exec_base::BaseHooks;
    use serde_json::json;

    #[derive(Clone, Debug)]
    struct MockRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl MockRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        fn json(&self) -> Json {
            serde_json::from_str(&self.body).unwrap_or(Json::Null)
        }
    }

    struct MockServer {
        url: String,
        requests: Arc<Mutex<Vec<MockRequest>>>,
    }

    impl MockServer {
        fn requests(&self) -> Vec<MockRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn paths(&self) -> Vec<String> {
            self.requests().into_iter().map(|r| r.path).collect()
        }
    }

    /// Serve one request per connection with `handler`; `Connection: close` keeps
    /// reqwest from reusing a socket the mock will not read twice.
    fn start_mock<F>(handler: F) -> MockServer
    where
        F: Fn(&MockRequest) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().expect("mock addr");
        let requests: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let handler = Arc::new(handler);

        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { return };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let Some((head, body)) = read_request(&mut stream) else { continue };

                let mut lines = head.lines();
                let request_line = lines.next().unwrap_or_default();
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let headers: Vec<(String, String)> = lines
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect();

                let request = MockRequest { method, path, headers, body };
                recorded.lock().unwrap().push(request.clone());
                let (status, payload) = handler(&request);

                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    403 => "Forbidden",
                    404 => "Not Found",
                    _ => "Error",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });

        MockServer { url: format!("http://{addr}"), requests }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Option<(String, String)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos;
            }
        };

        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let content_length = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
            })
            .unwrap_or(0);

        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < content_length {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        Some((head, String::from_utf8_lossy(&body).into_owned()))
    }

    fn exec_request<'a>(
        model: &'a str,
        body: Json,
        stream: bool,
        credentials: &'a mut Credentials,
        log: &'a nr_core::Logger,
        proxy: nr_core::http::ProxyOptions,
    ) -> ExecRequest<'a> {
        ExecRequest {
            model,
            body,
            stream,
            credentials,
            provider_session_id: None,
            client_tool: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            log,
            proxy,
            provider_overrides: None,
        }
    }

    #[tokio::test]
    async fn grok_cli_reaches_the_upstream_with_its_fingerprint_headers() {
        let mock = start_mock(|_req| {
            (
                200,
                "event: response.completed\ndata: {\"type\":\"response.completed\"}\n\n".to_string(),
            )
        });

        let mut creds = Credentials::from_value(json!({
            "accessToken": "xai-token",
            "connectionId": "conn-42",
            "providerSpecificData": {
                "email": "dev@example.com",
                "vercelRelayUrl": mock.url.clone(),
            }
        }));
        let proxy = nr_core::http::ProxyOptions::from_credentials(&creds);
        let log = nr_core::Logger::new();
        let exec = nr_exec_cli::grok_cli::GrokCliExecutor::new();
        let req = exec_request(
            "grok-4.5",
            json!({
                "messages": [
                    { "role": "user", "content": "one" },
                    { "role": "assistant", "content": "two" },
                    { "role": "user", "content": "three" }
                ]
            }),
            true,
            &mut creds,
            &log,
            proxy,
        );

        let result = exec.execute(req).await.expect("grok-cli request");
        assert_eq!(result.response.status, 200);
        assert_eq!(result.url, BaseHooks::build_url(&exec, "grok-4.5", true, 0, &creds).unwrap());

        let seen = mock.requests().pop().expect("mock saw the request");
        assert_eq!(seen.method, "POST");
        // The relay carries the original destination, so the executor really
        // built the Responses endpoint.
        assert_eq!(seen.header("x-relay-target"), Some("https://cli-chat-proxy.grok.com"));
        assert_eq!(seen.header("x-relay-path"), Some("/v1/responses"));
        // Grok CLI fingerprint + per-turn headers.
        assert_eq!(seen.header("Authorization"), Some("Bearer xai-token"));
        assert_eq!(seen.header("x-grok-turn-idx"), Some("2"));
        assert_eq!(seen.header("x-grok-session-id"), Some("conn-42"));
        assert_eq!(seen.header("x-email"), Some("dev@example.com"));
        assert!(seen.header("x-grok-req-id").is_some());

        let body = seen.json();
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["input"][0]["role"], json!("user"));
        assert_eq!(body["input"][2]["content"], json!("three"));
        assert!(body.get("messages").is_none(), "{body}");
    }

    #[tokio::test]
    async fn perchai_plan_gate_short_circuits_the_model_call() {
        nr_exec_cli::perchai::clear_plan_cache();
        let mock = start_mock(|req| match req.path.as_str() {
            "/api/perch-terminal/cli-plan" => (
                403,
                r#"{"error":"Plan not selected","errorCode":"tier_selection_required"}"#.to_string(),
            ),
            other => (404, format!(r#"{{"error":"unexpected path {other}"}}"#)),
        });

        let mut creds = Credentials::from_value(json!({
            "accessToken": "tok-gate-test",
            "providerSpecificData": { "appUrl": mock.url.clone() }
        }));
        let proxy = nr_core::http::ProxyOptions::from_credentials(&creds);
        let log = nr_core::Logger::new();
        let exec = nr_exec_cli::perchai::PerchAiExecutor::new("perchai");
        let req = exec_request(
            "standard",
            json!({ "messages": [{ "role": "user", "content": "hi" }] }),
            false,
            &mut creds,
            &log,
            proxy,
        );

        let result = exec.execute(req).await.expect("perch request");
        assert_eq!(result.response.status, 403);
        let body = result.response.json_body().await.unwrap();
        assert_eq!(body["errorCode"], json!("tier_selection_required"));
        assert!(body["error"].as_str().unwrap().contains("pick"), "{body}");
        // The gate is answered locally: model-call is never reached.
        assert_eq!(mock.paths(), vec!["/api/perch-terminal/cli-plan".to_string()]);
    }

    #[tokio::test]
    async fn perchai_ticket_then_model_call_streams_openai_chunks() {
        nr_exec_cli::perchai::clear_plan_cache();
        let mock = start_mock(|req| match req.path.as_str() {
            "/api/perch-terminal/cli-plan" => (200, r#"{"ok":true,"plan":{"id":"pro"}}"#.to_string()),
            "/api/perch-terminal/turn-ticket" => (
                200,
                r#"{"ok":true,"ticket":"tk_1","runId":"run_1"}"#.to_string(),
            ),
            "/api/perch-terminal/model-call" => (
                200,
                concat!(
                    "data: {\"type\":\"answer_delta\",\"text\":\"Hel\"}\n\n",
                    "data: {\"type\":\"answer_delta\",\"text\":\"lo\"}\n\n",
                    "data: {\"type\":\"done\",\"ok\":true}\n\n",
                )
                .to_string(),
            ),
            other => (404, format!(r#"{{"error":"unexpected path {other}"}}"#)),
        });

        let mut creds = Credentials::from_value(json!({
            "accessToken": "tok-happy-path-test",
            "providerSpecificData": { "appUrl": mock.url.clone() }
        }));
        let proxy = nr_core::http::ProxyOptions::from_credentials(&creds);
        let log = nr_core::Logger::new();
        let exec = nr_exec_cli::perchai::PerchAiExecutor::new("perchai");
        let req = exec_request(
            "standard",
            json!({ "messages": [{ "role": "user", "content": "hi" }] }),
            true,
            &mut creds,
            &log,
            proxy,
        );

        let result = exec.execute(req).await.expect("perch request");
        assert_eq!(result.response.status, 200);
        let events = nr_core::http::collect_sse_data(result.response.body).await.unwrap();
        assert!(events.iter().any(|e| e.contains("\"content\":\"Hel\"")), "{events:?}");
        assert!(events.iter().any(|e| e.contains("\"content\":\"lo\"")), "{events:?}");
        assert!(events.iter().any(|e| e.contains("\"finish_reason\":\"stop\"")), "{events:?}");
        assert_eq!(events.last().map(String::as_str), Some("[DONE]"));

        let requests = mock.requests();
        assert_eq!(
            requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            vec![
                "/api/perch-terminal/cli-plan",
                "/api/perch-terminal/turn-ticket",
                "/api/perch-terminal/model-call"
            ]
        );
        let call = requests.last().unwrap();
        assert_eq!(call.header("x-perch-turn-ticket"), Some("tk_1"));
        assert_eq!(call.header("Authorization"), Some("Bearer tok-happy-path-test"));
        let body = call.json();
        assert_eq!(body["clientSurface"], json!("cli"));
        assert_eq!(body["runId"], json!("run_1"));
        // "standard" is a roost tier: routing uses roostModelChoice, not the
        // manual-model selector.
        assert_eq!(body["preferredModelId"], json!(null));
        assert_eq!(body["roostModelChoice"], json!("standard"));
    }
}
