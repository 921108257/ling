//! 同步、带校验和的原子记录；文件内容与目录项分别同步。
//! Synchronous checksummed atomic records; file contents and directory entries are synced separately.

use crate::{
    Config,
    codec::{self, Decoder, Encoder},
    err,
};
use fs2::FileExt;
use linger_types::{DbError, ErrorCode};
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
        let mut journal = Self {
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
        journal.visit(|_| Ok(()))?;
        Ok((journal, stream))
    }
    /// 流式校验记录，末尾不完整才允许截断；完整坏记录必须拒绝。
    /// Validates records as a stream; only incomplete tails may truncate, never complete corrupt records.
    pub fn visit(
        &mut self,
        mut apply: impl FnMut(&[u8]) -> Result<(), DbError>,
    ) -> Result<(), DbError> {
        self.file
            .seek(SeekFrom::Start(HEADER as u64))
            .map_err(|_| unavailable())?;
        let total = self.file.metadata().map_err(|_| unavailable())?.len() as usize;
        let mut pos = HEADER;
        while pos < total {
            if total - pos < 8 {
                self.truncate(pos)?;
                break;
            }
            let mut h = [0; 8];
            self.file.read_exact(&mut h).map_err(|_| unavailable())?;
            let n = u32::from_be_bytes(h[..4].try_into().unwrap()) as usize;
            if n == 0 || n > self.record_limit {
                return Err(at_offset(codec::corrupt(), pos));
            }
            if n + 16 > total - pos {
                self.truncate(pos)?;
                break;
            }
            let mut payload = vec![0; n];
            self.file
                .read_exact(&mut payload)
                .map_err(|_| unavailable())?;
            let mut trailer = [0; 8];
            self.file
                .read_exact(&mut trailer)
                .map_err(|_| unavailable())?;
            if &trailer != COMMIT
                || crc32fast::hash(&payload) != u32::from_be_bytes(h[4..].try_into().unwrap())
            {
                return Err(at_offset(codec::corrupt(), pos));
            }
            apply(&payload).map_err(|error| at_offset(error, pos))?;
            pos += n + 16;
        }
        Ok(())
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

/// 真实 ENOSPC 设备只用于测试边界，不替代目录恢复测试。
/// A real ENOSPC device exercises the I/O boundary without replacing directory recovery tests.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    /// 内核拒绝写入后必须报告未知并禁止重试追加。
    /// Kernel write rejection must report unknown outcome and forbid subsequent appends.
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
