//! 三种 SDK 共享同一命令向量，防止类型与错误语义分叉。
//! All three SDKs share command vectors to prevent divergent types and error semantics.

use ling_client::{Client, Error, Options};
use ling_protocol::{Limits, decode_value};
use ling_server::{Config, Server};
use ling_types::Value;
use tokio::{net::TcpListener, sync::oneshot};

/// 黄金十六进制由文件独立提供；Golden hex is independently provided by the fixture.
fn bytes(text: &str) -> Vec<u8> {
    let hex = text.replace(' ', "");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}
/// 在真实服务端逐行执行相同命令及期望错误。
/// Executes identical commands and expected errors against a real server.
#[tokio::test]
async fn shared_command_contracts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = Server::from_listener(listener, Config::default()).unwrap();
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let mut client = Client::connect(address, None, 0, Options::default())
        .await
        .unwrap();
    for line in include_str!("../../../tests/fixtures/protocol/commands.tsv")
        .lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let fields: Vec<_> = line.split('|').map(str::trim).collect();
        let Value::List(args) = decode_value(&bytes(fields[2]), Limits::default()).unwrap() else {
            panic!()
        };
        let result = client.execute(fields[1], args).await;
        if fields[3].starts_with("ERR_") {
            let Err(Error::Server(error)) = result else {
                panic!("{}", fields[0])
            };
            assert_eq!(error.code, fields[3]);
        } else {
            assert_eq!(
                result.unwrap(),
                decode_value(&bytes(fields[3]), Limits::default()).unwrap()
            );
        }
    }
    client.close().await.unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}
