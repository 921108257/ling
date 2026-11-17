//! 同步、带校验和的原子记录；文件内容与目录项分别同步。
//! Synchronous checksummed atomic records; file contents and directory entries are synced separately.

use crate::{
    Config,
    codec::{self, Decoder, Encoder},
    err,
};
use fs2::FileExt;
use ling_types::{DbError, ErrorCode};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

/// 固定格式魔数；Fixed format magic.
const MAGIC: &[u8; 8] = b"LGJRNL01";
/// 完整提交尾标；Complete commit trailer.
const COMMIT: &[u8; 8] = b"LGCMT001";
/// 文件头字节；Header bytes.
const HEADER: usize = 40;
/// 目录独占权与有效日志句柄共同存活。
/// Directory ownership lives for the lifetime of the active journal handle.
pub(crate) struct Journal {
    /// 操作系统锁；Operating-system lock.
    _lock: File,
    /// 当前文件；Current file.
    file: File,
    /// 独占目录；Exclusive directory.
    dir: PathBuf,
    /// 冻结头；Frozen header.
    header: Vec<u8>,
    /// 已提交字节；Committed bytes.
    pub bytes: usize,
    /// 整个目录限额，含替换预留；Directory limit including replacement reserve.
    max: usize,
    /// 不确定写入后禁止追加；No appends after an uncertain write.
    pub failed: bool,
    /// 启动内存预算同时约束恢复分配；Startup memory budget also bounds recovery allocations.
    record_limit: usize,
    /// 测试在完整写入后模拟同步失败；Tests simulate sync failure after a complete write.
    #[cfg(test)]
    pub fail_sync: bool,
}
impl Journal {
    /// 在开始接收请求前加锁、校验头和拓扑。
    /// Locks and validates the header and topology before accepting requests.
    pub fn open(config: &Config, dbs: u32, shards: u32) -> Result<(Self, u128), DbError> {
        create_directory(&config.directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.directory.join("LOCK"))
            .map_err(|_| unavailable())?;
        lock.try_lock_exclusive().map_err(|_| {
            err(
                ErrorCode::JournalUnavailable,
                "Event Journal directory is locked",
            )
        })?;
        let path = config.directory.join("journal.lgj");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|_| unavailable())?;
        let len = file.metadata().map_err(|_| unavailable())?.len() as usize;
        let mut header = vec![0; HEADER];
        let stream = if len == 0 {
            let stream = rand::random::<u128>();
            let mut e = Encoder(MAGIC.to_vec());
            e.n(1, 4);
            e.n(stream, 16);
            e.n(dbs.into(), 4);
            e.n(shards.into(), 4);
            let sum = crc32fast::hash(&e.0);
            e.n(sum.into(), 4);
            header = e.0;
            file.write_all(&header)
                .and_then(|_| file.sync_all())
                .map_err(|_| unavailable())?;
            File::open(&config.directory)
                .and_then(|d| d.sync_all())
                .map_err(|_| unavailable())?;
            stream
        } else {
            if len < HEADER || len > config.max_bytes / 2 {
                return Err(at_offset(codec::corrupt(), 0));
            }
            file.read_exact(&mut header)
                .map_err(|_| at_offset(codec::corrupt(), 0))?;
            if &header[..8] != MAGIC
                || crc32fast::hash(&header[..36])
                    != u32::from_be_bytes(header[36..40].try_into().unwrap())
            {
                return Err(at_offset(codec::corrupt(), 0));
            }
            let mut d = Decoder {
                rest: &header[8..36],
            };
            if d.n(4)? != 1 {
                return Err(at_offset(codec::corrupt(), 0));
            }
            let stream = d.n(16)?;
            if d.n(4)? != dbs as u128 || d.n(4)? != shards as u128 {
                return Err(err(
                    ErrorCode::ConfigInvalid,
                    "Journal topology differs from configuration",
                ));
            }
            stream
        };
        let journal = Self {
            _lock: lock,
            file,
            dir: config.directory.clone(),
            header,
            bytes: len.max(HEADER),
            max: config.max_bytes,
            failed: false,
            record_limit: codec::MAX_RECORD.min(config.buffer_bytes / 2),
            #[cfg(test)]
            fail_sync: false,
        };
        // 只有持锁者可删除未发布的替换文件；从不把它当作已提交日志。
        // Only the lock owner removes unpublished replacement files; they never count as committed logs.
        let temporary = journal.dir.join("journal.next");
        if temporary.exists() {
            fs::remove_file(temporary).map_err(|_| unavailable())?;
            journal.sync_dir()?;
        }
        Ok((journal, stream))
    }
    /// 启动回放完整记录并返回有效前缀，全部语义检查通过前不修改原件。
    /// Replays startup records and returns the valid prefix without mutating before all semantic checks pass.
    pub fn recover(
        &mut self,
        mut apply: impl FnMut(&[u8]) -> Result<(), DbError>,
    ) -> Result<usize, DbError> {
        self.scan_from(HEADER, true, |record| {
            apply(record)?;
            Ok(true)
        })
    }
    /// 仅在启动的全部验证成功后，持久化删除已确认未提交的后缀。
    /// Persists removal of a confirmed uncommitted suffix only after every startup check succeeds.
    pub fn finish_recovery(&mut self, end: usize) -> Result<(), DbError> {
        if end < self.bytes {
            self.truncate(end)?;
        }
        Ok(())
    }
    /// 只读校验完整记录；异常保留原件并报告偏移，不修复运行中的日志。
    /// Validates complete records without mutation; faults preserve the original and report their offset.
    pub fn visit(
        &mut self,
        mut apply: impl FnMut(&[u8]) -> Result<(), DbError>,
    ) -> Result<(), DbError> {
        self.visit_from(HEADER, |record| {
            apply(record)?;
            Ok(true)
        })?;
        Ok(())
    }
    /// 从已验证的边界只读续访，false 保留当前记录位置，短尾也报告错误。
    /// Resumes read-only at a validated boundary; false retains that record and incomplete tails fail.
    pub fn visit_from(
        &mut self,
        start: usize,
        apply: impl FnMut(&[u8]) -> Result<bool, DbError>,
    ) -> Result<usize, DbError> {
        self.scan_from(start, false, apply)
    }
    /// 仅启动入口允许识别短尾；所有扫描都只读，确认修复另行提交。
    /// Only startup tolerates identifiable short tails; every scan is read-only until repair is finalized.
    fn scan_from(
        &mut self,
        start: usize,
        allow_tail: bool,
        mut apply: impl FnMut(&[u8]) -> Result<bool, DbError>,
    ) -> Result<usize, DbError> {
        self.file
            .seek(SeekFrom::Start(start as u64))
            .map_err(|_| unavailable())?;
        let total = self.file.metadata().map_err(|_| unavailable())?.len() as usize;
        let mut pos = start;
        if start < HEADER || start > total {
            return Err(at_offset(codec::corrupt(), start));
        }
        while pos < total {
            if total - pos < 8 {
                if allow_tail {
                    return Ok(pos);
                }
                return Err(at_offset(codec::corrupt(), pos));
            }
            let mut h = [0; 8];
            self.file.read_exact(&mut h).map_err(|_| unavailable())?;
            let n = u32::from_be_bytes(h[..4].try_into().unwrap()) as usize;
            if n == 0 || n > self.record_limit {
                return Err(at_offset(codec::corrupt(), pos));
            }
            // v1 的长度字段无独立校验，超出余量可能是已提交记录的长度损坏。
            // The v1 length has no independent checksum; overrun may be a damaged committed record.
            if n + 8 > total - pos {
                return Err(at_offset(codec::corrupt(), pos));
            }
            let mut payload = vec![0; n];
            self.file
                .read_exact(&mut payload)
                .map_err(|_| unavailable())?;
            if crc32fast::hash(&payload) != u32::from_be_bytes(h[4..].try_into().unwrap()) {
                return Err(at_offset(codec::corrupt(), pos));
            }
            let trailer_len = (total - pos - n - 8).min(8);
            let mut trailer = [0; 8];
            self.file
                .read_exact(&mut trailer[..trailer_len])
                .map_err(|_| unavailable())?;
            if trailer[..trailer_len] != COMMIT[..trailer_len] {
                return Err(at_offset(codec::corrupt(), pos));
            }
            if trailer_len < 8 {
                // 完整载荷及 CRC 确认边界，且现有尾标是正确前缀，才可作为未提交短尾。
                // A complete payload, valid CRC, and matching trailer prefix identify an incomplete commit.
                if allow_tail {
                    return Ok(pos);
                }
                return Err(at_offset(codec::corrupt(), pos));
            }
            if !apply(&payload).map_err(|error| at_offset(error, pos))? {
                return Ok(pos);
            }
            pos += n + 16;
        }
        Ok(pos)
    }
    /// 尾部修复必须同步，后续追加不能越过未确认字节。
    /// Tail repair is synced so later appends cannot skip uncertain bytes.
    fn truncate(&mut self, pos: usize) -> Result<(), DbError> {
        self.file
            .set_len(pos as u64)
            .and_then(|_| self.file.sync_all())
            .map_err(|_| unavailable())?;
        self.bytes = pos;
        Ok(())
    }
    /// 事件不能侵占控制与替换预留；Events cannot consume control or replacement reserves.
    pub fn fits(&self, n: usize, control: bool) -> bool {
        self.bytes.checked_add(n + 16).is_some_and(|b| {
            b <= if control {
                self.max / 2
            } else {
                self.max * 3 / 8
            }
        })
    }
    /// 完整记录及尾标 fsync 后才返回成功；任何 IO 错误进入不确定故障状态。
    /// Returns success only after record and trailer fsync; any I/O error faults the journal with an unknown outcome.
    pub fn append(&mut self, payload: &[u8], control: bool) -> Result<(), DbError> {
        if self.failed {
            return Err(unavailable());
        }
        if payload.len() > self.record_limit {
            return Err(err(
                ErrorCode::LimitExceeded,
                "Event transaction exceeds disk record limit",
            ));
        }
        if !self.fits(payload.len(), control) {
            return Err(err(
                ErrorCode::JournalFull,
                "Journal capacity reserved or exhausted",
            ));
        }
        let result = self
            .file
            .seek(SeekFrom::End(0))
            .and_then(|_| write_record(&mut self.file, payload))
            .and_then(|_| {
                #[cfg(test)]
                if std::mem::take(&mut self.fail_sync) {
                    return Err(std::io::Error::other("injected sync failure"));
                }
                self.file.sync_all()
            });
        if result.is_err() {
            self.failed = true;
            return Err(err(
                ErrorCode::CommitUnknown,
                "Journal write or sync failed; outcome unknown",
            ));
        }
        self.bytes += payload.len() + 16;
        Ok(())
    }
    /// 以流式复制完成压缩；替换和目录同步完成后才切换句柄。
    /// Compacts by streaming retained records; the handle switches after replacement and directory sync.
    pub fn compact(
        &mut self,
        checkpoint: &[u8],
        groups: &[Vec<u8>],
        mut retain: impl FnMut(&[u8]) -> Result<bool, DbError>,
    ) -> Result<(), DbError> {
        if self.failed {
            return Err(unavailable());
        }
        let path = self.dir.join("journal.next");
        let mut next = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|_| unavailable())?;
        let result = (|| -> Result<(), DbError> {
            next.write_all(&self.header).map_err(|_| unavailable())?;
            write_record(&mut next, checkpoint).map_err(|_| unavailable())?;
            let mut bytes = HEADER + checkpoint.len() + 16;
            let max = self.max;
            self.visit(|record| {
                if retain(record)? {
                    bytes = bytes
                        .checked_add(record.len() + 16)
                        .ok_or_else(unavailable)?;
                    if bytes > max / 2 {
                        return Err(err(
                            ErrorCode::JournalFull,
                            "Checkpoint exceeds reserved capacity",
                        ));
                    }
                    write_record(&mut next, record).map_err(|_| unavailable())?;
                }
                Ok(())
            })?;
            for group in groups {
                bytes = bytes
                    .checked_add(group.len() + 16)
                    .ok_or_else(unavailable)?;
                if bytes > self.max / 2 {
                    return Err(err(
                        ErrorCode::JournalFull,
                        "Group checkpoint exceeds reserved capacity",
                    ));
                }
                write_record(&mut next, group).map_err(|_| unavailable())?;
            }
            next.sync_all().map_err(|_| unavailable())?;
            fs::rename(&path, self.dir.join("journal.lgj")).map_err(|_| unavailable())?;
            self.sync_dir()?;
            self.file = next;
            self.bytes = bytes;
            Ok(())
        })();
        if result.is_err() {
            // 明确容量拒绝没有改变有效文件；只有 IO 不确定才关闭后续写入。
            // Capacity rejection leaves the active file intact; uncertain I/O faults further writes.
            if result
                .as_ref()
                .is_err_and(|e| e.code != ErrorCode::JournalFull.as_str())
            {
                self.failed = true;
            }
            let _ = fs::remove_file(path);
        }
        result
    }
    /// 新文件名与替换需要同步目录；New names and replacements require directory sync.
    fn sync_dir(&self) -> Result<(), DbError> {
        File::open(&self.dir)
            .and_then(|d| d.sync_all())
            .map_err(|_| unavailable())
    }
}
/// 底层记录边界，不等于已持久化成功；Raw record boundary, not a durability acknowledgement.
fn write_record(file: &mut File, payload: &[u8]) -> std::io::Result<()> {
    file.write_all(&(payload.len() as u32).to_be_bytes())?;
    file.write_all(&crc32fast::hash(payload).to_be_bytes())?;
    file.write_all(payload)?;
    file.write_all(COMMIT)
}
/// 日志不可用，不能静默变为 Fast；Journal unavailable, never silently Fast.
fn unavailable() -> DbError {
    err(ErrorCode::JournalUnavailable, "Event Journal unavailable")
}

/// 只暴露记录偏移，不泄露事件内容；Reports only the record offset without exposing event contents.
fn at_offset(mut error: DbError, offset: usize) -> DbError {
    error.message = format!("{} at byte offset {offset}", error.message);
    error
}

/// 新目录项同步父目录，避免首次提交后目录身份仍未持久化。
/// Syncs parents of new directories so initial commits cannot lose their directory identity.
fn create_directory(path: &std::path::Path) -> Result<(), DbError> {
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::metadata(current) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => return Err(unavailable()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(std::path::Path::new("."));
            }
            Err(_) => return Err(unavailable()),
        }
    }
    for dir in missing.into_iter().rev() {
        fs::create_dir(&dir).map_err(|_| unavailable())?;
        let parent = dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."));
        File::open(parent)
            .and_then(|p| p.sync_all())
            .map_err(|_| unavailable())?;
    }
    Ok(())
}

/// 原始记录夹具在各平台验证读写边界；真实目录恢复另由 Unix 契约覆盖。
/// Raw record fixtures check I/O boundaries on every platform; Unix contracts cover directory recovery.
#[cfg(test)]
mod tests {
    use super::*;

    /// 直接构造已打开文件，避免把 Windows 不支持目录同步误算作读取测试失败。
    /// Constructs an open file directly so Windows directory-sync limits do not mask read tests.
    fn fixture(bytes: &[u8]) -> Journal {
        let dir = std::env::temp_dir().join(format!("ling-record-{:032x}", rand::random::<u128>()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("journal.lgj");
        fs::write(&path, bytes).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        Journal {
            _lock: file.try_clone().unwrap(),
            file,
            dir,
            header: bytes[..HEADER].to_vec(),
            bytes: bytes.len(),
            max: 1024 * 1024,
            failed: false,
            record_limit: 4096,
            fail_sync: false,
        }
    }

    /// 关闭句柄后删除夹具，兼容 Windows 的打开文件删除规则。
    /// Closes handles before fixture cleanup, respecting Windows open-file deletion rules.
    fn cleanup(journal: Journal) {
        let dir = journal.dir.clone();
        drop(journal);
        fs::remove_dir_all(dir).unwrap();
    }

    /// 独立拼出记录字节；不使用被测写入器产生预期边界。
    /// Builds record bytes independently of the writer under test.
    fn record(payload: &[u8]) -> Vec<u8> {
        let mut bytes = (payload.len() as u32).to_be_bytes().to_vec();
        bytes.extend(crc32fast::hash(payload).to_be_bytes());
        bytes.extend(payload);
        bytes.extend(b"LGCMT001");
        bytes
    }

    /// 运行期完整与续读遍历遇到任何短尾都必须保留原件。
    /// Full and resumed runtime scans must leave every incomplete tail untouched.
    #[test]
    fn runtime_reads_preserve_incomplete_and_corrupt_files() {
        let complete = record(b"committed");
        let mut invalid_checksum = record(b"bad");
        invalid_checksum[4] ^= 1;
        for tail in [
            vec![0, 0],
            vec![0, 0, 0, 20, 0, 0, 0, 0, 1, 2],
            record(b"partial")[..18].to_vec(),
            invalid_checksum,
        ] {
            let mut bytes = vec![0; HEADER];
            bytes.extend(&complete);
            let offset = bytes.len();
            bytes.extend(tail);
            let mut journal = fixture(&bytes);
            for resumed in [false, true] {
                let error = if resumed {
                    journal.visit_from(offset, |_| Ok(true)).unwrap_err()
                } else {
                    journal.visit(|_| Ok(())).unwrap_err()
                };
                assert_eq!(error.code, ErrorCode::JournalUnavailable.as_str());
                assert!(error.message.contains(&format!("byte offset {offset}")));
                assert_eq!(fs::read(journal.dir.join("journal.lgj")).unwrap(), bytes);
                assert_eq!(journal.bytes, bytes.len());
            }
            cleanup(journal);
        }
    }

    /// 正常记录不改写；确认短尾只有完成全部恢复检查后才截断。
    /// Valid records remain unchanged; identifiable short tails truncate only after every recovery check.
    #[test]
    fn startup_recognizes_short_tails_and_defers_repair() {
        let mut complete = vec![0; HEADER];
        complete.extend(record(b"committed"));
        let partial = record(b"partial");
        let mut tails = vec![vec![], vec![0], vec![0, 0, 0, 7, 1, 2, 3]];
        for trailer_len in 0..8 {
            tails.push(partial[..8 + b"partial".len() + trailer_len].to_vec());
        }
        for tail in tails {
            let mut bytes = complete.clone();
            bytes.extend(tail);
            let mut journal = fixture(&bytes);
            let mut records = Vec::new();
            let end = journal
                .recover(|payload| {
                    records.push(payload.to_vec());
                    Ok(())
                })
                .unwrap();
            assert_eq!(records, [b"committed"]);
            assert_eq!(end, complete.len());
            assert_eq!(fs::read(journal.dir.join("journal.lgj")).unwrap(), bytes);
            journal.finish_recovery(end).unwrap();
            assert_eq!(fs::read(journal.dir.join("journal.lgj")).unwrap(), complete);
            assert_eq!(journal.bytes, complete.len());
            let mut replayed = Vec::new();
            journal
                .visit(|payload| {
                    replayed.push(payload.to_vec());
                    Ok(())
                })
                .unwrap();
            assert_eq!(replayed, records);
            cleanup(journal);
        }
    }

    /// 长度越界、CRC、尾标或回放语义错误都必须保留原件，不猜测可疑记录是否提交。
    /// Length overruns, CRC, trailer, and replay-semantic faults preserve the original without guessing commit state.
    #[test]
    fn startup_preserves_ambiguous_corrupt_and_semantically_invalid_records() {
        let mut valid = vec![0; HEADER];
        valid.extend(record(b"committed"));
        let mut bad_crc = record(b"bad");
        bad_crc[4] ^= 1;
        let mut bad_prefix = record(b"bad");
        bad_prefix.truncate(12);
        bad_prefix[11] ^= 1;
        let mut bad_trailer = record(b"bad");
        *bad_trailer.last_mut().unwrap() ^= 1;
        let mut inflated = record(b"first");
        inflated[..4].copy_from_slice(&4096_u32.to_be_bytes());
        inflated.extend(record(b"later"));
        let mut semantic = record(b"semantically invalid");
        semantic.extend([0, 0]);
        for (tail, semantic_error) in [
            (vec![0, 0, 0, 20, 0, 0, 0, 0, 1, 2], false),
            (bad_crc, false),
            (bad_prefix, false),
            (bad_trailer, false),
            (inflated, false),
            (semantic, true),
        ] {
            let mut bytes = valid.clone();
            bytes.extend(tail);
            let mut journal = fixture(&bytes);
            let error = journal
                .recover(|payload| {
                    if semantic_error && payload != b"committed" {
                        Err(codec::corrupt())
                    } else {
                        Ok(())
                    }
                })
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::JournalUnavailable.as_str());
            assert!(
                error
                    .message
                    .contains(&format!("byte offset {}", valid.len()))
            );
            assert_eq!(fs::read(journal.dir.join("journal.lgj")).unwrap(), bytes);
            assert_eq!(journal.bytes, bytes.len());
            cleanup(journal);
        }
    }

    /// 内核拒绝写入后必须报告未知并禁止重试追加。
    /// Kernel write rejection must report unknown outcome and forbid subsequent appends.
    #[cfg(target_os = "linux")]
    #[test]
    fn enospc_faults_the_journal_globally() {
        let directory =
            std::env::temp_dir().join(format!("linger-full-{:032x}", rand::random::<u128>()));
        let config = Config {
            reliable: true,
            directory: directory.clone(),
            ..Default::default()
        };
        let (mut journal, _) = Journal::open(&config, 1, 1).unwrap();
        journal.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/full")
            .unwrap();
        let error = journal.append(&[1, 0, 0, 0, 0], false).unwrap_err();
        assert_eq!(error.code, ErrorCode::CommitUnknown.as_str());
        assert!(journal.failed);
        assert_eq!(
            journal.append(&[1, 0, 0, 0, 0], false).unwrap_err().code,
            ErrorCode::JournalUnavailable.as_str()
        );
        drop(journal);
        fs::remove_dir_all(directory).unwrap();
    }
}
