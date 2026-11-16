//! 本地帮助语言、启动选项和命令目录；不依赖网络或数据库引擎。
//! Local help languages, startup options and command reference, independent of networking and the engine.

use clap::{Arg, ArgAction, Command};
mod diagnostics;
mod layout;
pub use diagnostics::{Context, Diagnostic, argument_diagnostic};

/// 先识别语言覆盖再解析本地化参数，命令字符串不会被当作启动选项扫描。
/// Probes the language override before localized parsing without scanning command-string contents as flags.
pub fn parse_localized<T: clap::Parser>(server: bool) -> (T, Language) {
    let arguments: Vec<_> = std::env::args_os().collect();
    // 跳过已知选项的完整参数值，错误选项不截断后续语言选择。
    // Skip complete values of known options; invalid options do not hide a later locale override.
    let definition = T::command();
    let mut values = arguments.iter().skip(1);
    let mut explicit = None;
    while let Some(value) = values.next() {
        let Some(value) = value.to_str() else {
            continue;
        };
        if value == "--" {
            break;
        }
        if let Some(locale) = value.strip_prefix("--lang=") {
            explicit = Some(locale.to_owned());
        } else if value == "--lang" {
            explicit = values
                .next()
                .and_then(|value| value.to_str())
                .map(str::to_owned);
        } else if !value.contains('=')
            && definition.get_arguments().any(|argument| {
                argument.get_action().takes_values()
                    && (argument
                        .get_long()
                        .is_some_and(|name| value.strip_prefix("--") == Some(name))
                        || argument
                            .get_short()
                            .is_some_and(|name| value == format!("-{name}")))
            })
        {
            values.next();
        }
    }
    let language = Language::detect(explicit.as_deref());
    let matches = localize(T::command(), language, server)
        .try_get_matches_from(arguments)
        .unwrap_or_else(|error| {
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                error.exit();
            }
            eprintln!("{}", argument_diagnostic(language, &error, server));
            std::process::exit(2);
        });
    let parsed = T::from_arg_matches(&matches).unwrap_or_else(|error| {
        eprintln!("{}", argument_diagnostic(language, &error, server));
        std::process::exit(2);
    });
    (parsed, language)
}

/// 七种帮助语言；不改变命令、类型或协议。
/// Seven help languages; command names, types and the protocol remain unchanged.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Language {
    /// 默认美式英语；Default US English.
    #[default]
    English,
    /// 简体中文；Simplified Chinese.
    Chinese,
    /// 韩语；Korean.
    Korean,
    /// 日语；Japanese.
    Japanese,
    /// 俄语；Russian.
    Russian,
    /// 德语；German.
    German,
    /// 法语；French.
    French,
}

impl Language {
    /// 按地区/语言标签匹配，不支持的语言与繁体中文回退英语。
    /// Matches locale tags; unsupported languages and Traditional Chinese fall back to English.
    pub fn from_locale(locale: &str) -> Self {
        let locale = locale.trim().replace('_', "-").to_ascii_lowercase();
        let locale = locale.split(['.', '@']).next().unwrap_or_default();
        let parts: Vec<_> = locale.split('-').collect();
        match parts[0] {
            "zh" if !parts
                .iter()
                .any(|p| matches!(*p, "tw" | "hk" | "mo" | "hant")) =>
            {
                Self::Chinese
            }
            "ko" => Self::Korean,
            "ja" => Self::Japanese,
            "ru" => Self::Russian,
            "de" => Self::German,
            "fr" => Self::French,
            _ => Self::English,
        }
    }

    /// 纯函数选择：显式参数、LING_LANG、LC_ALL、LC_MESSAGES、LANG、本机语言。
    /// Pure selection: explicit option, LING_LANG, LC_ALL, LC_MESSAGES, LANG, then native locale.
    /// 空字符串不覆盖低优先级来源，未知的非空标签直接回退英语。
    /// Empty strings do not override lower-priority sources; unknown nonempty tags fall back to English.
    pub fn select(
        explicit: Option<&str>,
        environment: [Option<&str>; 4],
        native: Option<&str>,
    ) -> Self {
        explicit
            .into_iter()
            .chain(environment.into_iter().flatten())
            .chain(native)
            .find(|s| !s.trim().is_empty())
            .map(Self::from_locale)
            .unwrap_or_default()
    }

    /// 读取帮助语言覆盖与系统语言，不修改进程环境。
    /// Reads help-language overrides and the system locale without modifying the process environment.
    pub fn detect(explicit: Option<&str>) -> Self {
        let environment =
            ["LING_LANG", "LC_ALL", "LC_MESSAGES", "LANG"].map(|name| std::env::var(name).ok());
        let native = sys_locale::get_locale();
        Self::select(
            explicit,
            environment.each_ref().map(|value| value.as_deref()),
            native.as_deref(),
        )
    }

    /// 返回稳定的语言标签；Returns the stable language tag.
    pub fn code(self) -> &'static str {
        [
            "en-US", "zh-CN", "ko-KR", "ja-JP", "ru-RU", "de-DE", "fr-FR",
        ][self as usize]
    }

    /// 按语言选择静态译文，顺序与枚举一致。
    /// Selects static text in the same order as the enum variants.
    fn text(self, alternatives: [&'static str; 7]) -> &'static str {
        alternatives[self as usize]
    }
}

/// 为启动参数添加本地帮助文案，不改变参数名、默认值与校验。
/// Localizes startup help without changing argument names, defaults or validation.
/// `server` 为 true 时展示服务端介绍，否则展示 CLI 介绍。
/// `server` selects the server introduction instead of the CLI introduction.
pub fn localize(command: Command, language: Language, server: bool) -> Command {
    let about = if server {
        language.text([
            "Ling — standalone typed memory cache database (preview)",
            "Ling — 独立部署的强类型内存缓存数据库（预览版）",
            "Ling — 독립 실행형 강타입 메모리 캐시 데이터베이스 (미리 보기)",
            "Ling — 独立した強い型付けのメモリキャッシュデータベース（プレビュー版）",
            "Ling — автономная типизированная база данных в памяти (предварительная версия)",
            "Ling — eigenständige typisierte Cache-Datenbank im Arbeitsspeicher (Vorschau)",
            "Ling — base de données de cache en mémoire à typage strict (préversion)",
        ])
    } else {
        language.text([
            "Ling command-line client",
            "Ling 命令行客户端",
            "Ling 명령줄 클라이언트",
            "Ling コマンドラインクライアント",
            "Клиент командной строки Ling",
            "Ling Kommandozeilenclient",
            "Client en ligne de commande Ling",
        ])
    };
    let command = command
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help),
        )
        .arg(
            Arg::new("version")
                .short('V')
                .long("version")
                .action(ArgAction::Version),
        )
        .about(about)
        .long_about(about)
        .mut_args(|arg| {
            if let Some(description) = flag_help(arg.get_id().as_str(), language) {
                arg.help(description).long_help(description)
            } else {
                arg
            }
        });
    let rendered = layout::startup(&command, language, server);
    command.override_help(rendered)
}
/// 在共享表中查找启动选项译文。
/// Looks up startup-option translations in the shared table.
fn flag_help(id: &str, language: Language) -> Option<&'static str> {
    include_str!("flags.tsv").lines().find_map(|line| {
        let mut columns = line.split('\t');
        (columns.next()? == id)
            .then(|| columns.nth(language as usize))
            .flatten()
    })
}

/// 交互式入口提示；Interactive entry-point hint.
pub fn interactive_hint(language: Language) -> &'static str {
    language.text([
        "Type HELP for the command reference, HELP SET for details, or QUIT to exit.\nLocal help works offline. Use --lang to choose the help language.",
        "输入 HELP 查看命令速查，HELP SET 查看详情，QUIT 退出。\n本地帮助无需连接服务；使用 --lang 指定帮助语言。",
        "HELP로 명령 참조, HELP SET으로 상세 정보를 확인하고 QUIT로 종료하세요.\n로컬 도움말은 오프라인에서도 제공됩니다. 언어 선택: --lang",
        "HELP でコマンド早見表、HELP SET で詳細を表示し、QUIT で終了します。\nローカルヘルプはオフラインで利用できます。言語の指定: --lang",
        "Введите HELP для списка команд, HELP SET для подробностей, QUIT для выхода.\nЛокальная справка работает без подключения. Язык: --lang",
        "HELP zeigt die Befehlsübersicht, HELP SET die Details, QUIT beendet den Client.\nDie lokale Hilfe funktioniert offline. Sprache wählen: --lang",
        "Tapez HELP pour la référence, HELP SET pour les détails, QUIT pour quitter.\nL’aide locale fonctionne hors ligne. Choisissez la langue avec --lang.",
    ])
}

/// 未知帮助主题的诊断，不包含输入以避免回显敏感数据。
/// Unknown-topic diagnostic without echoing potentially sensitive input.
pub fn unknown_topic(language: Language) -> &'static str {
    language.text([
        "Unknown help topic. Type HELP for the command reference.",
        "未知帮助主题。输入 HELP 查看命令速查。",
        "알 수 없는 도움말 주제입니다. 명령 참조를 보려면 HELP를 입력하세요.",
        "不明なヘルプ項目です。HELP でコマンド早見表を表示してください。",
        "Неизвестная тема справки. Введите HELP для списка команд.",
        "Unbekanntes Hilfethema. HELP zeigt die Befehlsübersicht.",
        "Rubrique d’aide inconnue. Tapez HELP pour la référence des commandes.",
    ])
}

/// Batch 禁用本地 HELP 的诊断，整批应在发送前拒绝。
/// Diagnostic for local HELP in Batch; reject the whole batch before sending.
pub fn batch_help_error(language: Language) -> &'static str {
    language.text([
        "HELP is local and cannot be used inside --batch; no commands were submitted.",
        "HELP 是本地命令，不能用于 --batch；整批未提交。",
        "HELP는 로컬 명령으로 --batch에서 사용할 수 없습니다. 명령은 제출되지 않았습니다.",
        "HELP はローカルコマンドのため --batch では使用できません。コマンドは送信されていません。",
        "HELP — локальная команда, недоступная в --batch. Команды не отправлены.",
        "HELP ist ein lokaler Befehl und in --batch nicht zulässig. Es wurden keine Befehle gesendet.",
        "HELP est une commande locale indisponible dans --batch. Aucune commande n’a été envoyée.",
    ])
}

/// 从静态目录提取名称、签名与当前语言的说明。
/// Extracts names, signature and the selected description from the static catalog.
fn entries(language: Language) -> impl Iterator<Item = (&'static str, &'static str, &'static str)> {
    include_str!("commands.tsv").lines().map(move |line| {
        let mut columns = line.split('\t');
        let names = columns.next().expect("catalog names");
        let signature = columns.next().expect("catalog signature");
        let description = columns.nth(language as usize).expect("catalog translation");
        (names, signature, description)
    })
}

/// 全部已公开命令的离线速查，不访问网络。
/// Offline reference for every public command, without network access.
pub fn cheatsheet(language: Language) -> String {
    layout::reference(language)
}

/// 按大小写不敏感的命令名查找分组帮助；未知主题返回 None。
/// Looks up grouped help by case-insensitive command name; unknown topics return None.
/// 支持 CONFIG 与其子命令；不在错误中回显输入。
/// Supports CONFIG and its subcommands without echoing input in diagnostics.
pub fn command_help(language: Language, topic: &str) -> Option<String> {
    layout::detail(language, topic)
}
