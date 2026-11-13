//! List、Map、Set 共享写入发布边界。
//! List, Map, and Set operations share the publication boundary.
mod lists;
mod maps;
mod sets;
use crate::{Engine, support::*};
use linger_types::{DbError, ErrorCode, Value};
use std::time::Instant;

impl Engine {
    /// 精确匹配名称，不把未知名称误作同前缀命令。
    /// Matches names exactly instead of accepting unknown commands by prefix.
    pub(crate) fn container(
        &self,
        db: u16,
        name: &str,
        args: &[Value],
        now: Instant,
        response_bytes: usize,
    ) -> Result<Effect, DbError> {
        match name {
            "LPUSH" | "RPUSH" | "LPOP" | "RPOP" | "LRANGE" | "LINDEX" | "LSET" | "LLEN"
            | "LREM" | "LTRIM" => self.list_command(db, name, args, now),
            "FSET" | "FSETNX" | "FGET" | "FDEL" | "FEXISTS" | "FKEYS" | "FVALS" | "FGETALL"
            | "FLEN" | "FMERGE" => self.map_command(db, name, args, now, response_bytes),
            "SADD" | "SREM" | "SISMEMBER" | "SMISMEMBER" | "SMEMBERS" | "SCARD" | "SUNION"
            | "SINTER" | "SDIFF" | "SUNIONSTORE" | "SINTERSTORE" | "SDIFFSTORE" | "SRANDMEMBER"
            | "SPOP" => self.set_command(db, name, args, now, response_bytes),
            _ => Err(error(
                ErrorCode::UnknownCommand,
                "Unknown or unimplemented command",
            )),
        }
    }
}
