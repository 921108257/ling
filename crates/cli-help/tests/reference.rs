//! 语言优先级和离线目录契约，不修改全局环境。
//! Language-priority and offline-reference contracts without modifying the global environment.

use clap::{Arg, Command};
use ling_cli_help::{Language, cheatsheet, command_help, localize};
use std::collections::BTreeSet;

/// 系统常见标签、编码后缀和繁体中文回退。
/// Common system tags, encoding suffixes and Traditional Chinese fallback.
#[test]
fn locale_tags_and_fallback() {
    for (tag, expected) in [
        ("en_US.UTF-8", Language::English),
        ("zh_CN.UTF-8", Language::Chinese),
        ("zh-Hans-CN", Language::Chinese),
        ("zh", Language::Chinese),
        ("ZH_hans", Language::Chinese),
        ("zh-TW", Language::English),
        ("zh-Hant-CN", Language::English),
        ("zh_HK.UTF-8", Language::English),
        ("ko_KR", Language::Korean),
        ("ja-JP", Language::Japanese),
        ("ru_RU.UTF-8", Language::Russian),
        ("de_DE@euro", Language::German),
        ("fr_CA.UTF-8", Language::French),
        ("C.UTF-8", Language::English),
        ("POSIX", Language::English),
        ("es-ES", Language::English),
        ("", Language::English),
    ] {
        assert_eq!(Language::from_locale(tag), expected, "{tag}");
    }
}

/// 首个非空来源生效，即使该标签未支持也不越过显式选择。
/// The first nonempty source wins, including unsupported explicit choices.
#[test]
fn language_priority_is_deterministic() {
    let environment = [Some("ru-RU"), Some("zh-CN"), Some("ko-KR"), Some("de-DE")];
    assert_eq!(
        Language::select(Some("ja-JP"), environment, Some("fr-FR")),
        Language::Japanese
    );
    assert_eq!(
        Language::select(None, environment, Some("fr-FR")),
        Language::Russian
    );
    assert_eq!(
        Language::select(Some("es-ES"), environment, Some("fr-FR")),
        Language::English
    );
    assert_eq!(
        Language::select(None, [Some(" "), Some("zh-CN"), None, None], None),
        Language::Chinese
    );
    assert_eq!(
        Language::select(None, [None, None, Some("ko-KR"), Some("de-DE")], None),
        Language::Korean
    );
    assert_eq!(
        Language::select(None, [None, None, None, Some("de-DE")], None),
        Language::German
    );
    assert_eq!(
        Language::select(None, [None; 4], Some("fr-FR")),
        Language::French
    );
    assert_eq!(Language::select(None, [None; 4], None), Language::English);
}

/// 七种语言均有完整译文；每个公开命令有签名与说明。
/// All seven languages have complete translations and every public command has a signature and description.
#[test]
fn all_languages_cover_public_commands() {
    let commands = "HELP HELLO AUTH SELECT PING QUIT SET GET GETSET MGET MSET DEL EXISTS TYPE DBSIZE EXPIRE PEXPIRE EXPIREAT PEXPIREAT PERSIST TTL PTTL EXPIRETIME PEXPIRETIME RENAME RENAMENX COPY MOVE KEYS SCAN RANDOMKEY FLUSHDB FLUSHALL SWAPDB INFO SHUTDOWN SUBSCRIBE UNSUBSCRIBE ACK INCR DECR INCRBY DECRBY INCRFLOAT APPEND STRLEN BLEN LPUSH RPUSH LPOP RPOP LRANGE LINDEX LSET LLEN LREM LTRIM FSET FSETNX FGET FDEL FEXISTS FKEYS FVALS FGETALL FLEN FMERGE SADD SREM SISMEMBER SMISMEMBER SMEMBERS SCARD SUNION SINTER SDIFF SUNIONSTORE SINTERSTORE SDIFFSTORE SRANDMEMBER SPOP";
    let languages = [
        Language::English,
        Language::Chinese,
        Language::Korean,
        Language::Japanese,
        Language::Russian,
        Language::German,
        Language::French,
    ];
    let titles: BTreeSet<_> = languages
        .map(|language| cheatsheet(language).lines().next().unwrap().to_owned())
        .into_iter()
        .collect();
    assert_eq!(titles.len(), languages.len());
    for language in languages {
        for name in commands.split_whitespace().chain([
            "CONFIG",
            "CONFIG GET",
            "CONFIG SET",
            "CONFIG RELOAD",
        ]) {
            let help = command_help(language, name)
                .unwrap_or_else(|| panic!("{}: {name}", language.code()));
            assert!(help.contains(name));
            assert!(help.contains("docs/commands.md"));
        }
        assert!(command_help(language, "SAVE").is_none());
        assert!(command_help(language, "").is_some());
        assert_eq!(
            command_help(language, "  config   get "),
            command_help(language, "CONFIG GET")
        );
        assert!(cheatsheet(language).contains("INCRFLOAT key float(delta)"));
    }
}

/// 静态表的列数、名称唯一性和非空译文防止漏译和错位。
/// Static-table column counts, unique names and nonempty translations prevent omissions and column shifts.
#[test]
fn translation_tables_are_complete() {
    for (catalog, expected) in [
        (include_str!("../src/commands.tsv"), 9),
        (include_str!("../src/flags.tsv"), 8),
        (include_str!("../src/labels.tsv"), 8),
        (include_str!("../src/command-layout.tsv"), 4),
        (include_str!("../src/command-options.tsv"), 8),
    ] {
        let mut names = BTreeSet::new();
        for line in catalog.lines() {
            let columns: Vec<_> = line.split('\t').collect();
            assert_eq!(columns.len(), expected, "{line}");
            assert!(columns.iter().all(|value| !value.trim().is_empty()));
            for name in columns[0].split(',') {
                assert!(names.insert(name), "duplicate {name}");
            }
        }
    }
}

/// 本地化仅改变帮助，参数约束和选项名仍由 clap 执行。
/// Localization changes only help while clap still enforces argument names and validation.
#[test]
fn localized_clap_help_and_validation() {
    for (tag, marker) in [
        ("en-US", "Usage:"),
        ("zh-CN", "用法："),
        ("ko-KR", "사용법:"),
        ("ja-JP", "使い方："),
        ("ru-RU", "Использование:"),
        ("de-DE", "Verwendung:"),
        ("fr-FR", "Utilisation :"),
    ] {
        let command = Command::new("ling")
            .version("0.1.0")
            .arg(Arg::new("config").long("config"))
            .arg(Arg::new("lang").long("lang"));
        let command = localize(command, Language::from_locale(tag), true);
        let help = command
            .clone()
            .try_get_matches_from(["ling", "--help"])
            .unwrap_err();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(help.to_string().contains(marker), "{tag}: {help}");
        assert!(help.to_string().contains("--config"));
        assert!(
            command
                .clone()
                .try_get_matches_from(["ling", "--config"])
                .is_err()
        );
        let version = command
            .try_get_matches_from(["ling", "--version"])
            .unwrap_err();
        assert_eq!(version.kind(), clap::error::ErrorKind::DisplayVersion);
    }
}
