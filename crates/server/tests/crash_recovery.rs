//! 强制终止真实服务进程后恢复事件与 ACK；不使用优雅关闭替代崩溃。
//! Recovers events and ACKs after forcibly killing a real server, rather than substituting graceful shutdown.

use ling_client::{Client, Options, Value};
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

/// 测试拥有的服务进程，失败时也终止回收；Test-owned server reaped even on test failures.
struct Process(Child);
impl Drop for Process {
    /// 不遗留后台服务；Leaves no background server.
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
/// 自动清理配置和 Journal；Cleans the configuration and Journal.
struct Directory(PathBuf);
impl Drop for Directory {
    /// 进程先退出后删除文件；Deletes files after the process exits.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// 启动真实二进制，并轮询确认完成恢复和监听；Starts the real binary and polls recovery/listening readiness.
async fn start(path: &std::path::Path, address: std::net::SocketAddr) -> Process {
    let child = Command::new(env!("CARGO_BIN_EXE_ling"))
        .arg("--config")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut process = Process(child);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            assert!(
                process.0.try_wait().unwrap().is_none(),
                "Server exited during recovery"
            );
            if let Ok(mut client) = Client::connect(address, None, 0, Options::default()).await {
                client.close().await.unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    process
}
/// 建立经过握手的普通连接；Creates an initialized ordinary connection.
async fn connect(address: std::net::SocketAddr) -> Client {
    Client::connect(address, None, 0, Options::default())
        .await
        .unwrap()
}

/// 强制终止后事件事务完整恢复，确认空洞保留。
/// Forced termination preserves complete event transactions and ACK holes.
#[tokio::test]
async fn kill_preserves_atomic_journal_and_ack_hole_without_restoring_values() {
    let dir = Directory(
        std::env::temp_dir().join(format!("linger-kill-{:032x}", rand::random::<u128>())),
    );
    fs::create_dir_all(&dir.0).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let path = dir.0.join("server.toml");
    fs::write(&path,format!(
        "[server]\nport={}\n[engine]\nshard_count=2\n[events]\nmode=\"reliable\"\n[event_journal]\ndir=\"journal\"\n",
        address.port())).unwrap();
    let mut process = start(&path, address).await;
    let mut sub = connect(address)
        .await
        .subscribe(vec!["*".into()], Some("kill-test"))
        .await
        .unwrap();
    let mut writer = connect(address).await;
    writer
        .execute(
            "MSET",
            (0..8)
                .flat_map(|n| [Value::String(format!("key{n}")), Value::UInt(n)])
                .collect(),
        )
        .await
        .unwrap();
    let mut expected = Vec::new();
    for _ in 0..8 {
        let id = sub.next_event().await.unwrap().id;
        assert!(!expected.contains(&id));
        expected.push(id);
    }
    // 在同一分片内留下较早的未确认事件，确保确实产生 ACK 空洞。
    // Leave an earlier event unacknowledged in the same shard to create a real ACK hole.
    let acked = expected
        .iter()
        .position(|id| id.sequence > 1)
        .map(|index| expected.swap_remove(index))
        .unwrap();
    assert!(
        expected
            .iter()
            .any(|id| id.db == acked.db && id.shard == acked.shard && id.sequence < acked.sequence)
    );
    sub.ack(acked).await.unwrap();
    process.0.kill().unwrap();
    process.0.wait().unwrap();
    drop(writer);
    drop(sub);
    // 检查一个 MSET 只形成一个含八事件的物理记录。
    // Verify that one MSET formed exactly one physical record with eight events.
    let bytes = fs::read(dir.0.join("journal/journal.lgj")).unwrap();
    let mut pos = 40;
    let mut transactions = Vec::new();
    while pos < bytes.len() {
        let n = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        if bytes[pos + 8] == 1 {
            transactions.push(u32::from_be_bytes(
                bytes[pos + 9..pos + 13].try_into().unwrap(),
            ));
        }
        pos += n + 16;
    }
    assert_eq!(transactions, vec![8]);
    drop(process);
    let mut process = start(&path, address).await;
    let mut writer = connect(address).await;
    assert_eq!(
        writer.execute("DBSIZE", vec![]).await.unwrap(),
        Value::UInt(0)
    );
    let mut sub = connect(address)
        .await
        .subscribe(vec!["*".into()], Some("kill-test"))
        .await
        .unwrap();
    // 跨分片不保证消费顺序；逐一核对未确认的原始 ID，拒绝缺失、重复或额外事件。
    // Cross-shard delivery is unordered; match every unacknowledged original ID exactly once.
    while !expected.is_empty() {
        let replay = sub.next_event().await.unwrap();
        assert_ne!(replay.id, acked);
        let index = expected
            .iter()
            .position(|id| *id == replay.id)
            .expect("Unexpected or duplicate replayed event");
        expected.swap_remove(index);
        sub.ack(replay.id).await.unwrap();
    }
    process.0.kill().unwrap();
    process.0.wait().unwrap();
}
