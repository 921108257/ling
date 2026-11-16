//! 帮助分组、命令详情与七语言目录完整性契约。
//! Contracts for grouped help, command details and complete references in seven locales.

use clap::{Arg, ArgAction, Command};
use ling_cli_help::{Language, cheatsheet, command_help, localize};
use std::collections::BTreeSet;

/// 七语言运行相同版式契约，防止单一语言有层级而其余退化。
/// Applies the same layout contracts to every locale, preventing partial localization.
const LANGUAGES: [Language; 7] = [
    Language::English,
    Language::Chinese,
    Language::Korean,
    Language::Japanese,
    Language::Russian,
    Language::German,
    Language::French,
];

/// 选项使用真实 ID 与动作，布局不得掩盖原有参数校验。
/// Uses real option IDs and actions; layout must not conceal argument validation.
fn cli() -> Command {
    let mut command = Command::new("ling-cli").version("0.1.0");
    for (id, long) in [
        ("host", "host"),
        ("port", "port"),
        ("db", "db"),
        ("timeout_ms", "timeout-ms"),
        ("command", "command"),
        ("count", "count"),
        ("tls_server_name", "tls-server-name"),
        ("tls_ca", "tls-ca"),
        ("tls_cert", "tls-cert"),
        ("tls_key", "tls-key"),
        ("lang", "lang"),
    ] {
        command = command.arg(Arg::new(id).long(long));
    }
    for (id, long) in [
        ("auto_ack", "auto-ack"),
        ("batch", "batch"),
        ("cheatsheet", "cheatsheet"),
    ] {
        command = command.arg(Arg::new(id).long(long).action(ArgAction::SetTrue));
    }
    command
}

/// 主组、子组和逐条选项分开，纯文本输出无需 ANSI 色彩。
/// Separates main groups, subgroups and options without relying on ANSI colors.
#[test]
fn startup_help_has_two_levels_and_spacing_in_every_language() {
    let groups = [
        (
            "Connection",
            "Address",
            "Security",
            "Execution",
            "Local reference",
        ),
        ("连接", "地址", "安全", "执行", "本地参考"),
        ("연결", "주소", "보안", "실행", "로컬 참조"),
        ("接続", "接続先", "セキュリティ", "実行", "ローカル参照"),
        (
            "Подключение",
            "Адрес",
            "Безопасность",
            "Выполнение",
            "Локальная справка",
        ),
        (
            "Verbindung",
            "Adresse",
            "Sicherheit",
            "Ausführung",
            "Lokale Referenz",
        ),
        (
            "Connexion",
            "Adresse",
            "Sécurité",
            "Exécution",
            "Référence locale",
        ),
    ];
    for (language, (connection, address, security, execution, reference)) in
        LANGUAGES.into_iter().zip(groups)
    {
        let help = localize(cli(), language, false)
            .render_long_help()
            .to_string();
        assert!(help.contains(&format!("\n\n{connection}\n\n  {address}\n")));
        for group in [security, execution, reference] {
            assert!(help.contains(&format!("\n{group}\n\n  ")));
        }
        assert!(help.contains("    --host <IP>\n      "));
        assert!(help.contains("LING_PASSWORD"));
        assert!(help.contains("ling-cli -c 'PING'"));
        assert!(help.contains("ling-cli help SET examples"));
        assert!(!help.contains('\u{1b}'));
        assert!(!help.lines().any(|line| line.chars().count() > 72));
    }
}

/// 独立服务端帮助同样分出配置、语言、帮助与示例。
/// Standalone server help also separates configuration, language, help and examples.
#[test]
fn server_help_has_subgroups_and_real_startup_examples() {
    for language in LANGUAGES {
        let command = Command::new("ling")
            .version("0.1.0")
            .arg(Arg::new("config").long("config"))
            .arg(Arg::new("lang").long("lang"));
        let help = localize(command, language, true)
            .render_long_help()
            .to_string();
        assert!(help.contains("    --config <FILE>\n      "));
        assert!(help.contains("    --lang <LOCALE>\n      "));
        assert!(help.contains("ling --config /path/to/ling.toml"));
        assert!(help.contains("ling help update"));
        assert!(!help.contains("--tls-server-name"));
    }
}

/// 目录中每个命令恰有一个分类和示例，不会因分组被丢弃。
/// Every catalog command has one classification and example, so grouping cannot omit commands.
#[test]
fn layout_metadata_covers_the_whole_command_catalog() {
    let names: BTreeSet<_> = include_str!("../src/commands.tsv")
        .lines()
        .flat_map(|line| line.split('\t').next().unwrap().split(','))
        .collect();
    let mut layouts = BTreeSet::new();
    for line in include_str!("../src/command-layout.tsv").lines() {
        let columns: Vec<_> = line.split('\t').collect();
        assert_eq!(columns.len(), 4);
        assert!(columns.iter().all(|column| !column.is_empty()));
        assert!(layouts.insert(columns[0]), "duplicate {}", columns[0]);
    }
    assert_eq!(layouts, names);
    for line in include_str!("../src/labels.tsv").lines() {
        let columns: Vec<_> = line.split('\t').collect();
        assert_eq!(columns.len(), 8);
        assert!(columns.iter().all(|column| !column.is_empty()));
    }
}

/// 所有详情包含独立内容区、初始化示例和相关命令。
/// Details include separate content sections, initialization examples and related commands.
#[test]
fn command_details_keep_types_examples_and_related_commands() {
    let labels = [
        (
            "Syntax",
            "Description",
            "Types and boundaries",
            "Examples",
            "Related commands",
        ),
        ("语法", "说明", "类型与边界", "示例", "相关命令"),
        ("문법", "설명", "타입 및 경계", "예제", "관련 명령"),
        ("構文", "説明", "型と制約", "使用例", "関連コマンド"),
        (
            "Синтаксис",
            "Описание",
            "Типы и границы",
            "Примеры",
            "Связанные команды",
        ),
        (
            "Syntax",
            "Beschreibung",
            "Typen und Grenzen",
            "Beispiele",
            "Verwandte Befehle",
        ),
        (
            "Syntaxe",
            "Description",
            "Types et limites",
            "Exemples",
            "Commandes associées",
        ),
    ];
    for (language, labels) in LANGUAGES.into_iter().zip(labels) {
        let help = command_help(language, "LPUSH").unwrap();
        for heading in [labels.0, labels.1, labels.2, labels.3, labels.4] {
            assert!(help.contains(&format!("\n\n  {heading}\n")));
        }
        assert!(help.contains("SET queue list([])"));
        assert!(help.contains("LPUSH queue \"first\" int(2)"));
        assert!(help.contains("value: List; item: Value"));
        assert!(!help.contains("RPUSH key value"));
        assert!(help.contains("RPUSH, LPOP"));
        assert!(help.contains("docs/cli.md"));
        let reference = cheatsheet(language);
        assert!(reference.contains("    LPOP key [count]\n    RPOP key [count]"));
        assert!(reference.contains("    SET key value [EX seconds | PX ms | EXAT unix_sec | PXAT"));
        assert!(!reference.contains('\u{1b}'));
    }
}

/// HELP 逐层展开，目录不会提前输出全部命令的签名和说明。
/// HELP expands one level at a time without printing every command signature and description.
#[test]
fn directory_and_group_paths_are_compact_in_every_language() {
    for language in LANGUAGES {
        let directory = command_help(language, "").unwrap();
        assert!(directory.lines().count() < 30);
        assert!(directory.contains("HELP containers"));
        assert!(!directory.contains("SET key value"));
        assert!(!directory.contains("LPOP"));
        assert!(directory.contains("ling-cli --cheatsheet"));
        for (group, subgroups) in [
            ("reference", &["lookup"][..]),
            ("data", &["readwrite", "identity", "transfer", "scan"][..]),
            ("expiry", &["setexpiry", "queryexpiry"][..]),
            ("scalar", &["numbers", "text"][..]),
            ("containers", &["list", "map", "set"][..]),
            ("administration", &["session", "database", "operations"][..]),
            ("events", &["consume"][..]),
        ] {
            let help = command_help(language, group).unwrap();
            for subgroup in subgroups {
                assert!(help.contains(&format!("HELP {group} {subgroup}")));
                assert!(command_help(language, &format!("{group} {subgroup}")).is_some());
            }
        }
        let list = command_help(language, "containers list").unwrap();
        assert!(list.contains("LPUSH, RPUSH"));
        assert!(!list.contains("FGET"));
        assert!(!list.contains("LPUSH key value"));
        assert!(
            command_help(language, "containers set")
                .unwrap()
                .contains("SADD")
        );
        assert!(command_help(language, "set").unwrap().starts_with("SET\n"));
        assert_eq!(
            command_help(language, "DATA READWRITE"),
            command_help(language, "data readwrite")
        );
        for unknown in ["containers unknown", "containers list extra", "data syntax"] {
            assert!(command_help(language, unknown).is_none());
        }
    }
}

/// 章节查询只显示所选章节，并保留多词命令名和可选参数边界。
/// Focused help shows only one section while retaining multiword command names and option boundaries.
#[test]
fn focused_sections_exclude_unrequested_content_in_every_language() {
    for language in LANGUAGES {
        let syntax = command_help(language, "SET syntax").unwrap();
        assert!(syntax.contains("SET key value"));
        assert!(!syntax.contains("SET session null"));
        assert!(!syntax.contains("docs/cli.md"));
        let options = command_help(language, "SET options").unwrap();
        let option_words = options.split_whitespace().collect::<Vec<_>>().join(" ");
        for option in [
            "EX seconds",
            "PX ms",
            "EXAT unix_sec",
            "PXAT unix_ms",
            "NX",
            "XX",
        ] {
            assert!(
                option_words.contains(option),
                "{}: {option}",
                language.code()
            );
        }
        assert!(!options.contains("SET key value"));
        assert!(!options.contains("SET session null"));
        let examples = command_help(language, "SET examples").unwrap();
        assert!(examples.contains("SET session null PX 5000 NX"));
        assert!(!examples.contains("key: String"));
        let related = command_help(language, "SET related").unwrap();
        assert!(related.contains("GET, GETSET"));
        assert!(!related.contains("SET session null"));
        assert!(
            command_help(language, "CONFIG GET syntax")
                .unwrap()
                .contains("CONFIG GET pattern")
        );
        assert!(
            command_help(language, "CONFIG examples")
                .unwrap()
                .contains("CONFIG RELOAD")
        );
        for name in ["SET", "GET", "HELP", "SCAN", "CONFIG GET"] {
            for section in [
                "syntax",
                "description",
                "options",
                "types",
                "examples",
                "related",
            ] {
                assert!(command_help(language, &format!("{name} {section}")).is_some());
            }
        }
        assert!(command_help(language, "SET unknown-section").is_none());
    }
}

/// 服务更新子命令使用相同的七语帮助，不泄漏 clap 默认英语布局。
/// Service update subcommands use the same seven-language help instead of the default clap layout.
#[test]
fn explicit_update_subcommand_is_localized_recursively() {
    let mut translations = BTreeSet::new();
    for language in LANGUAGES {
        let command = Command::new("ling")
            .version("0.1.3")
            .disable_help_subcommand(true)
            .subcommand(
                Command::new("update")
                    .arg(Arg::new("check").long("check").action(ArgAction::SetTrue))
                    .arg(Arg::new("target_version").long("version"))
                    .arg(Arg::new("stable").long("stable").action(ArgAction::SetTrue)),
            );
        let mut localized = localize(command, language, true);
        let direct = localized
            .find_subcommand_mut("update")
            .unwrap()
            .render_help()
            .to_string();
        let error = localized
            .try_get_matches_from(["ling", "update", "--help"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        assert_eq!(direct, error.to_string());
        for item in [
            "ling update [OPTIONS]",
            "--check",
            "--version <VERSION>",
            "--stable",
        ] {
            assert!(direct.contains(item));
        }
        assert!(translations.insert(direct));
    }
}
