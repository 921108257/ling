use linger_client::{Client, Error, LocalError, Options, Value};
use linger_protocol::{Body, Frame, Limits, Request, read_frame, write_frame};
use linger_server::{Config, Server};
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

async fn mock() -> (std::net::SocketAddr, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    (listener.local_addr().unwrap(), listener)
}

async fn receive(stream: &mut TcpStream) -> Frame {
    read_frame(stream, Limits::default())
        .await
        .unwrap()
        .unwrap()
}
async fn reply(stream: &mut TcpStream, id: u64, value: Value) {
    write_frame(
        stream,
        &Frame {
            request_id: id,
            body: Body::Response(Ok(value)),
        },
        Limits::default(),
    )
    .await
    .unwrap();
}
async fn initialize(stream: &mut TcpStream) {
    let hello = receive(stream).await;
    assert_eq!(
        hello,
        Frame {
            request_id: 1,
            body: Body::Request(Request::new("HELLO", vec![Value::UInt(1)]))
        }
    );
    reply(
        stream,
        1,
        Value::Map(BTreeMap::from([
            ("protocol".into(), Value::UInt(1)),
            ("server_version".into(), Value::String("test".into())),
            ("events".into(), Value::Bool(false)),
            ("reliable_events".into(), Value::Bool(false)),
            ("stream_id".into(), Value::Null),
        ])),
    )
    .await;
    let selected = receive(stream).await;
    assert_eq!(selected.request_id, 2);
    let Body::Request(request) = selected.body else {
        panic!()
    };
    assert_eq!(request.command, "SELECT");
    reply(stream, 2, Value::Bool(true)).await;
}

#[tokio::test]
async fn real_service_auth_database_errors_and_close() {
    let mut config = Config::default();
    config.auth.enabled = true;
    config.auth.password = "secret".into();
    let (addr, listener) = mock().await;
    let server = Server::from_listener(listener, config).unwrap();
    let (stop, rx) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = rx.await;
    }));
    let wrong = Client::connect(addr, Some("wrong"), 0, Options::default())
        .await
        .err()
        .unwrap();
    assert!(matches!(wrong, Error::Server(ref e) if e.code == "ERR_AUTH_FAILED"));
    assert!(!wrong.outcome_unknown());
    assert!(!wrong.to_string().contains("wrong"));
    let mut client = Client::connect(addr, Some("secret"), 15, Options::default())
        .await
        .unwrap();
    assert_eq!(client.database(), Some(15));
    assert_eq!(
        client.ping(Some("中\n文")).await.unwrap(),
        Value::String("中\n文".into())
    );
    assert!(
        matches!(client.execute("SELECT", vec![Value::UInt(16)]).await, Err(Error::Server(e)) if e.code == "ERR_DB_OUT_OF_RANGE")
    );
    assert_eq!(client.database(), Some(15));
    client.select(0).await.unwrap();
    assert_eq!(client.database(), Some(0));
    let error = client
        .execute("GET", vec![Value::String("null".into())])
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Server(e) if e.code == "ERR_UNKNOWN_COMMAND"));
    client.close().await.unwrap();
    client.close().await.unwrap();
    assert!(client.is_closed());
    assert!(matches!(
        client.ping(None).await,
        Err(Error::Local {
            kind: LocalError::Closed,
            outcome_unknown: false
        })
    ));
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn id_matching_ignores_duplicates_and_heartbeats_without_reusing_ids() {
    let (addr, listener) = mock().await;
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        initialize(&mut stream).await;
        assert_eq!(receive(&mut stream).await.request_id, 3);
        write_frame(
            &mut stream,
            &Frame {
                request_id: 0,
                body: Body::Heartbeat,
            },
            Limits::default(),
        )
        .await
        .unwrap();
        reply(&mut stream, 2, Value::String("late".into())).await;
        reply(&mut stream, 3, Value::Null).await;
        assert_eq!(receive(&mut stream).await.request_id, 4);
        reply(&mut stream, 3, Value::String("duplicate".into())).await;
        reply(&mut stream, 4, Value::UInt(u64::MAX)).await;
    });
    let mut client = Client::connect(addr, None, 0, Options::default())
        .await
        .unwrap();
    assert_eq!(client.ping(None).await.unwrap(), Value::Null);
    assert_eq!(client.ping(None).await.unwrap(), Value::UInt(u64::MAX));
    task.await.unwrap();
}

#[tokio::test]
async fn rejects_unsent_response_id_and_never_reuses_connection() {
    let (addr, listener) = mock().await;
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        initialize(&mut stream).await;
        receive(&mut stream).await;
        reply(&mut stream, 4, Value::Bool(true)).await;
        assert!(
            read_frame(&mut stream, Limits::default())
                .await
                .unwrap()
                .is_none()
        );
    });
    let mut client = Client::connect(addr, None, 0, Options::default())
        .await
        .unwrap();
    let error = client.ping(None).await.unwrap_err();
    assert!(matches!(
        error,
        Error::Local {
            kind: LocalError::Protocol,
            ..
        }
    ));
    assert!(error.outcome_unknown());
    assert!(client.is_closed());
    task.await.unwrap();
}

#[tokio::test]
async fn sent_request_timeout_is_unknown_and_is_not_replayed() {
    let (addr, listener) = mock().await;
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        initialize(&mut stream).await;
        let request = receive(&mut stream).await;
        assert_eq!(request.request_id, 3);
        // The SDK must close this socket on timeout, with no replay or later QUIT.
        assert!(
            read_frame(&mut stream, Limits::default())
                .await
                .unwrap()
                .is_none()
        );
    });
    let options = Options {
        request_timeout: Duration::from_millis(100),
        ..Options::default()
    };
    let mut client = Client::connect(addr, None, 0, options).await.unwrap();
    let error = client
        .execute("INCR", vec![Value::String("counter".into())])
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        Error::Local {
            kind: LocalError::Timeout,
            ..
        }
    ));
    assert!(error.outcome_unknown());
    assert!(client.is_closed());
    task.await.unwrap();
}

#[tokio::test]
async fn disconnect_and_cancellation_close_socket() {
    for cancel in [false, true] {
        let (addr, listener) = mock().await;
        let (seen_tx, seen_rx) = oneshot::channel();
        let (finish_tx, finish_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            initialize(&mut stream).await;
            receive(&mut stream).await;
            seen_tx.send(()).unwrap();
            if cancel {
                assert!(
                    read_frame(&mut stream, Limits::default())
                        .await
                        .unwrap()
                        .is_none()
                );
            } else {
                drop(stream);
            }
            finish_tx.send(()).unwrap();
        });
        let mut client = Client::connect(addr, None, 0, Options::default())
            .await
            .unwrap();
        if cancel {
            let future = client.ping(None);
            tokio::pin!(future);
            tokio::select! { _ = &mut future => panic!("Unexpected reply"), _ = seen_rx => {} }
        } else {
            let error = client.ping(None).await.unwrap_err();
            assert!(error.outcome_unknown());
            assert!(matches!(
                error,
                Error::Local {
                    kind: LocalError::Disconnected,
                    ..
                }
            ));
        }
        assert!(client.is_closed());
        finish_rx.await.unwrap();
        task.await.unwrap();
    }
}

#[tokio::test]
async fn local_encoding_failure_does_not_send_or_consume_id() {
    let (addr, listener) = mock().await;
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        initialize(&mut stream).await;
        assert_eq!(receive(&mut stream).await.request_id, 3);
        reply(&mut stream, 3, Value::Bool(true)).await;
    });
    let mut client = Client::connect(addr, None, 0, Options::default())
        .await
        .unwrap();
    let error = client.execute("不合法", vec![]).await.unwrap_err();
    assert!(!error.outcome_unknown());
    assert!(!client.is_closed());
    assert_eq!(client.ping(None).await.unwrap(), Value::Bool(true));
    task.await.unwrap();
}
