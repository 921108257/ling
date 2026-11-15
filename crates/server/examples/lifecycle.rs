//! 真实网络 TTL 验收，记录过期可见性、后台回收和到期事件的独立延迟。
//! Real-network TTL validation separates visibility, background reclamation, and expiry-event latency.

use linger_client::{Client, Options, Value};
use linger_types::EventKind;
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

/// 每次用不同 Key，避免旧身份被误认为本次到期。
/// Uses fresh keys so old lifecycle identities cannot be mistaken for this expiry.
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let address: SocketAddr = std::env::args().nth(1).ok_or("address required")?.parse()?;
    let mut client = Client::connect(address, None, 0, Options::default()).await?;
    let mut subscriber = Client::connect(address, None, 0, Options::default())
        .await?
        .subscribe(vec!["ttl-probe:*".into()], None)
        .await?;
    let mut rows = Vec::new();
    for n in 0..30 {
        let key = Value::String(format!("ttl-probe:{n}"));
        let started = Instant::now();
        for target in [
            key.clone(),
            Value::String(format!("ttl-probe:background:{n}")),
        ] {
            client
                .execute(
                    "SET",
                    vec![
                        target,
                        Value::Null,
                        Value::String("PX".into()),
                        Value::Int(20),
                    ],
                )
                .await?;
        }
        let accepted = Instant::now();
        // 从应答后等待完整 TTL，服务端到期不可能晚于此上界。
        // Waiting a full TTL after the reply is a conservative upper bound on the accepted deadline.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let visibility = client.execute("GET", vec![key]).await;
        let visible = match visibility {
            Err(linger_client::Error::Server(error)) if error.code == "ERR_KEY_NOT_FOUND" => false,
            Ok(_) => true,
            Err(error) => return Err(error.into()),
        };
        let mut expiry = None;
        let mut expired_count = 0;
        for _ in 0..6 {
            let event =
                tokio::time::timeout(Duration::from_secs(5), subscriber.next_event()).await??;
            subscriber.ack(event.id).await?;
            if event.kind == EventKind::Expired {
                expired_count += 1;
                if expired_count == 2 {
                    expiry = Some(accepted.elapsed().as_micros());
                    break;
                }
            }
        }
        let event_us = expiry.ok_or("missing expiry event")?;
        let mut memory = None;
        for _ in 0..100 {
            let info = client
                .execute("INFO", vec![Value::String("memory".into())])
                .await?;
            let Value::Map(sections) = info else {
                return Err("INFO envelope".into());
            };
            let Value::Map(fields) = &sections["memory"] else {
                return Err("INFO memory".into());
            };
            if fields["managed_bytes"] == Value::UInt(0) {
                memory = Some(accepted.elapsed().as_micros());
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        rows.push(serde_json::json!({"expired_value_visible":visible,"set_roundtrip_us":accepted.duration_since(started).as_micros(),"expiry_event_observed_after_reply_us":event_us,"reclaimed_after_reply_us":memory}));
        if visible || memory.is_none() {
            return Err("TTL validation failed".into());
        }
    }
    println!(
        "{}",
        serde_json::json!({"ttl_ms":20,"keys_per_sample":2,"background_key_read":false,"clock":"client monotonic; reply-relative conservative deadline, not server scheduler precision","samples":rows})
    );
    Ok(())
}
