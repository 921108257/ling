//! 测试证书仅用于本地契约，包含公开测试私钥，禁止部署使用。
//! Fixtures contain public test private keys for local contracts only; never deploy them.

use ling_client::{Client, ClientTls, Options, Value};
use ling_server::{Config, Server};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

/// 仓库内固定测试身份；Fixed repository test identity.
fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/tls")
        .join(name)
}
/// 服务端和客户端都校验身份，错误主机名与无证书客户端失败。
/// Both peers verify identity; wrong hostnames and missing client certificates fail.
#[tokio::test]
async fn verified_tls_and_mutual_authentication() {
    for mutual in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = Config::default();
        config.tls.enabled = true;
        config.tls.cert_file = fixture("identity.pem");
        config.tls.key_file = fixture("identity-key.pem");
        if mutual {
            config.tls.ca_file = fixture("ca.pem");
        }
        let server = Server::from_listener(listener, config).unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = stopped.await;
        }));
        let mut tls = ClientTls {
            server_name: "localhost".into(),
            ca_file: Some(fixture("ca.pem")),
            cert_file: None,
            key_file: None,
        };
        if mutual {
            assert!(
                Client::connect(
                    address,
                    None,
                    0,
                    Options {
                        tls: Some(tls.clone()),
                        ..Default::default()
                    }
                )
                .await
                .is_err()
            );
            tls.cert_file = Some(fixture("identity.pem"));
            tls.key_file = Some(fixture("identity-key.pem"));
        }
        let mut client = Client::connect(
            address,
            None,
            0,
            Options {
                tls: Some(tls.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            client.ping(None).await.unwrap(),
            Value::String("PONG".into())
        );
        let mut subscription = client.subscribe(vec!["*".into()], None).await.unwrap();
        subscription.ping().await.unwrap();
        subscription.close().await.unwrap();
        tls.server_name = "wrong.invalid".into();
        assert!(
            Client::connect(
                address,
                None,
                0,
                Options {
                    tls: Some(tls),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        assert!(
            Client::connect(
                address,
                None,
                0,
                Options {
                    connect_timeout: std::time::Duration::from_millis(50),
                    request_timeout: std::time::Duration::from_millis(50),
                    ..Default::default()
                }
            )
            .await
            .is_err()
        );
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}

/// 半开 TLS 在关闭期限内归还连接许可，超限连接不收到明文 ERROR，健康 TLS 连接继续工作。
/// Half-open TLS releases its permit within the shutdown timeout, excess connections receive no plaintext ERROR, and healthy TLS peers keep working.
#[tokio::test]
async fn half_open_tls_expires_without_blocking_healthy_connections() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = Config::default();
    config.server.max_connections = 2;
    config.server.shutdown_timeout_ms = 1000;
    config.events.enabled = false;
    config.tls.enabled = true;
    config.tls.cert_file = fixture("identity.pem");
    config.tls.key_file = fixture("identity-key.pem");
    let server = Server::from_listener(listener, config).unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let options = Options {
        tls: Some(ClientTls {
            server_name: "localhost".into(),
            ca_file: Some(fixture("ca.pem")),
            cert_file: None,
            key_file: None,
        }),
        ..Default::default()
    };
    let mut healthy = Client::connect(address, None, 0, options.clone())
        .await
        .unwrap();
    let mut half_open = TcpStream::connect(address).await.unwrap();
    half_open.write_all(&[0x16]).await.unwrap();
    assert_eq!(
        healthy.ping(None).await.unwrap(),
        Value::String("PONG".into())
    );
    let mut excess = TcpStream::connect(address).await.unwrap();
    let mut byte = [0];
    let rejected = tokio::time::timeout(Duration::from_millis(500), excess.read(&mut byte))
        .await
        .expect("TLS admission waited for another handshake");
    assert!(matches!(rejected, Ok(0) | Err(_)), "{rejected:?}");
    let expired = tokio::time::timeout(Duration::from_secs(3), half_open.read(&mut byte))
        .await
        .expect("half-open TLS retained its connection permit");
    assert!(matches!(expired, Ok(0) | Err(_)), "{expired:?}");
    let mut recovered = Client::connect(address, None, 0, options).await.unwrap();
    assert_eq!(
        recovered.ping(None).await.unwrap(),
        Value::String("PONG".into())
    );
    assert_eq!(
        healthy.ping(None).await.unwrap(),
        Value::String("PONG".into())
    );
    healthy.close().await.unwrap();
    recovered.close().await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}
