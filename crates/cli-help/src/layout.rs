//! 有层级的纯文本帮助，不依赖终端颜色或宽表格。
//! Hierarchical plain-text help without terminal colors or wide tables.
//!
//! 本模块的 String formatting 断言只写 String；其 fmt::Write 实现不会返回格式化错误，参数均为标准字符串与整数。
//! String-formatting assertions here only write to String, whose fmt::Write never returns formatting errors; arguments are standard strings and integers.

use super::{Language, entries, flag_help};
use clap::Command;
use std::fmt::Write;

/// 文本每行限制为 72 个显示列，窄终端仍保留层级缩进。
/// Limits text to 72 display columns while keeping hierarchy on narrow terminals.
const WIDTH: usize = 72;

/// 按稳定键查询七种语言的标题和提示。
/// Looks up headings and notes in all seven locales by stable key.
fn label(key: &str, language: Language) -> &'static str {
    // 键由本模块固定调用点和静态目录提供，七语帮助契约保证内嵌表覆盖全部键与语言。
    // Keys come from fixed call sites and static catalogs; seven-locale help contracts cover every embedded key and language.
    include_str!("labels.tsv")
        .lines()
        .find_map(|line| {
            let mut cells = line.split('\t');
            (cells.next()? == key)
                .then(|| cells.nth(language as usize))
                .flatten()
        })
        .expect("complete help labels")
}

/// 近似终端宽度：组合符不占列，东亚宽字符占两列。
/// Approximates terminal width: combining marks use no column, wide East Asian characters use two.
fn columns(ch: char) -> usize {
    match ch as u32 {
        0x0300..=0x036f | 0xfe00..=0xfe0f => 0,
        0x1100..=0x115f
        | 0x2329..=0x232a
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe10..=0xfe6f
        | 0xff01..=0xff60
        | 0xffe0..=0xffe6
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

/// 折行说明文字，不改变示例命令内部空白与引号。
/// Wraps explanatory prose; example commands retain their whitespace and quotes.
fn prose(output: &mut String, text: &str, indent: usize) {
    let prefix = " ".repeat(indent);
    for paragraph in text.lines() {
        let mut line = String::new();
        let mut width = indent;
        for word in paragraph.split_whitespace() {
            let word_width: usize = word.chars().map(columns).sum();
            if !line.is_empty() && width + 1 + word_width > WIDTH {
                writeln!(output, "{prefix}{line}").expect("String formatting");
                line.clear();
                width = indent;
            }
            if !line.is_empty() {
                line.push(' ');
                width += 1;
            }
            for ch in word.chars() {
                if width + columns(ch) > WIDTH {
                    writeln!(output, "{prefix}{line}").expect("String formatting");
                    line.clear();
                    width = indent;
                }
                line.push(ch);
                width += columns(ch);
            }
        }
        if !line.is_empty() {
            writeln!(output, "{prefix}{line}").expect("String formatting");
        }
    }
}

/// 标题前空行将主组与缩进子组分开。
/// Blank lines before headings separate main groups and indented subgroups.
fn heading(output: &mut String, key: &str, language: Language, indent: usize) {
    if !output.ends_with("\n\n") {
        output.push('\n');
    }
    writeln!(output, "{}{}", " ".repeat(indent), label(key, language)).expect("String formatting");
}

/// 原样输出命令行，避免折行后的复制改变行为。
/// Emits literal command lines so wrapping cannot change copy-and-paste behavior.
fn example(output: &mut String, commands: &str, indent: usize) {
    for line in commands.split("\\n") {
        writeln!(output, "{}{line}", " ".repeat(indent)).expect("String formatting");
    }
}

/// 显示已定义选项的名称、默认值与换行说明。
/// Displays defined option names, defaults and descriptions on separate lines.
fn flags(output: &mut String, command: &Command, ids: &[&str], language: Language) {
    for id in ids {
        let arg = command.get_arguments().find(|arg| arg.get_id() == *id);
        // 全局语言选项在 clap 构建时才传播到子命令，预渲染仍展示它。
        // Global language flags propagate during clap build, after help is pre-rendered.
        if arg.is_none() && *id != "lang" {
            continue;
        }
        let signature = match *id {
            "host" => "--host <IP>",
            "port" => "--port <PORT>",
            "db" => "--db <DB>",
            "timeout_ms" => "--timeout-ms <MS>",
            "tls_server_name" => "--tls-server-name <NAME>",
            "tls_ca" => "--tls-ca <FILE>",
            "tls_cert" => "--tls-cert <FILE>",
            "tls_key" => "--tls-key <FILE>",
            "command" => "-c, --command <TEXT>",
            "batch" => "--batch",
            "count" => "--count <N>",
            "auto_ack" => "--auto-ack",
            "config" => "--config <FILE>",
            "check" => "--check",
            "target_version" => "--version <VERSION>",
            "stable" => "--stable",
            "lang" => "--lang <LOCALE>",
            "cheatsheet" => "--cheatsheet",
            "help" => "-h, --help",
            "version" => "-V, --version",
            // ids 仅来自下方 startup 中的固定选项列表，已由七语启动帮助测试遍历。
            // Ids come only from startup's fixed option lists below, traversed by seven-locale startup-help tests.
            _ => unreachable!("known help flag"),
        };
        writeln!(output, "    {signature}").expect("String formatting");
        if let Some(description) = flag_help(id, language) {
            prose(output, description, 6);
        }
        let defaults = arg.map(|arg| arg.get_default_values()).unwrap_or_default();
        if !defaults.is_empty() && !matches!(*id, "batch" | "auto_ack" | "cheatsheet") {
            writeln!(
                output,
                "      {}: {}",
                label("default", language),
                defaults
                    .iter()
                    .map(|value| value.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .expect("String formatting");
        }
        output.push('\n');
    }
}

/// 生成启动帮助，两个层级都由空行和缩进表示。
/// Builds startup help with two levels expressed through spacing and indentation.
pub(super) fn startup(command: &Command, language: Language, server: bool) -> String {
    let mut output = String::new();
    // localize 在调用前为根命令与 update 设置 about；这里只接收这两个静态命令。
    // Localize assigns about before calling this function, which accepts only the two static root/update commands.
    prose(
        &mut output,
        &command.get_about().expect("help introduction").to_string(),
        0,
    );
    if command.get_name() == "update" {
        heading(&mut output, "usage", language, 0);
        example(&mut output, "ling update [OPTIONS]", 2);
        heading(&mut output, "options", language, 0);
        flags(
            &mut output,
            command,
            &["check", "target_version", "stable", "lang", "help"],
            language,
        );
        prose(&mut output, label("update_note", language), 2);
        heading(&mut output, "examples", language, 0);
        example(
            &mut output,
            "ling update --check\\nling update\\nling update --stable",
            2,
        );
        return output;
    }
    if command.get_name() == "help" {
        heading(&mut output, "usage", language, 0);
        example(
            &mut output,
            if server {
                "ling help [TOPIC ...]"
            } else {
                "ling-cli help [TOPIC ...]"
            },
            2,
        );
        heading(&mut output, "options", language, 0);
        flags(&mut output, command, &["lang", "help"], language);
        heading(&mut output, "examples", language, 0);
        example(
            &mut output,
            if server {
                "ling help update"
            } else {
                "ling-cli help\\nling-cli help containers list\\nling-cli help SET examples"
            },
            2,
        );
        return output;
    }
    heading(&mut output, "usage", language, 0);
    example(
        &mut output,
        if server {
            "ling [OPTIONS]"
        } else {
            "ling-cli [OPTIONS]\\nling-cli help [TOPIC ...]\\nling-cli --help [TOPIC ...]"
        },
        2,
    );
    if server {
        heading(&mut output, "server", language, 0);
        heading(&mut output, "configuration", language, 2);
        flags(&mut output, command, &["config"], language);
        prose(&mut output, label("server_note", language), 4);
    } else {
        heading(&mut output, "connection", language, 0);
        for (group, names) in [
            ("address", &["host", "port"][..]),
            ("database", &["db"][..]),
            ("timeout", &["timeout_ms"][..]),
        ] {
            heading(&mut output, group, language, 2);
            flags(&mut output, command, names, language);
        }
        heading(&mut output, "security", language, 0);
        heading(&mut output, "authentication", language, 2);
        prose(&mut output, label("auth_note", language), 4);
        heading(&mut output, "tls", language, 2);
        flags(
            &mut output,
            command,
            &["tls_server_name", "tls_ca", "tls_cert", "tls_key"],
            language,
        );
        heading(&mut output, "execution", language, 0);
        for (group, names) in [
            ("single", &["command"][..]),
            ("batch", &["batch"][..]),
            ("subscription", &["count", "auto_ack"][..]),
        ] {
            heading(&mut output, group, language, 2);
            flags(&mut output, command, names, language);
        }
    }
    heading(&mut output, "reference", language, 0);
    heading(&mut output, "language", language, 2);
    flags(&mut output, command, &["lang"], language);
    heading(&mut output, "help", language, 2);
    flags(
        &mut output,
        command,
        &["cheatsheet", "help", "version"],
        language,
    );
    heading(&mut output, "examples", language, 0);
    heading(&mut output, "startup", language, 2);
    example(
        &mut output,
        if server {
            "ling --config /path/to/ling.toml"
        } else {
            "ling-cli -c 'PING'\\nling-cli --db 1 -c 'SET greeting \"hello\"'"
        },
        4,
    );
    heading(&mut output, "lookup", language, 2);
    example(
        &mut output,
        if server {
            "ling update\\nling help update\\nling-cli help"
        } else {
            "ling-cli help\\nling-cli help containers list\\nling-cli help SET examples\\nling-cli --cheatsheet"
        },
        4,
    );
    heading(&mut output, "more", language, 0);
    example(&mut output, "https://921108257.github.io/ling/", 2);
    output
}

/// 单个命令的固定分组、子组和可执行文本示例。
/// Fixed category, subgroup and textual example for one command.
fn metadata(name: &str) -> (&'static str, &'static str, &'static str) {
    // name 来自内嵌命令目录，目录契约逐行验证布局表的四列与一一覆盖关系。
    // Names come from the embedded command catalog; catalog contracts check every layout row's four columns and one-to-one coverage.
    include_str!("command-layout.tsv")
        .lines()
        .find_map(|line| {
            let mut cells = line.split('\t');
            (cells.next()? == name).then(|| {
                (
                    cells.next().unwrap(),
                    cells.next().unwrap(),
                    cells.next().unwrap(),
                )
            })
        })
        .expect("every command has layout metadata")
}

/// 固定分组顺序保证相关命令相邻，所有命令仍从共享目录获取。
/// A fixed hierarchy keeps related commands together while using the shared catalog.
const GROUPS: &[(&str, &[&str])] = &[
    ("reference", &["lookup"]),
    ("data", &["readwrite", "identity", "transfer", "scan"]),
    ("expiry", &["setexpiry", "queryexpiry"]),
    ("scalar", &["numbers", "text"]),
    ("containers", &["list", "map", "set"]),
    ("administration", &["session", "database", "operations"]),
    ("events", &["consume"]),
];

/// 目录只列下一层入口；分组键不翻译，避免用户猜测输入。
/// Lists only the next level; group keys remain stable across locales.
fn directory(language: Language, group: Option<(&str, &[&str])>) -> String {
    let mut output = String::new();
    let title = group.map_or("directory", |(name, _)| name);
    writeln!(output, "{}", label(title, language)).expect("String formatting");
    prose(&mut output, label("browse_note", language), 0);
    output.push('\n');
    match group {
        Some((name, subgroups)) => {
            for subgroup in subgroups {
                prose(
                    &mut output,
                    &format!("HELP {name} {subgroup} — {}", label(subgroup, language)),
                    2,
                );
            }
        }
        None => {
            for (name, _) in GROUPS {
                prose(
                    &mut output,
                    &format!("HELP {name} — {}", label(name, language)),
                    2,
                );
            }
        }
    }
    output.push('\n');
    example(&mut output, "HELP SET\\nHELP SET examples", 2);
    prose(&mut output, label("full_reference", language), 2);
    output
}

/// 子组只列本组命令；完整签名和示例由下一层按需提供。
/// Lists only commands in a subgroup; signatures and examples belong to the next level.
fn subgroup(language: Language, group: &str, subgroup: &str) -> String {
    let mut output = format!(
        "{} / {}\n",
        label(group, language),
        label(subgroup, language)
    );
    prose(&mut output, label("command_note", language), 0);
    for (names, _, description) in entries(language) {
        // str::split 即使输入为空也至少返回一段，next 必定为 Some。
        // str::split yields at least one segment even for empty input, so next is always Some.
        let name = names.split(',').next().unwrap();
        let (item_group, item_subgroup, _) = metadata(name);
        if group == item_group && subgroup == item_subgroup {
            output.push('\n');
            prose(&mut output, &names.replace(',', ", "), 2);
            prose(&mut output, description, 4);
        }
    }
    output
}

/// 渲染一条目录签名，替代项拆行但保留语法文本。
/// Renders a catalog signature, splitting command alternatives without changing syntax.
fn signature(output: &mut String, source: &str, indent: usize) {
    let mut start = 0;
    let mut brackets = 0;
    for (index, ch) in source.char_indices() {
        match ch {
            '[' => brackets += 1,
            ']' => brackets -= 1,
            '|' if brackets == 0 && source[index..].starts_with("| ") => {
                prose(output, source[start..index].trim(), indent);
                start = index + 1;
            }
            _ => {}
        }
    }
    prose(output, source[start..].trim(), indent);
}

/// 单命令详情从组合签名选出该命令，方括号中的可选项保持完整。
/// Selects one command from a combined signature while retaining bracketed options.
fn selected_signature<'a>(source: &'a str, name: &str) -> &'a str {
    let mut start = 0;
    let mut brackets = 0;
    for (index, ch) in source.char_indices() {
        match ch {
            '[' => brackets += 1,
            ']' => brackets -= 1,
            '|' if brackets == 0 && source[index..].starts_with("| ") => {
                let part = source[start..index].trim();
                if part == name
                    || part
                        .strip_prefix(name)
                        .is_some_and(|tail| tail.starts_with(' '))
                {
                    return part;
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    let part = source[start..].trim();
    if part == name
        || part
            .strip_prefix(name)
            .is_some_and(|tail| tail.starts_with(' '))
    {
        part
    } else {
        source
    }
}

/// 分层目录以简短说明和空行保留快速查找能力。
/// Hierarchical reference uses short descriptions and whitespace for quick scanning.
pub(super) fn reference(language: Language) -> String {
    let mut output = label("reference", language).to_owned();
    output.push('\n');
    prose(&mut output, super::interactive_hint(language), 0);
    for (group, subgroups) in GROUPS {
        heading(&mut output, group, language, 0);
        for subgroup in *subgroups {
            heading(&mut output, subgroup, language, 2);
            for (names, source, description) in entries(language) {
                // str::split 即使输入为空也至少返回一段，next 必定为 Some。
                // str::split yields at least one segment even for empty input, so next is always Some.
                let name = names.split(',').next().unwrap();
                let (item_group, item_subgroup, _) = metadata(name);
                if item_group == *group && item_subgroup == *subgroup {
                    signature(&mut output, source, 4);
                    prose(&mut output, description, 6);
                    output.push('\n');
                }
            }
        }
    }
    notes(&mut output, language);
    output
}

/// 类型说明使用协议标识，不翻译 Value 名称或数值语法。
/// Type notes preserve protocol identifiers and numeric syntax.
fn argument_types(name: &str) -> &'static str {
    match name {
        "HELP" => "command: ASCII String",
        "SET" | "GETSET" => "key: String; value: Value; EX/PX/EXAT/PXAT: Int",
        "MSET" => "key: String; value: Value",
        "COPY" | "MOVE" | "SELECT" | "SWAPDB" | "HELLO" => "key: String; db/version: UInt",
        "SCAN" => "cursor/COUNT: UInt; MATCH/TYPE: String",
        "EXPIRE" | "PEXPIRE" | "EXPIREAT" | "PEXPIREAT" => "key: String; time: Int",
        "INCRBY" | "DECRBY" => "key: String; value: Int/UInt; n: Int",
        "INCR" | "DECR" => "key: String; value: Int/UInt",
        "INCRFLOAT" => "key: String; value/delta: Float",
        "APPEND" | "STRLEN" => "key: String; value: String",
        "BLEN" => "key: String; value: Bytes",
        "LPUSH" | "RPUSH" => "key: String; value: List; item: Value",
        "LPOP" | "RPOP" => "key: String; value: List; count: UInt",
        "LRANGE" | "LTRIM" | "LINDEX" => "key: String; value: List; index/start/end: Int",
        "LSET" | "LREM" => "key: String; value: List; index/count: Int; item: Value",
        "LLEN" => "key: String; value: List",
        "FSET" | "FSETNX" => "key/field: String; value: Map; item: Value",
        "FGET" | "FDEL" | "FEXISTS" => "key/field: String; value: Map",
        "FKEYS" | "FVALS" | "FGETALL" | "FLEN" | "FMERGE" => "key: String; value: Map",
        "SADD" | "SREM" | "SISMEMBER" | "SMISMEMBER" => "key: String; value: Set; member: Scalar",
        "SRANDMEMBER" => "key: String; value: Set; count: Int",
        "SPOP" => "key: String; value: Set; count: UInt",
        "SMEMBERS" | "SCARD" | "SUNION" | "SINTER" | "SDIFF" | "SUNIONSTORE" | "SINTERSTORE"
        | "SDIFFSTORE" => "key/dest: String; value: Set",
        "ACK" => "stream: 32 hex; db: u16; shard: u32; sequence: u64",
        "SUBSCRIBE" => "pattern/group: String",
        "AUTH" => "password: String",
        "PING" => "message: String",
        "CONFIG GET" | "CONFIG SET" | "INFO" => "pattern/option/section: String",
        "DBSIZE" | "SHUTDOWN" | "QUIT" | "CONFIG RELOAD" | "UNSUBSCRIBE" => "()",
        "FLUSHDB" | "FLUSHALL" => "ASYNC: String",
        _ => "key: String; value: Value",
    }
}

/// 可单独查询的稳定章节名，不随帮助语言改变。
/// Stable section names can be requested independently in every locale.
const SECTIONS: &[&str] = &[
    "syntax",
    "description",
    "options",
    "types",
    "examples",
    "related",
];

/// 分层查询优先匹配目录路径，命令和章节保持大小写不敏感。
/// Resolves directory paths first, then case-insensitive command and section names.
pub(super) fn detail(language: Language, topic: &str) -> Option<String> {
    let mut words: Vec<_> = topic.split_whitespace().collect();
    if words.is_empty() {
        return Some(directory(language, None));
    }
    if let Some((group, subgroups)) = GROUPS
        .iter()
        .find(|(group, _)| group.eq_ignore_ascii_case(words[0]))
    {
        return match words.as_slice() {
            [_] => Some(directory(language, Some((group, subgroups)))),
            [_, key] => subgroups
                .iter()
                .find(|subgroup| subgroup.eq_ignore_ascii_case(key))
                .map(|key| subgroup(language, group, key)),
            _ => None,
        };
    }
    let section = words.last().and_then(|word| {
        SECTIONS
            .iter()
            .copied()
            .find(|section| section.eq_ignore_ascii_case(word))
    });
    if section.is_some() {
        words.pop();
    }
    let topic = words.join(" ").to_ascii_uppercase();
    let selected: Vec<_> = entries(language)
        .filter(|(names, _, _)| {
            names.split(',').any(|name| name == topic)
                || (topic == "CONFIG" && names.starts_with("CONFIG "))
        })
        .collect();
    if selected.is_empty() {
        return None;
    }
    let requested = [section.unwrap_or_default()];
    let selected_sections = if section.is_some() {
        &requested[..]
    } else {
        SECTIONS
    };
    let mut output = format!("{topic}\n");
    for (names, source, description) in selected {
        let name = if names.split(',').any(|name| name == topic) {
            topic.as_str()
        } else {
            names
        };
        let (group, subgroup, sample) = metadata(name);
        if section.is_none() {
            heading(&mut output, group, language, 0);
            heading(&mut output, subgroup, language, 2);
        } else if topic == "CONFIG" {
            prose(&mut output, name, 0);
        }
        for selected_section in selected_sections {
            heading(&mut output, selected_section, language, 2);
            match *selected_section {
                "syntax" => signature(&mut output, selected_signature(source, name), 4),
                "description" => prose(&mut output, description, 4),
                "options" => prose(&mut output, command_options(name, language), 4),
                "types" => prose(&mut output, argument_types(name), 4),
                "examples" => {
                    if matches!(name, "HELLO" | "AUTH") {
                        prose(&mut output, label("protocol_example", language), 4);
                    } else if name == "ACK" {
                        prose(&mut output, label("ack_example", language), 4);
                    } else if name != "SUBSCRIBE" {
                        prose(&mut output, label("sequence_note", language), 4);
                        if name == "CONFIG RELOAD" {
                            prose(&mut output, label("config_example", language), 4);
                        }
                    }
                    example(&mut output, sample, 4);
                }
                "related" => {
                    let related: Vec<_> = include_str!("command-layout.tsv")
                        .lines()
                        .filter_map(|line| {
                            let fields: Vec<_> = line.split('\t').collect();
                            (fields[1] == group && fields[2] == subgroup && fields[0] != name)
                                .then_some(fields[0])
                        })
                        .collect();
                    if related.is_empty() {
                        example(&mut output, "HELP SET\\nHELP SCAN", 4);
                    } else {
                        prose(&mut output, &related.join(", "), 4);
                    }
                }
                // 章节在上方通过 SECTIONS 验证；默认分支也只枚举同一静态列表。
                // Sections were validated against SECTIONS above; the default branch also enumerates that same static list.
                _ => unreachable!("known help section"),
            }
        }
    }
    if section.is_none() {
        heading(&mut output, "sections", language, 0);
        prose(&mut output, label("section_note", language), 2);
        prose(&mut output, &SECTIONS.join(" | "), 2);
        example(&mut output, &format!("HELP {topic} examples"), 2);
        heading(&mut output, "more", language, 0);
        example(&mut output, "docs/commands.md\\ndocs/cli.md", 2);
    }
    Some(output)
}

/// 有可选参数的命令提供本地化边界，其他命令明确没有可选参数。
/// Commands with optional arguments provide localized boundaries; others explicitly have none.
fn command_options(name: &str, language: Language) -> &'static str {
    include_str!("command-options.tsv")
        .lines()
        .find_map(|line| {
            let mut cells = line.split('\t');
            cells
                .next()?
                .split(',')
                .any(|key| key == name)
                .then(|| cells.nth(language as usize))
                .flatten()
        })
        .unwrap_or_else(|| label("no_options", language))
}

/// 所有帮助共享的语义提示也按主组和子组拆分。
/// Shared semantic notes also use main groups and subgroups.
fn notes(output: &mut String, language: Language) {
    heading(output, "notes", language, 0);
    heading(output, "values", language, 2);
    prose(output, label("value_note", language), 4);
    example(
        output,
        "null  true  int(-1)  uint(1)  float(1.5)  \"text\"\\nbytes(0x01)  list([])  map({})  set([])",
        4,
    );
    heading(output, "semantics", language, 2);
    for key in ["boundary_note", "expiry_note", "event_note"] {
        prose(output, label(key, language), 4);
        output.push('\n');
    }
    heading(output, "more", language, 2);
    example(
        output,
        "https://921108257.github.io/ling/\\ndocs/commands.md\\ndocs/cli.md",
        4,
    );
}
