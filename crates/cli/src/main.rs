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
    about = "linger-cache-db 阶段 4 CLI；认证密码通过 LINGER_PASSWORD 环境变量提供"
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
