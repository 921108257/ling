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
    about = "linger-cache-db 阶段 2 CLI；认证密码通过 LINGER_PASSWORD 环境变量提供"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,
    #[arg(long, default_value_t = 6380)]
    port: u16,
    #[arg(long, default_value_t = 0)]
    db: u16,
    #[arg(long, default_value_t = 30000)]
    timeout_ms: u64,
    /// 单条文本命令；省略时逐行读取标准输入
    #[arg(short = 'c', long)]
    command: Option<String>,
}

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
