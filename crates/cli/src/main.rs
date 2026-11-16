//! CLI 入口；网络语义复用 Rust SDK，本地帮助无需连接。
//! CLI entry point; network semantics reuse the Rust SDK and local help needs no connection.

use clap::Parser;
use ling_cli::{display_value, parse_command};
use ling_client::{Client, Options};
use std::{
    io::{self, BufRead, IsTerminal, Read, Write},
    net::{IpAddr, SocketAddr},
    time::Duration,
};

#[derive(Parser)]
#[command(name = "ling-cli", version)]
/// 启动参数；凭据不进入命令行参数。
/// Startup arguments; credentials are not accepted on the command line.
struct Args {
    /// 本地帮助语言，默认按系统语言选择。
    /// Local help language, detected from the system by default.
    #[arg(long)]
    lang: Option<String>,
    /// 离线显示全部命令速查。
    /// Shows the complete command reference offline.
    #[arg(long, conflicts_with_all = ["command", "batch"])]
    cheatsheet: bool,
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
    /// 验证服务端证书的名称；Verified TLS server name.
    #[arg(long)]
    tls_server_name: Option<String>,
    /// 自定义可信 CA 文件；Custom trusted CA file.
    #[arg(long, requires = "tls_server_name")]
    tls_ca: Option<std::path::PathBuf>,
    /// mTLS 客户端证书；mTLS client certificate.
    #[arg(long, requires_all = ["tls_server_name", "tls_key"])]
    tls_cert: Option<std::path::PathBuf>,
    /// mTLS 客户端私钥；mTLS client private key.
    #[arg(long, requires_all = ["tls_server_name", "tls_cert"])]
    tls_key: Option<std::path::PathBuf>,
    /// 从 stdin 读取最多 1000 条普通命令并以非事务 Batch 提交。
    /// Reads up to 1000 ordinary commands from stdin and submits a nontransactional Batch.
    #[arg(long, conflicts_with = "command")]
    batch: bool,
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
    let (args, language) = ling_cli_help::parse_localized::<Args>(false);
    if args.cheatsheet {
        print!("{}", ling_cli_help::cheatsheet(language));
        return Ok(true);
    }
    if let Some(result) = args
        .command
        .as_deref()
        .and_then(|s| local_help(s, language))
    {
        return Ok(result);
    }
    let password = std::env::var("LING_PASSWORD")
        .or_else(|_| std::env::var("LINGER_PASSWORD"))
        .ok();
    let options = Options {
        request_timeout: Duration::from_millis(args.timeout_ms),
        tls: args
            .tls_server_name
            .clone()
            .map(|server_name| ling_client::ClientTls {
                server_name,
                ca_file: args.tls_ca.clone(),
                cert_file: args.tls_cert.clone(),
                key_file: args.tls_key.clone(),
            }),
        ..Options::default()
    };
    if args.command.is_none() && !args.batch {
        return run_input(&args, language, password.as_deref(), options).await;
    }
    if args.batch {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut requests = Vec::new();
        let mut total = 0;
        loop {
            let mut line = Vec::new();
            let n = input
                .by_ref()
                .take(64 * 1024 * 1024 + 1)
                .read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            total += n;
            if total > 64 * 1024 * 1024 {
                return Err("批量输入超过上限".into());
            }
            let line = std::str::from_utf8(&line)?;
            if line.trim().is_empty() {
                continue;
            }
            if requests.len() >= 1000 {
                return Err("批量命令数超过上限".into());
            }
            let request = parse_command(line)?;
            if request.command == "HELP" {
                return Err(ling_cli_help::batch_help_error(language).into());
            }
            requests.push(request);
        }
        let mut client = Client::connect(
            SocketAddr::new(args.host, args.port),
            password.as_deref(),
            args.db,
            options,
        )
        .await?;
        let mut success = true;
        for result in client.batch(requests).await? {
            match result {
                Ok(value) => println!("{}", display_value(&value)),
                Err(error) => {
                    eprintln!("{error}");
                    success = false;
                }
            }
        }
        client.close().await?;
        return Ok(success);
    }
    let mut client = Client::connect(
        SocketAddr::new(args.host, args.port),
        password.as_deref(),
        args.db,
        options,
    )
    .await?;
    if let Some(source) = args.command.as_ref() {
        let request = parse_command(source)?;
        if request.command == "SUBSCRIBE" {
            let mut arguments = request.arguments.into_iter();
            let Some(ling_client::Value::List(patterns)) = arguments.next() else {
                unreachable!()
            };
            let patterns = patterns
                .into_iter()
                .map(|p| match p {
                    ling_client::Value::String(s) => s,
                    _ => unreachable!(),
                })
                .collect();
            let group = arguments.nth(1).map(|v| match v {
                ling_client::Value::String(s) => s,
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
                    ling_cli::display_event_id(event.id),
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
                        if local_help(&line, language).is_some() {
                            continue;
                        }
                        let request = match parse_command(&line) {
                            Ok(r) => r,
                            Err(e) => {
                                eprintln!("{e}");
                                continue;
                            }
                        };
                        match request.command.as_str() {
                            "ACK" => {
                                let id = ling_client::EventId::from_value(&request.arguments[0])?;
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
    let success = execute(
        &mut client,
        args.command.as_deref().expect("single command"),
    )
    .await;
    if !client.is_closed() {
        client.close().await?;
    }
    Ok(success)
}

/// 拦截本地 HELP，不回显未知主题，以免泄漏敏感输入。
/// Intercepts local HELP without echoing unknown topics that may contain sensitive input.
fn local_help(source: &str, language: ling_cli_help::Language) -> Option<bool> {
    let command = source.split_whitespace().next()?;
    if !command.eq_ignore_ascii_case("HELP") {
        return None;
    }
    let request = match parse_command(source) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("{error}");
            return Some(false);
        }
    };
    if request.arguments.is_empty() {
        print!("{}", ling_cli_help::cheatsheet(language));
        return Some(true);
    }
    let topic = request
        .arguments
        .iter()
        .filter_map(|value| match value {
            ling_client::Value::String(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    match ling_cli_help::command_help(language, &topic) {
        Some(help) => {
            print!("{help}");
            Some(true)
        }
        None => {
            eprintln!("{}", ling_cli_help::unknown_topic(language));
            Some(false)
        }
    }
}

/// 帮助先在本地执行，首条业务命令才触发连接。
/// Executes help locally and connects only for the first database command.
async fn run_input(
    args: &Args,
    language: ling_cli_help::Language,
    password: Option<&str>,
    options: Options,
) -> Result<bool, Box<dyn std::error::Error>> {
    let interactive = io::stdin().is_terminal();
    let mut client: Option<Client> = None;
    let mut success = true;
    if interactive {
        println!("{}", ling_cli_help::interactive_hint(language));
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        if interactive {
            print!("ling> ");
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
        if let Some(result) = local_help(source, language) {
            success &= result;
            continue;
        }
        // 无连接也可查阅帮助及退出；只有业务命令才建立连接。
        // Help and exit work offline; connect only for a database command.
        if source.trim().eq_ignore_ascii_case("QUIT") && client.is_none() {
            break;
        }
        if client.is_none() {
            client = Some(
                Client::connect(
                    SocketAddr::new(args.host, args.port),
                    password,
                    args.db,
                    options.clone(),
                )
                .await?,
            );
        }
        let active = client.as_mut().expect("connected above");
        success &= execute(active, source).await;
        if active.is_closed() {
            break;
        }
    }
    if let Some(mut client) = client {
        if !client.is_closed() {
            client.close().await?;
        }
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
