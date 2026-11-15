//! CLI 入口；网络语义复用 Rust SDK，标准输出只展示成功 Value。
//! CLI entry point; network semantics reuse the Rust SDK and stdout contains only successful Values.

use clap::Parser;
use linger_cli::{display_value, parse_command};
use linger_client::{Client, Options};
use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    net::{IpAddr, SocketAddr},
    time::Duration,
};

#[derive(Parser)]
#[command(
    version,
    about = "linger-cache-db 阶段 5 CLI；认证密码通过 LINGER_PASSWORD 环境变量提供"
)]
/// 启动参数；凭据不进入命令行参数。
/// Startup arguments; credentials are not accepted on the command line.
struct Args {
    /// 目标 IP，默认回环地址。
    /// Target IP, loopback by default.
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    /// 目标 TCP 端口。
    /// Target TCP port.
    #[arg(long, default_value_t = 6380)]
    port: u16,
    /// 初始化后选择的数据库编号。
    /// Database selected after initialization.
    #[arg(long, default_value_t = 0)]
    db: u16,
    /// 每个请求的总时限，单位毫秒。
    /// Overall deadline per request in milliseconds.
    #[arg(long, default_value_t = 30000)]
    timeout_ms: u64,
    /// 单条文本命令；省略时逐行读取标准输入。
    /// One text command; omission reads standard input line by line.
    #[arg(short = 'c', long)]
    command: Option<String>,
    /// 订阅模式接收数量，零表示持续到中断；Events to receive in subscription mode; zero waits until interrupted.
    #[arg(long, default_value_t = 0)]
    count: usize,
    /// 输出并刷新后自动 ACK；省略时逐条从 stdin 读取 ACK。
    /// Automatically ACK after output and flush; otherwise read explicit ACKs from stdin.
    #[arg(long)]
    auto_ack: bool,
}

/// 将失败转为非零退出状态，不输出敏感输入。
/// Converts failures to nonzero exit status without printing sensitive inputs.
#[tokio::main]
async fn main() {
    match run().await {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

/// 装配配置与运行时资源，完成退出前的连接清理。
/// Assembles configuration and runtime resources and cleans up connections before exit.
async fn run() -> Result<bool, Box<dyn std::error::Error>> {
    let args = Args::parse();
    let password = std::env::var("LINGER_PASSWORD").ok();
    let options = Options {
        request_timeout: Duration::from_millis(args.timeout_ms),
        ..Options::default()
    };
    let mut client = Client::connect(
        SocketAddr::new(args.host, args.port),
        password.as_deref(),
        args.db,
        options,
    )
    .await?;
    let mut success = true;
    if let Some(source) = args.command.as_ref() {
        let request = parse_command(source)?;
        if request.command == "SUBSCRIBE" {
            let mut arguments = request.arguments.into_iter();
            let Some(linger_client::Value::List(patterns)) = arguments.next() else {
                unreachable!()
            };
            let patterns = patterns
                .into_iter()
                .map(|p| match p {
                    linger_client::Value::String(s) => s,
                    _ => unreachable!(),
                })
                .collect();
            let group = arguments.nth(1).map(|v| match v {
                linger_client::Value::String(s) => s,
                _ => unreachable!(),
            });
            let mut subscriber = client.subscribe(patterns, group.as_deref()).await?;
            let mut received = 0;
            let mut input = if args.auto_ack {
                None
            } else {
                Some(subscription_input())
            };
            loop {
                let event = tokio::select! {
                    result=subscriber.next_event()=>result?,
                    _=tokio::signal::ctrl_c()=>break
                };
                // 输出成功后才确认；下游应按 event_id 幂等处理。
                // Acknowledge only after successful output; downstream processing should be idempotent by event_id.
                println!(
                    "{} {}",
                    linger_cli::display_event_id(event.id),
                    display_value(&event.to_value())
                );
                io::stdout().flush()?;
                if let Some(input) = input.as_mut() {
                    loop {
                        let line = tokio::select! {
                            line=input.recv()=>line,
                            _=tokio::signal::ctrl_c()=>None
                        };
                        let Some(line) = line else {
                            subscriber.close().await?;
                            return Ok(true);
                        };
                        let request = match parse_command(&line) {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("{e}");
                                continue;
                            }
                        };
                        match request.command.as_str() {
                            "ACK" => {
                                let id = linger_client::EventId::from_value(&request.arguments[0])?;
                                match subscriber.ack(id).await {
                                    Ok(()) if id == event.id => break,
                                    Ok(()) => {}
                                    Err(e) => eprintln!("{e}"),
                                }
                            }
                            "PING" if request.arguments.is_empty() => {
                                subscriber.ping().await?;
                            }
                            "UNSUBSCRIBE" | "QUIT" if request.arguments.is_empty() => {
                                subscriber.close().await?;
                                return Ok(true);
                            }
                            _ => eprintln!("订阅连接仅接受 ACK、PING、UNSUBSCRIBE 或 QUIT"),
                        }
                    }
                } else {
                    subscriber.ack(event.id).await?;
                }
                received += 1;
                if args.count != 0 && received >= args.count {
                    break;
                }
            }
            subscriber.close().await?;
            return Ok(true);
        }
    }
    if let Some(source) = args.command {
        success = execute(&mut client, &source).await;
    } else {
        let interactive = io::stdin().is_terminal();
        let stdin = io::stdin();
        let mut input = stdin.lock();
        loop {
            if interactive {
                print!("linger> ");
                io::stdout().flush()?;
            }
            let mut bytes = Vec::new();
            // 读取时限制每行大小，不先分配完整的无界输入。
            // Bound each input line before allocating its complete contents.
            let count = input
                .by_ref()
                .take(64 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut bytes)?;
            if count == 0 {
                break;
            }
            if count > 64 * 1024 * 1024 {
                eprintln!("命令超过输入上限");
                success = false;
                break;
            }
            let source = match std::str::from_utf8(&bytes) {
                Ok(s) => s,
                Err(_) => {
                    eprintln!("命令必须是 UTF-8");
                    success = false;
                    continue;
                }
            };
            if source.trim().is_empty() {
                continue;
            }
            success &= execute(&mut client, source).await;
            if client.is_closed() {
                break;
            }
        }
    }
    if !client.is_closed() {
        client.close().await?;
    }
    Ok(success)
}

/// 解析一条命令，通过 SDK 执行，并将错误写入 stderr。
/// Parses one command, executes it through the SDK, and writes errors to stderr.
async fn execute(client: &mut Client, source: &str) -> bool {
    let request = match parse_command(source) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return false;
        }
    };
    match client.execute(&request.command, request.arguments).await {
        Ok(value) => {
            println!("{}", display_value(&value));
            true
        }
        Err(error) => {
            eprintln!("{error}");
            false
        }
    }
}

/// 独立 stdin 线程与容量一的通道；不占用 Tokio 阻塞池等待人工输入。
/// A separate stdin thread and capacity-one channel avoid occupying Tokio's blocking pool while awaiting input.
fn subscription_input() -> tokio::sync::mpsc::Receiver<String> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        loop {
            let mut bytes = Vec::new();
            match input.by_ref().take(65537).read_until(b'\n', &mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(n) if n > 65536 => {
                    eprintln!("订阅控制命令超过输入上限");
                    break;
                }
                _ => {}
            }
            let Ok(line) = String::from_utf8(bytes) else {
                eprintln!("命令必须是 UTF-8");
                continue;
            };
            if tx.blocking_send(line).is_err() {
                break;
            }
        }
    });
    rx
}
