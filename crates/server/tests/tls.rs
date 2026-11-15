//! 测试证书仅用于本地契约，包含公开测试私钥，禁止部署使用。
//! Fixtures contain public test private keys for local contracts only; never deploy them.

use linger_client::{Client, ClientTls, Options, Value};
use linger_server::{Config, Server};
use std::path::PathBuf;
use tokio::{net::TcpListener, sync::oneshot};

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
