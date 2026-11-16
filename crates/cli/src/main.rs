//! CLI 入口；网络语义复用 Rust SDK，本地帮助无需连接。
//! CLI entry point; network semantics reuse the Rust SDK and local help needs no connection.

use clap::Parser;
use ling_cli::{display_value, parse_command};
use ling_cli_help::{Context, Diagnostic, Language};
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
async fn run() -> Result<bool, Diagnostic> {
    let (args, language) = ling_cli_help::parse_localized::<Args>(false);
    if args.cheatsheet {
        output(&ling_cli_help::cheatsheet(language), language)?;
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
                .read_until(b'\n', &mut line)
                .map_err(|_| Diagnostic::new(language, "LOCAL_IO", Context::Batch))?;
            if n == 0 {
                break;
            }
            total += n;
            if total > 64 * 1024 * 1024 {
                return Err(Diagnostic::new(
                    language,
                    "LOCAL_INPUT_LIMIT",
                    Context::Batch,
                ));
            }
            let line = std::str::from_utf8(&line)
                .map_err(|_| Diagnostic::new(language, "LOCAL_UTF8", Context::Batch))?;
            if line.trim().is_empty() {
                continue;
            }
            if requests.len() >= 1000 {
                return Err(Diagnostic::new(
                    language,
                    "LOCAL_INPUT_LIMIT",
                    Context::Batch,
                ));
            }
            let request = checked_parse(line, language)?;
            if request.command == "HELP" {
                return Err(Diagnostic::new(
                    language,
                    "LOCAL_BATCH_HELP",
                    Context::Batch,
                ));
            }
            requests.push(request);
        }
        let mut client = Client::connect(
            SocketAddr::new(args.host, args.port),
            password.as_deref(),
            args.db,
            options,
        )
        .await
        .map_err(|error| sdk_error(&error, language, Context::Batch))?;
        let mut success = true;
        let commands: Vec<_> = requests.iter().map(request_topic).collect();
        for (result, command) in client
            .batch(requests)
            .await
            .map_err(|error| sdk_error(&error, language, Context::Batch))?
            .into_iter()
            .zip(commands)
        {
            match result {
                Ok(mut value) => {
                    success &= item_diagnostics(&command, &mut value, language);
                    output(&format!("{}\n", display_value(&value)), language)?;
                }
                Err(error) => {
                    eprintln!("{}", Diagnostic::command(language, &error.code, &command));
                    success = false;
                }
            }
        }
        client
            .close()
            .await
            .map_err(|error| sdk_error(&error, language, Context::Batch))?;
        return Ok(success);
    }
    let source = args.command.as_deref().expect("single command");
    let request = checked_parse(source, language)?;
    let mut client = Client::connect(
        SocketAddr::new(args.host, args.port),
        password.as_deref(),
        args.db,
        options,
    )
    .await
    .map_err(|error| {
        Diagnostic::command(language, sdk_code(&error), &request_topic(&request))
            .outcome_unknown(error.outcome_unknown())
    })?;
    {
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
            let mut subscriber = client
                .subscribe(patterns, group.as_deref())
                .await
                .map_err(|error| sdk_error(&error, language, Context::Subscription))?;
            let mut received = 0;
            let mut success = true;
            let mut input = if args.auto_ack {
                None
            } else {
                Some(subscription_input(language))
            };
            loop {
                let event = tokio::select! {
                    result=subscriber.next_event()=>result.map_err(|error| sdk_error(&error, language, Context::Subscription))?,
                    _=tokio::signal::ctrl_c()=>break
                };
                // 输出成功后才确认；下游应按 event_id 幂等处理。
                // Acknowledge only after successful output; downstream processing should be idempotent by event_id.
                output(
                    &format!(
                        "{} {}\n",
                        ling_cli::display_event_id(event.id),
                        display_value(&event.to_value())
                    ),
                    language,
                )?;
                if let Some(input) = input.as_mut() {
                    loop {
                        let line = tokio::select! {
                            line=input.recv()=>line,
                            _=tokio::signal::ctrl_c()=>None
                        };
                        let Some(line) = line else {
                            subscriber.close().await.map_err(|error| {
                                sdk_error(&error, language, Context::Subscription)
                            })?;
                            return Ok(success);
                        };
                        let line = line?;
                        if let Some(result) = local_help(&line, language) {
                            success &= result;
                            continue;
                        }
                        let request = match parse_command(&line) {
                            Ok(r) => r,
                            Err(_) => {
                                eprintln!(
                                    "{}",
                                    Diagnostic::command(language, "LOCAL_PARSE", &line)
                                );
                                success = false;
                                continue;
                            }
                        };
                        match request.command.as_str() {
                            "ACK" => {
                                let id = ling_client::EventId::from_value(&request.arguments[0])
                                    .map_err(|_| {
                                        Diagnostic::new(
                                            language,
                                            "ERR_INVALID_ACK",
                                            Context::Subscription,
                                        )
                                    })?;
                                match subscriber.ack(id).await {
                                    Ok(()) if id == event.id => break,
                                    Ok(()) => {}
                                    Err(error) => {
                                        eprintln!(
                                            "{}",
                                            sdk_error(&error, language, Context::Subscription)
                                        );
                                        success = false;
                                    }
                                }
                            }
                            "PING" if request.arguments.is_empty() => {
                                subscriber.ping().await.map_err(|error| {
                                    sdk_error(&error, language, Context::Subscription)
                                })?;
                            }
                            "UNSUBSCRIBE" | "QUIT" if request.arguments.is_empty() => {
                                subscriber.close().await.map_err(|error| {
                                    sdk_error(&error, language, Context::Subscription)
                                })?;
                                return Ok(success);
                            }
                            _ => {
                                eprintln!(
                                    "{}",
                                    Diagnostic::new(
                                        language,
                                        "LOCAL_SUBSCRIPTION_COMMAND",
                                        Context::Subscription
                                    )
                                );
                                success = false;
                            }
                        }
                    }
                } else {
                    subscriber
                        .ack(event.id)
                        .await
                        .map_err(|error| sdk_error(&error, language, Context::Subscription))?;
                }
                received += 1;
                if args.count != 0 && received >= args.count {
                    break;
                }
            }
            subscriber
                .close()
                .await
                .map_err(|error| sdk_error(&error, language, Context::Subscription))?;
            return Ok(success);
        }
    }
    let success = execute(&mut client, request, language).await?;
    if !client.is_closed() {
        client
            .close()
            .await
            .map_err(|error| sdk_error(&error, language, Context::Client))?;
    }
    Ok(success)
}

/// 拦截本地 HELP，不回显未知主题，以免泄漏敏感输入。
/// Intercepts local HELP without echoing unknown topics that may contain sensitive input.
fn local_help(source: &str, language: Language) -> Option<bool> {
    let command = source.split_whitespace().next()?;
    if !command.eq_ignore_ascii_case("HELP") {
        return None;
    }
    let request = match parse_command(source) {
        Ok(request) => request,
        Err(_) => {
            eprintln!("{}", Diagnostic::command(language, "LOCAL_PARSE", source));
            return Some(false);
        }
    };
    if request.arguments.is_empty() {
        return Some(
            match output(&ling_cli_help::cheatsheet(language), language) {
                Ok(()) => true,
                Err(error) => {
                    eprintln!("{error}");
                    false
                }
            },
        );
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
        Some(help) => Some(match output(&help, language) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("{error}");
                false
            }
        }),
        None => {
            eprintln!(
                "{}",
                Diagnostic::new(language, "LOCAL_HELP_TOPIC", Context::Command("HELP"))
            );
            Some(false)
        }
    }
}

/// 帮助先在本地执行，首条业务命令才触发连接。
/// Executes help locally and connects only for the first database command.
async fn run_input(
    args: &Args,
    language: Language,
    password: Option<&str>,
    options: Options,
) -> Result<bool, Diagnostic> {
    let interactive = io::stdin().is_terminal();
    let mut client: Option<Client> = None;
    let mut success = true;
    if interactive {
        output(
            &format!("{}\n", ling_cli_help::interactive_hint(language)),
            language,
        )?;
    }
    let stdin = io::stdin();
    let mut input = stdin.lock();
    loop {
        if interactive {
            output("ling> ", language)?;
        }
        let mut bytes = Vec::new();
        // 读取时限制每行大小，不先分配完整的无界输入。
        // Bound each input line before allocating its complete contents.
        let count = input
            .by_ref()
            .take(64 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| Diagnostic::new(language, "LOCAL_IO", Context::Client))?;
        if count == 0 {
            break;
        }
        if count > 64 * 1024 * 1024 {
            eprintln!(
                "{}",
                Diagnostic::new(language, "LOCAL_INPUT_LIMIT", Context::Client)
            );
            success = false;
            break;
        }
        let source = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => {
                eprintln!(
                    "{}",
                    Diagnostic::new(language, "LOCAL_UTF8", Context::Client)
                );
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
        let request = match checked_parse(source, language) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("{error}");
                success = false;
                continue;
            }
        };
        if client.is_none() {
            client = Some(
                Client::connect(
                    SocketAddr::new(args.host, args.port),
                    password,
                    args.db,
                    options.clone(),
                )
                .await
                .map_err(|error| {
                    Diagnostic::command(language, sdk_code(&error), &request_topic(&request))
                        .outcome_unknown(error.outcome_unknown())
                })?,
            );
        }
        let active = client.as_mut().expect("connected above");
        success &= execute(active, request, language).await?;
        if active.is_closed() {
            break;
        }
    }
    if let Some(mut client) = client {
        if !client.is_closed() {
            client
                .close()
                .await
                .map_err(|error| sdk_error(&error, language, Context::Client))?;
        }
    }
    Ok(success)
}

/// 执行已在联网前解析的命令，错误仅使用稳定码与安全上下文。
/// Executes a command parsed before connecting; errors use only stable codes and safe context.
async fn execute(
    client: &mut Client,
    request: ling_protocol::Request,
    language: Language,
) -> Result<bool, Diagnostic> {
    let topic = request_topic(&request);
    match client.execute(&request.command, request.arguments).await {
        Ok(mut value) => {
            let success = item_diagnostics(&topic, &mut value, language);
            output(&format!("{}\n", display_value(&value)), language)?;
            Ok(success)
        }
        Err(error) => {
            eprintln!(
                "{}",
                Diagnostic::command(language, sdk_code(&error), &topic)
                    .outcome_unknown(error.outcome_unknown())
            );
            Ok(false)
        }
    }
}

/// 独立 stdin 线程与容量一的通道；不占用 Tokio 阻塞池等待人工输入。
/// A separate stdin thread and capacity-one channel avoid occupying Tokio's blocking pool while awaiting input.
fn subscription_input(
    language: Language,
) -> tokio::sync::mpsc::Receiver<Result<String, Diagnostic>> {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        loop {
            let mut bytes = Vec::new();
            match input.by_ref().take(65537).read_until(b'\n', &mut bytes) {
                Ok(0) => break,
                Err(_) => {
                    let _ = tx.blocking_send(Err(Diagnostic::new(
                        language,
                        "LOCAL_IO",
                        Context::Subscription,
                    )));
                    break;
                }
                Ok(n) if n > 65536 => {
                    let _ = tx.blocking_send(Err(Diagnostic::new(
                        language,
                        "LOCAL_INPUT_LIMIT",
                        Context::Subscription,
                    )));
                    break;
                }
                _ => {}
            }
            let Ok(line) = String::from_utf8(bytes) else {
                let _ = tx.blocking_send(Err(Diagnostic::new(
                    language,
                    "LOCAL_UTF8",
                    Context::Subscription,
                )));
                break;
            };
            if tx.blocking_send(Ok(line)).is_err() {
                break;
            }
        }
    });
    rx
}

/// 解析失败只引用内置命令签名，不包含原始参数。
/// Parse failures refer only to built-in signatures and never raw arguments.
fn checked_parse(source: &str, language: Language) -> Result<ling_protocol::Request, Diagnostic> {
    parse_command(source).map_err(|_| Diagnostic::command(language, "LOCAL_PARSE", source))
}

/// 安全写入成功输出并报告终端或管道故障，避免 println 的 I/O panic。
/// Writes successful output safely and reports terminal or pipe failures instead of println I/O panics.
fn output(text: &str, language: Language) -> Result<(), Diagnostic> {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|_| Diagnostic::new(language, "LOCAL_IO", Context::Client))
}

/// SDK 分类在展示层映射到本地码，不改变 SDK 或协议错误。
/// Maps SDK categories to display-only local codes without changing SDK or protocol errors.
fn sdk_code(error: &ling_client::Error) -> &str {
    use ling_client::{Error, LocalError};
    match error {
        Error::Server(error) => &error.code,
        Error::Local { kind, .. } => match kind {
            LocalError::Connect => "LOCAL_CONNECT",
            LocalError::Timeout => "LOCAL_TIMEOUT",
            LocalError::Disconnected => "LOCAL_DISCONNECTED",
            LocalError::Protocol => "LOCAL_PROTOCOL",
            LocalError::InvalidRequest => "LOCAL_INVALID_REQUEST",
            LocalError::Closed => "LOCAL_CLOSED",
            LocalError::BufferFull => "LOCAL_BUFFER_FULL",
        },
    }
}

/// 丢弃远端说明及底层异常文本，保留稳定错误码与结果未知状态。
/// Drops remote messages and low-level exception text while retaining codes and unknown outcomes.
fn sdk_error(error: &ling_client::Error, language: Language, context: Context) -> Diagnostic {
    Diagnostic::new(language, sdk_code(error), context).outcome_unknown(error.outcome_unknown())
}

/// CONFIG 子操作只有白名单名称可进入诊断上下文，其余参数不进入文本。
/// Only allowlisted CONFIG subcommands enter diagnostic context; other arguments never enter text.
fn request_topic(request: &ling_protocol::Request) -> String {
    if request.command == "CONFIG" {
        if let Some(ling_client::Value::String(subcommand)) = request.arguments.first() {
            let subcommand = subcommand.to_ascii_uppercase();
            if matches!(subcommand.as_str(), "GET" | "SET" | "RELOAD") {
                return format!("CONFIG {subcommand}");
            }
        }
    }
    request.command.clone()
}

/// 仅解释 MGET/FGET 约定的最外层结果封套，不遍历成功值或任意用户 Map。
/// Interprets only the outer result envelopes defined for MGET/FGET, never successful values or arbitrary user Maps.
fn item_diagnostics(command: &str, value: &mut ling_client::Value, language: Language) -> bool {
    use ling_client::Value;
    if !matches!(command, "MGET" | "FGET") {
        return true;
    }
    let Value::List(items) = value else {
        return true;
    };
    let mut success = true;
    for item in items {
        let Value::Map(fields) = item else {
            continue;
        };
        if fields.len() != 3 || fields.get("ok") != Some(&Value::Bool(false)) {
            continue;
        }
        let Some(Value::String(code)) = fields.get("code") else {
            continue;
        };
        if !matches!(fields.get("message"), Some(Value::String(_))) {
            continue;
        }
        let error = Diagnostic::command(language, code, command);
        fields.insert("message".into(), Value::String(error.message().into()));
        eprintln!("{error}");
        success = false;
    }
    success
}
