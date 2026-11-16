//! 命令行安全诊断：稳定错误码、翻译和静态示例，不展示原始输入。
//! Safe CLI diagnostics: stable codes, translations and static examples, never raw input.

use crate::{Language, entries};
use std::fmt;

/// 诊断所属上下文；命令名称只能来自内置目录。
/// Diagnostic context; command names are resolved only from the built-in catalog.
#[derive(Clone, Copy, Debug)]
pub enum Context {
    /// 客户端启动或连接；Client startup or connection.
    Client,
    /// 服务端配置或启动；Server configuration or startup.
    Server,
    /// 批量输入；Batch input.
    Batch,
    /// 专用订阅连接；Dedicated subscription connection.
    Subscription,
    /// 已验证的公开命令；Validated public command.
    Command(&'static str),
    /// 已知启动选项；Known startup option.
    Option(&'static str),
}

/// 不含原始请求、密码、文件内容和远端错误文本的本地化诊断。
/// Localized diagnostic without raw requests, passwords, file contents or remote messages.
#[derive(Debug)]
pub struct Diagnostic {
    language: Language,
    code: String,
    context: Context,
    unknown: bool,
}

impl Diagnostic {
    /// 从稳定错误码构造诊断；不合法的外部代码归为 ERR_SERVER。
    /// Builds a diagnostic from a stable code; malformed external codes become ERR_SERVER.
    pub fn new(language: Language, code: &str, context: Context) -> Self {
        let valid = code.len() <= 64
            && (code.starts_with("ERR_") || code.starts_with("LOCAL_"))
            && code
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
        let context = match context {
            Context::Command(name)
                if name != "CONFIG"
                    && !entries(Language::English)
                        .any(|(names, _, _)| names.split(',').any(|known| known == name)) =>
            {
                Context::Client
            }
            _ => context,
        };
        Self {
            language,
            code: if valid { code } else { "ERR_SERVER" }.into(),
            context,
            unknown: code == "ERR_COMMIT_UNKNOWN",
        }
    }

    /// 从命令输入提取安全上下文；只保留内置名称或唯一的一处拼写修正。
    /// Extracts safe context from command input, keeping only built-in names or a unique one-edit suggestion.
    pub fn command(language: Language, code: &str, source: &str) -> Self {
        Self::new(language, code, command_context(source))
    }

    /// 明确保留发送后执行结果未知，禁止自动重放的语义。
    /// Explicitly retains unknown post-send outcomes and the prohibition on automatic replay.
    pub fn outcome_unknown(mut self, unknown: bool) -> Self {
        self.unknown |= unknown;
        self
    }

    /// 返回安全的本地化说明，用于规范明确的逐项错误响应展示。
    /// Returns a safe localized description for displaying contract-defined per-item error envelopes.
    pub fn message(&self) -> &'static str {
        include_str!("diagnostics.tsv")
            .lines()
            .find_map(|line| {
                let mut columns = line.split('\t');
                let code = columns.next()?;
                (code == self.code)
                    .then(|| columns.nth(self.language as usize))
                    .flatten()
            })
            .unwrap_or_else(|| {
                self.language.text([
            "The server returned an unrecognized error; retain the code for troubleshooting.",
            "服务端返回未识别的错误；请保留错误码以便排查。",
            "서버가 알 수 없는 오류를 반환했습니다. 문제 해결을 위해 오류 코드를 보관하세요.",
            "サーバーが未認識のエラーを返しました。調査用にエラーコードを保存してください。",
            "Сервер вернул неизвестную ошибку; сохраните код для диагностики.",
            "Der Server meldet einen unbekannten Fehler; Fehlercode zur Diagnose aufbewahren.",
            "Le serveur a renvoyé une erreur inconnue ; conservez son code pour le diagnostic.",
        ])
            })
    }
}

impl fmt::Display for Diagnostic {
    /// 固定诊断布局，仅插入受信任译文、内置命令和静态示例。
    /// Uses one diagnostic layout with trusted translations, built-in commands and static examples.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message())?;
        if self.unknown {
            write!(f, " {}", self.language.text([
                "The request may have committed. Do not replay it automatically; verify its result first.",
                "请求可能已经提交。禁止自动重放；请先核实执行结果。",
                "요청이 커밋되었을 수 있습니다. 자동 재실행하지 말고 먼저 결과를 확인하세요.",
                "リクエストはコミット済みの可能性があります。自動再実行せず、先に結果を確認してください。",
                "Запрос мог быть выполнен. Не повторяйте автоматически; сначала проверьте результат.",
                "Die Anfrage wurde möglicherweise ausgeführt. Nicht automatisch wiederholen; zuerst Ergebnis prüfen.",
                "La requête a peut-être été validée. Ne la rejouez pas automatiquement ; vérifiez son résultat.",
            ]))?;
        }
        let examples = self.language.text([
            "Usage examples",
            "使用示例",
            "사용 예",
            "使用例",
            "Примеры использования",
            "Anwendungsbeispiele",
            "Exemples d’utilisation",
        ]);
        let related = self.language.text([
            "Related commands",
            "相关命令",
            "관련 명령",
            "関連コマンド",
            "Связанные команды",
            "Verwandte Befehle",
            "Commandes associées",
        ]);
        let (sample, commands) = guidance(self.context);
        write!(f, "\n\n{examples}\n")?;
        if let Context::Command(command) = self.context {
            if let Some(example) = command_example(command) {
                let label = if matches!(command, "HELLO" | "AUTH" | "SUBSCRIBE") {
                    self.language.text([
                        "Run in a terminal:",
                        "在终端执行：",
                        "터미널에서 실행:",
                        "端末で実行：",
                        "Выполнить в терминале:",
                        "Im Terminal ausführen:",
                        "Exécuter dans un terminal :",
                    ])
                } else if command == "ACK" {
                    help_label("ack_example", self.language)
                } else {
                    help_label("sequence_note", self.language)
                };
                writeln!(f, "  {label}")?;
                if command == "CONFIG RELOAD" {
                    writeln!(f, "  {}", help_label("config_example", self.language))?;
                }
                for line in example.split("\\n") {
                    writeln!(f, "  {line}")?;
                }
            } else {
                writeln!(f, "  ling-cli -c 'HELP {command}'\n  {sample}")?;
            }
        } else {
            writeln!(f, "  {sample}")?;
        }
        write!(f, "\n{related}\n  ")?;
        let mut related_commands = Vec::new();
        if let Context::Command(command) = self.context {
            related_commands.push(format!("HELP {command}"));
        }
        for command in commands.split("; ") {
            if !related_commands.iter().any(|existing| existing == command) {
                related_commands.push(command.to_owned());
            }
        }
        f.write_str(&related_commands.join("; "))
    }
}

impl std::error::Error for Diagnostic {}

/// 仅从第一个命令词查找目录，不复制后续的密码、Key 或 Value。
/// Resolves only the first command token without copying passwords, keys or Values.
fn command_context(source: &str) -> Context {
    let mut words = source.split_whitespace();
    let Some(word) = words.next() else {
        return Context::Client;
    };
    if word.len() > 20 || !word.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Context::Client;
    }
    let word = word.to_ascii_uppercase();
    if word == "CONFIG" {
        if let Some(subcommand) = words.next().map(str::to_ascii_uppercase) {
            return Context::Command(match subcommand.as_str() {
                "GET" => "CONFIG GET",
                "SET" => "CONFIG SET",
                "RELOAD" => "CONFIG RELOAD",
                _ => "CONFIG",
            });
        }
        return Context::Command("CONFIG");
    }
    let names = || entries(Language::English).flat_map(|(names, _, _)| names.split(','));
    if let Some(name) = names().find(|name| *name == word) {
        return Context::Command(name);
    }
    if word.len() < 3 {
        return Context::Client;
    }
    let mut close = names().filter(|name| one_edit_apart(word.as_bytes(), name.as_bytes()));
    match (close.next(), close.next()) {
        (Some(name), None) => Context::Command(name),
        _ => Context::Client,
    }
}

/// 复用帮助目录内已验证的逐命令示例，不引入另一套语法。
/// Reuses validated per-command help examples instead of maintaining another grammar.
fn command_example(name: &str) -> Option<&'static str> {
    include_str!("command-layout.tsv").lines().find_map(|line| {
        let mut cells = line.split('\t');
        (cells.next()? == name).then(|| cells.nth(2)).flatten()
    })
}

/// 复用帮助中关于执行位置与先决条件的译文。
/// Reuses help translations for execution context and prerequisites.
fn help_label(key: &str, language: Language) -> &'static str {
    include_str!("labels.tsv")
        .lines()
        .find_map(|line| {
            let mut cells = line.split('\t');
            (cells.next()? == key)
                .then(|| cells.nth(language as usize))
                .flatten()
        })
        .expect("complete help label")
}

/// 一次插入、删除、替换或相邻交换，仅用于唯一的保守建议。
/// One insertion, deletion, replacement or adjacent swap for unique conservative suggestions.
fn one_edit_apart(a: &[u8], b: &[u8]) -> bool {
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    if a.len() == b.len() {
        let differences: Vec<_> = a
            .iter()
            .zip(b)
            .enumerate()
            .filter(|(_, (x, y))| x != y)
            .map(|(i, _)| i)
            .collect();
        return differences.len() == 1
            || (differences.len() == 2
                && differences[1] == differences[0] + 1
                && a[differences[0]] == b[differences[1]]
                && a[differences[1]] == b[differences[0]]);
    }
    let (short, long) = if a.len() < b.len() { (a, b) } else { (b, a) };
    let first = short
        .iter()
        .zip(long)
        .position(|(x, y)| x != y)
        .unwrap_or(short.len());
    short[first..] == long[first + 1..]
}

/// 静态例子不含用户数据；未知上下文只给安全的帮助与连通性入口。
/// Static examples contain no user data; unknown contexts offer only safe help and connectivity entries.
fn guidance(context: Context) -> (&'static str, &'static str) {
    match context {
        Context::Option("--port") => ("ling-cli --port 6380 -c 'PING'", "ling-cli --help; PING"),
        Context::Option("--host") => (
            "ling-cli --host 127.0.0.1 -c 'PING'",
            "ling-cli --help; PING",
        ),
        Context::Option("--db") => (
            "ling-cli --db 0 -c 'PING'",
            "ling-cli --help; HELP SELECT; HELP DBSIZE",
        ),
        Context::Option("--timeout-ms") => (
            "ling-cli --timeout-ms 30000 -c 'PING'",
            "ling-cli --help; PING",
        ),
        Context::Option("--count") => (
            "ling-cli --count 1 -c 'SUBSCRIBE order:*'",
            "ling-cli --help; HELP SUBSCRIBE; HELP ACK",
        ),
        Context::Option("--tls-server-name" | "--tls-ca" | "--tls-cert" | "--tls-key") => (
            "ling-cli --tls-server-name cache.example.com --tls-ca ca.pem -c 'PING'",
            "ling-cli --help; HELP PING",
        ),
        Context::Option("--lang") => (
            "ling-cli --lang en-US --help",
            "ling --help; ling-cli --help",
        ),
        Context::Option("--batch" | "--cheatsheet" | "--command") => (
            "ling-cli -c 'PING'\n  ling-cli --cheatsheet",
            "ling-cli --help; HELP",
        ),
        Context::Option("--config") => guidance(Context::Server),
        Context::Option(_) => guidance(Context::Client),
        Context::Server => (
            "ling --config ling.toml\n  ling --help",
            "ling --help; ling-cli -c 'INFO'; ling-cli -c 'CONFIG GET server.*'",
        ),
        Context::Batch => (
            "ling-cli --batch < commands.txt\n  ling-cli -c 'HELP SET'",
            "ling-cli --help; HELP SET; HELP MSET",
        ),
        Context::Subscription => (
            "ling-cli -c 'SUBSCRIBE order:* GROUP workers'\n  ACK 00000000000000000000000000000000/0/0/1",
            "HELP SUBSCRIBE; HELP ACK; PING; UNSUBSCRIBE; QUIT",
        ),
        Context::Command(command) => command_guidance(command),
        Context::Client => (
            "ling-cli --host 127.0.0.1 --port 6380 -c 'PING'\n  ling-cli --help",
            "ling-cli --cheatsheet; HELP; PING",
        ),
    }
}

/// 从 clap 的选项上下文提取白名单名称，不使用参数值或自由格式错误文本。
/// Extracts an allowlisted option from clap context without argument values or free-form error text.
pub fn argument_diagnostic(language: Language, error: &clap::Error, server: bool) -> Diagnostic {
    use clap::error::{ContextKind, ContextValue};
    let context = if server {
        Context::Server
    } else {
        Context::Client
    };
    let option = match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(value)) => value.split_whitespace().next(),
        _ => None,
    };
    let known = [
        "--port",
        "--host",
        "--db",
        "--timeout-ms",
        "--count",
        "--tls-server-name",
        "--tls-ca",
        "--tls-cert",
        "--tls-key",
        "--lang",
        "--batch",
        "--cheatsheet",
        "--command",
        "--config",
    ];
    Diagnostic::new(
        language,
        "LOCAL_ARGUMENT",
        known
            .into_iter()
            .find(|known| Some(*known) == option)
            .map_or(context, Context::Option),
    )
}

/// 按命令意图选择固定示例，不在失败后自动执行。
/// Selects fixed examples by command intent without automatically executing after failure.
fn command_guidance(command: &str) -> (&'static str, &'static str) {
    match command {
        "SET" | "MSET" => (
            "ling-cli -c 'SET example int(1) PX 5000'",
            "HELP SET; HELP MSET; GET; PTTL",
        ),
        "GET" | "MGET" | "GETSET" => (
            "ling-cli -c 'GET example'\n  ling-cli -c 'HELP GET'",
            "HELP GET; HELP MGET; SET; EXISTS; TYPE",
        ),
        "DEL" | "EXISTS" | "TYPE" => (
            "ling-cli -c 'EXISTS example'",
            "HELP DEL; HELP EXISTS; HELP TYPE; GET",
        ),
        "RENAME" | "RENAMENX" | "COPY" | "MOVE" => (
            "ling-cli -c 'COPY example example-copy DB 1'",
            "HELP COPY; HELP MOVE; HELP RENAME; HELP RENAMENX",
        ),
        "SCAN" | "KEYS" | "RANDOMKEY" => (
            "ling-cli -c 'SCAN 0 MATCH example:* COUNT 100'",
            "HELP SCAN; HELP KEYS; HELP RANDOMKEY",
        ),
        "EXPIRE" | "PEXPIRE" | "EXPIREAT" | "PEXPIREAT" | "PERSIST" | "TTL" | "PTTL"
        | "EXPIRETIME" | "PEXPIRETIME" => (
            "ling-cli -c 'PTTL example'\n  ling-cli -c 'PEXPIRE example 5000'",
            "HELP PTTL; HELP PEXPIRE; HELP PERSIST; HELP SET",
        ),
        "INCR" | "DECR" | "INCRBY" | "DECRBY" | "INCRFLOAT" => (
            "ling-cli -c 'SET example-counter int(0)'\n  ling-cli -c 'INCRBY example-counter 2'",
            "HELP INCRBY; HELP INCRFLOAT; TYPE",
        ),
        "APPEND" | "STRLEN" | "BLEN" => (
            "ling-cli -c 'HELP APPEND'\n  ling-cli -c 'STRLEN example'",
            "HELP APPEND; HELP STRLEN; HELP BLEN; TYPE",
        ),
        "LPUSH" | "RPUSH" | "LPOP" | "RPOP" | "LRANGE" | "LINDEX" | "LSET" | "LLEN" | "LREM"
        | "LTRIM" => (
            "ling-cli -c 'HELP LRANGE'\n  ling-cli -c 'LRANGE example-list 0 -1'",
            "HELP LPUSH; HELP LRANGE; HELP LSET; TYPE",
        ),
        "FSET" | "FSETNX" | "FGET" | "FDEL" | "FEXISTS" | "FKEYS" | "FVALS" | "FGETALL"
        | "FLEN" | "FMERGE" => (
            "ling-cli -c 'HELP FSET'\n  ling-cli -c 'FGET example-map name'",
            "HELP FSET; HELP FGET; HELP FGETALL; TYPE",
        ),
        "SADD" | "SREM" | "SISMEMBER" | "SMISMEMBER" | "SMEMBERS" | "SCARD" | "SUNION"
        | "SINTER" | "SDIFF" | "SUNIONSTORE" | "SINTERSTORE" | "SDIFFSTORE" | "SRANDMEMBER"
        | "SPOP" => (
            "ling-cli -c 'HELP SADD'\n  ling-cli -c 'SMEMBERS example-set'",
            "HELP SADD; HELP SMEMBERS; HELP SUNION; TYPE",
        ),
        "SUBSCRIBE" | "ACK" | "UNSUBSCRIBE" => guidance(Context::Subscription),
        "CONFIG" | "CONFIG GET" | "CONFIG SET" | "CONFIG RELOAD" | "INFO" | "SHUTDOWN" => (
            "ling-cli -c 'CONFIG GET log.*'\n  ling-cli -c 'INFO'",
            "HELP CONFIG; HELP INFO; HELP SHUTDOWN",
        ),
        "SELECT" | "DBSIZE" | "SWAPDB" | "FLUSHDB" | "FLUSHALL" => (
            "ling-cli --db 1 -c 'DBSIZE'",
            "HELP SELECT; HELP DBSIZE; HELP FLUSHDB; HELP SWAPDB",
        ),
        "HELP" => (
            "ling-cli -c 'HELP SET'\n  ling-cli --cheatsheet",
            "HELP; HELP SET; ling-cli --help",
        ),
        "AUTH" | "HELLO" | "PING" | "QUIT" => (
            "ling-cli -c 'PING'\n  ling-cli --help",
            "HELP PING; HELP AUTH; HELP HELLO; HELP QUIT",
        ),
        _ => guidance(Context::Client),
    }
}
