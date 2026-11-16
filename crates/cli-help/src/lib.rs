//! 本地帮助语言、启动选项和命令目录；不依赖网络或数据库引擎。
//! Local help languages, startup options and command reference, independent of networking and the engine.

use clap::{Arg, ArgAction, Command};

/// 先识别语言覆盖再解析本地化参数，命令字符串不会被当作启动选项扫描。
/// Probes the language override before localized parsing without scanning command-string contents as flags.
pub fn parse_localized<T: clap::Parser>(server: bool) -> (T, Language) {
    let arguments: Vec<_> = std::env::args_os().collect();
    let probe = T::command()
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(
            Arg::new("_help_probe")
                .long("help")
                .short('h')
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("_version_probe")
                .long("version")
                .short('V')
                .action(ArgAction::SetTrue),
        )
        .ignore_errors(true)
        .get_matches_from(arguments.clone());
    let language = Language::detect(
        probe
            .try_get_one::<String>("lang")
            .ok()
            .flatten()
            .map(String::as_str),
    );
    let matches = localize(T::command(), language, server).get_matches_from(arguments);
    let parsed = T::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
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
    let options = language.text([
        "Options",
        "选项",
        "옵션",
        "オプション",
        "Параметры",
        "Optionen",
        "Options",
    ]);
    let template = language.text([
        "{about-with-newline}\nUsage: {usage}\n\n{all-args}{after-help}",
        "{about-with-newline}\n用法：{usage}\n\n{all-args}{after-help}",
        "{about-with-newline}\n사용법: {usage}\n\n{all-args}{after-help}",
        "{about-with-newline}\n使い方：{usage}\n\n{all-args}{after-help}",
        "{about-with-newline}\nИспользование: {usage}\n\n{all-args}{after-help}",
        "{about-with-newline}\nVerwendung: {usage}\n\n{all-args}{after-help}",
        "{about-with-newline}\nUtilisation : {usage}\n\n{all-args}{after-help}",
    ]);
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
            "Ling command-line client. Provide credentials through LING_PASSWORD.",
            "Ling 命令行客户端。通过 LING_PASSWORD 环境变量提供认证密码。",
            "Ling 명령줄 클라이언트. 인증 암호는 LING_PASSWORD로 설정하세요.",
            "Ling コマンドラインクライアント。認証パスワードは LING_PASSWORD で設定します。",
            "Клиент командной строки Ling. Пароль задаётся через LING_PASSWORD.",
            "Ling Kommandozeilenclient. Passwort über LING_PASSWORD angeben.",
            "Client en ligne de commande Ling. Fournissez le mot de passe via LING_PASSWORD.",
        ])
    };
    let footer = if server {
        language.text([
            "Default: 127.0.0.1:6380, Fast events. Cache values and TTL are not restored after restart.\nRun ling-cli --cheatsheet for the command reference.",
            "默认监听 127.0.0.1:6380，使用 Fast 事件。重启不恢复缓存值与 TTL。\n运行 ling-cli --cheatsheet 查看命令速查。",
            "기본값: 127.0.0.1:6380, Fast 이벤트. 재시작 시 캐시 값과 TTL은 복원되지 않습니다.\n명령 참조: ling-cli --cheatsheet",
            "既定値: 127.0.0.1:6380、Fast イベント。再起動後にキャッシュ値と TTL は復元されません。\nコマンド早見表: ling-cli --cheatsheet",
            "По умолчанию: 127.0.0.1:6380, события Fast. Значения и TTL после перезапуска не восстанавливаются.\nСправочник команд: ling-cli --cheatsheet",
            "Standard: 127.0.0.1:6380, Fast-Ereignisse. Cache-Werte und TTL werden nach einem Neustart nicht wiederhergestellt.\nBefehlsübersicht: ling-cli --cheatsheet",
            "Par défaut : 127.0.0.1:6380, événements Fast. Les valeurs et TTL ne sont pas restaurés après redémarrage.\nRéférence des commandes : ling-cli --cheatsheet",
        ])
    } else {
        interactive_hint(language)
    };
    command
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
        .help_template(template)
        .after_help(footer)
        .after_long_help(footer)
        .mut_args(|arg| {
            let description = flag_help(arg.get_id().as_str(), language);
            let arg = if let Some(description) = description {
                arg.help(description).long_help(description)
            } else {
                arg
            };
            arg.help_heading(options)
        })
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

/// 共享的类型与语义提示，显示在速查及单命令帮助后。
/// Shared typed-value and semantic notes shown after reference entries.
fn notes(language: Language) -> &'static str {
    language.text([
        "Values: null, true, false, int(-1), uint(1), float(1.5), \"text\", bytes(0x01), list([]), map({}), set([]).\nValue arguments require explicit types; fixed indices/counts/TTL/database numbers use decimal integers.\nMissing keys are errors except where specified. Typed mutations do not create missing keys.\nTTL/PTTL/EXPIRETIME/PEXPIRETIME: -1 = no expiry, -2 = missing. SCAN cursors require the same connection and filters.\nBatch is not a transaction. Fast may drop events; Reliable restores retained committed events, not cached values or TTL.\nSUBSCRIBE uses -c; ACK is explicit by default. A timeout after sending may mean the outcome is unknown.\nFull contracts: docs/commands.md; CLI guide: docs/cli.md.",
        "Value 示例：null、true、false、int(-1)、uint(1)、float(1.5)、\"text\"、bytes(0x01)、list([])、map({})、set([])。\nValue 参数必须显式标注类型；固定索引、计数、TTL 与库号使用十进制整数。\n除另有说明外，Key 缺失报错；类型专用写命令不自动创建 Key。\nTTL/PTTL/EXPIRETIME/PEXPIRETIME：-1 为无期限，-2 为缺失；SCAN 游标须使用同一连接与筛选条件。\nBatch 不是事务。Fast 可能丢事件；Reliable 仅恢复保留期内已提交事件，不恢复缓存值与 TTL。\n使用 -c 进入 SUBSCRIBE；默认显式 ACK。发送后超时可能表示结果未知。\n完整契约：docs/commands.md；CLI 指南：docs/cli.md。",
        "값 예: null, true, false, int(-1), uint(1), float(1.5), \"text\", bytes(0x01), list([]), map({}), set([]).\n값에는 명시적 타입이 필요합니다. 인덱스, 개수, TTL, 데이터베이스 번호는 십진 정수입니다.\n별도 명시가 없으면 키 누락은 오류이며 타입별 변경 명령은 키를 자동 생성하지 않습니다.\nTTL/PTTL/EXPIRETIME/PEXPIRETIME: -1 = 만료 없음, -2 = 키 없음. SCAN은 같은 연결과 필터를 유지해야 합니다.\nBatch는 트랜잭션이 아닙니다. Fast는 이벤트를 잃을 수 있습니다. Reliable은 보존 기간 내 커밋된 이벤트만 복구하며 값과 TTL은 복구하지 않습니다.\nSUBSCRIBE는 -c로 실행하며 ACK는 기본적으로 명시적입니다. 전송 후 시간 초과 시 결과가 불명확할 수 있습니다.\n전체 계약: docs/commands.md; CLI 안내: docs/cli.md.",
        "値の例: null, true, false, int(-1), uint(1), float(1.5), \"text\", bytes(0x01), list([]), map({}), set([])。\n値には明示的な型が必要です。固定の添字、件数、TTL、DB 番号は十進整数です。\n明記された場合を除き、キーの欠落はエラーです。型別の更新命令はキーを自動作成しません。\nTTL/PTTL/EXPIRETIME/PEXPIRETIME: -1 = 期限なし、-2 = キーなし。SCAN は同一接続とフィルターを維持してください。\nBatch はトランザクションではありません。Fast はイベントを失う可能性があります。Reliable は保持期間内のコミット済みイベントを復元しますが、値と TTL は復元しません。\nSUBSCRIBE は -c で実行し、既定では明示的に ACK します。送信後のタイムアウトでは結果が不明な場合があります。\n完全な仕様: docs/commands.md、CLI ガイド: docs/cli.md。",
        "Значения: null, true, false, int(-1), uint(1), float(1.5), \"text\", bytes(0x01), list([]), map({}), set([]).\nДля значений нужны явные типы; индексы, количества, TTL и номера БД — десятичные целые.\nОтсутствующий ключ обычно является ошибкой; типизированные изменения не создают ключи автоматически.\nTTL/PTTL/EXPIRETIME/PEXPIRETIME: -1 = без срока, -2 = ключ отсутствует. SCAN требует того же соединения и фильтров.\nBatch не является транзакцией. Fast может терять события. Reliable восстанавливает сохранённые подтверждённые события, но не значения и TTL.\nSUBSCRIBE запускается через -c; ACK по умолчанию явный. Тайм-аут после отправки может означать неизвестный результат.\nПолные условия: docs/commands.md; руководство CLI: docs/cli.md.",
        "Werte: null, true, false, int(-1), uint(1), float(1.5), \"text\", bytes(0x01), list([]), map({}), set([]).\nWerte benötigen explizite Typen; feste Indizes, Anzahlen, TTL und Datenbanknummern sind dezimale Ganzzahlen.\nFehlende Schlüssel sind normalerweise Fehler; typisierte Änderungen erzeugen keine Schlüssel.\nTTL/PTTL/EXPIRETIME/PEXPIRETIME: -1 = keine Frist, -2 = fehlt. SCAN benötigt dieselbe Verbindung und dieselben Filter.\nBatch ist keine Transaktion. Fast kann Ereignisse verlieren. Reliable stellt aufbewahrte bestätigte Ereignisse wieder her, keine Cache-Werte oder TTL.\nSUBSCRIBE wird mit -c gestartet; ACK erfolgt standardmäßig explizit. Ein Timeout nach dem Senden kann ein unbekanntes Ergebnis bedeuten.\nVollständige Verträge: docs/commands.md; CLI-Anleitung: docs/cli.md.",
        "Valeurs : null, true, false, int(-1), uint(1), float(1.5), \"text\", bytes(0x01), list([]), map({}), set([]).\nLes valeurs exigent un type explicite ; indices, nombres, TTL et numéros de base sont des entiers décimaux.\nUne clé absente est normalement une erreur ; les mutations typées ne créent pas de clé.\nTTL/PTTL/EXPIRETIME/PEXPIRETIME : -1 = sans expiration, -2 = absente. SCAN exige la même connexion et les mêmes filtres.\nBatch n’est pas une transaction. Fast peut perdre des événements. Reliable restaure les événements validés encore conservés, pas les valeurs ni les TTL.\nSUBSCRIBE utilise -c ; ACK est explicite par défaut. Un délai dépassé après envoi peut laisser le résultat inconnu.\nContrats complets : docs/commands.md ; guide CLI : docs/cli.md.",
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
    use std::fmt::Write;
    let mut output = language
        .text([
            "Ling command reference",
            "Ling 命令速查",
            "Ling 명령 참조",
            "Ling コマンド早見表",
            "Справочник команд Ling",
            "Ling Befehlsübersicht",
            "Référence des commandes Ling",
        ])
        .to_owned();
    output.push_str("\n\n");
    output.push_str(interactive_hint(language));
    output.push_str("\n\n");
    for (_, signature, description) in entries(language) {
        writeln!(output, "{signature}\n  {description}").expect("String formatting");
    }
    output.push('\n');
    output.push_str(notes(language));
    output.push('\n');
    output
}

/// 按大小写不敏感的命令名查找离线帮助；未知主题返回 None。
/// Looks up offline help by case-insensitive command name; unknown topics return None.
/// 支持 CONFIG 与其子命令；不在错误中回显输入。
/// Supports CONFIG and its subcommands without echoing input in diagnostics.
pub fn command_help(language: Language, topic: &str) -> Option<String> {
    use std::fmt::Write;
    let topic = topic
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_uppercase();
    let mut output = String::new();
    for (names, signature, description) in entries(language) {
        if names.split(',').any(|name| name == topic)
            || (topic == "CONFIG" && names.starts_with("CONFIG "))
        {
            writeln!(output, "{signature}\n  {description}\n").expect("String formatting");
        }
    }
    if output.is_empty() {
        return None;
    }
    output.push_str(notes(language));
    output.push('\n');
    Some(output)
}
