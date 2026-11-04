//! SQLite 元数据层：文件表 + FTS5 全文索引 + 规则/分类/日志。
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub const FTS_SCHEMA_VERSION: i64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub dir: String,
    pub name: String,
    pub ext: String,
    pub kind: String,
    pub size: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub text_len: i64,
    pub text_state: String,
    pub embed_state: String,
    pub thumb_key: Option<String>,
    pub category: Option<String>,
    pub tags: Option<String>,
}

impl FileRow {
    pub fn new(path: &str) -> Self {
        // 统一分隔符：用户可能粘贴 C:/a/b，walkdir 给的是 C:\a\b。
        // 不统一的话同一文件会在库里存成两条（path 是 UNIQUE，但字符串不同就绕过约束），
        // 表现为「统计 35 个文件，实际只有 16 个」。
        let norm = crate::util::norm_path(path);
        let p = Path::new(&norm);
        Self {
            id: 0,
            path: norm.clone(),
            dir: p.parent().map(|d| d.to_string_lossy().to_string()).unwrap_or_default(),
            name: p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            ext: p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default(),
            kind: "other".into(),
            size: 0,
            mtime: 0,
            ctime: 0,
            text_len: 0,
            text_state: "pending".into(),
            embed_state: "pending".into(),
            thumb_key: None,
            category: None,
            tags: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub field: String,
    pub op: String,
    pub value: String,
    pub category: String,
    pub priority: i64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub id: i64,
    pub ts: i64,
    pub kind: String,
    pub msg: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRoot {
    pub root: String,
    pub enabled: bool,
    pub last_scan: i64,
    pub files: i64,
    pub bytes: i64,
    pub discovered_files: i64,
    pub discovered_bytes: i64,
    pub added: i64,
    pub updated: i64,
    pub skipped: i64,
    pub errors: i64,
    pub missing: i64,
    pub text_ok: i64,
    pub embed_ok: i64,
    pub completed: i64,
    pub text_pending: i64,
    pub embed_pending: i64,
    pub kinds: Vec<IndexKindStat>,
    pub last_error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexKindStat {
    pub kind: String,
    pub count: i64,
    pub bytes: i64,
}

pub struct Db {
    conn: Mutex<Connection>,
    /// 全库目录聚合的短 TTL 缓存。
    ///
    /// 一条 `GROUP BY dir` 在 28 万行库上要约 220~275 ms（线上库比热副本慢），
    /// 而目录树面板 / 星系图在用户连续点开几层时会连着调好几次。缓存 3 秒
    /// 既能让连点几乎零成本，索引在后台跑时也只是最多滞后 3 秒。
    dir_rollup_cache: Mutex<Option<(std::time::Instant, std::sync::Arc<Vec<(String, i64, i64)>>)>>,
}

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -32768)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Self { conn: Mutex::new(conn), dir_rollup_cache: Mutex::new(None) };
        db.migrate()?;
        Ok(db)
    }

    pub fn migrate(&self) -> rusqlite::Result<()> {
        let c = self.conn.lock();
        c.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS files(
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                dir TEXT NOT NULL,
                name TEXT NOT NULL,
                ext TEXT NOT NULL,
                kind TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                mtime INTEGER NOT NULL DEFAULT 0,
                ctime INTEGER NOT NULL DEFAULT 0,
                text_len INTEGER NOT NULL DEFAULT 0,
                text_state TEXT NOT NULL DEFAULT 'pending',
                embed_state TEXT NOT NULL DEFAULT 'pending',
                thumb_key TEXT,
                category TEXT,
                tags TEXT,
                lsh TEXT,
                missing INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS ix_files_dir ON files(dir);
            CREATE INDEX IF NOT EXISTS ix_files_path_nocase ON files(path COLLATE NOCASE);
            CREATE INDEX IF NOT EXISTS ix_files_kind ON files(kind);
            CREATE INDEX IF NOT EXISTS ix_files_mtime ON files(mtime DESC);
            CREATE INDEX IF NOT EXISTS ix_files_size ON files(size DESC);
            CREATE INDEX IF NOT EXISTS ix_files_text ON files(text_state);
            CREATE INDEX IF NOT EXISTS ix_files_embed ON files(embed_state);
            CREATE INDEX IF NOT EXISTS ix_files_cat ON files(category);
            CREATE INDEX IF NOT EXISTS ix_files_missing ON files(missing);
            -- 星图展开用：按类别统计并列出目录时走 (kind,dir)，避免把整类文件行拉回内存。
            CREATE INDEX IF NOT EXISTS ix_files_kind_dir ON files(kind, dir);
            -- 目录树逐层展开用的覆盖索引：dir 区间扫描 + COUNT/SUM(size) 不回表。
            -- 1TB 库（千万行）上这条能把「展开一个目录」从秒级压到毫秒级。
            CREATE INDEX IF NOT EXISTS ix_files_dir_size ON files(dir, size) WHERE missing=0;

            CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT);
            CREATE TABLE IF NOT EXISTS rules(
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                field TEXT NOT NULL,
                op TEXT NOT NULL,
                value TEXT NOT NULL,
                category TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                enabled INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS activity(
                id INTEGER PRIMARY KEY,
                ts INTEGER NOT NULL,
                kind TEXT NOT NULL,
                msg TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS ix_activity_ts ON activity(ts DESC);
            CREATE TABLE IF NOT EXISTS index_roots(
                root TEXT PRIMARY KEY,
                enabled INTEGER NOT NULL DEFAULT 1,
                last_scan INTEGER NOT NULL DEFAULT 0,
                files INTEGER NOT NULL DEFAULT 0,
                bytes INTEGER NOT NULL DEFAULT 0,
                added INTEGER NOT NULL DEFAULT 0,
                updated INTEGER NOT NULL DEFAULT 0,
                skipped INTEGER NOT NULL DEFAULT 0,
                errors INTEGER NOT NULL DEFAULT 0,
                missing INTEGER NOT NULL DEFAULT 0,
                discovered_files INTEGER NOT NULL DEFAULT 0,
                discovered_bytes INTEGER NOT NULL DEFAULT 0,
                last_error TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS doc_pages(
                id INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL,
                page_no INTEGER NOT NULL,
                body TEXT NOT NULL,
                raw TEXT NOT NULL,
                UNIQUE(file_id, page_no)
            );
            CREATE INDEX IF NOT EXISTS ix_doc_pages_file ON doc_pages(file_id, page_no);
            CREATE VIRTUAL TABLE IF NOT EXISTS page_docs USING fts5(
                file_id UNINDEXED,
                page_no UNINDEXED,
                name,
                body,
                raw
            );
            "#,
        )?;
        // 盘符是 `dir` 的纯函数（前两个字符 "E:"）。早期版本想用虚拟生成列
        // `drive` 来承载它，但 `ALTER TABLE ... ADD COLUMN` 在某些 SQLite 构建
        // 上会失败，而失败被 `let _ =` 吞掉 —— 结果是 `drive_kind_stats()` 里
        // 的 `SELECT drive ...` 直接报 "no such column: drive"，又被调用方的
        // `unwrap_or_default()` 吃成空 Vec，星图首屏因此没有任何恒星（点什么都
        // 是空的）。这里改成**表达式索引**：不依赖表结构，任何库都能建成功。
        let _ = c.execute("DROP INDEX IF EXISTS ix_files_drive_kind", []);
        if let Err(error) = c.execute(
            "CREATE INDEX IF NOT EXISTS ix_files_drive_kind \
             ON files(substr(dir, 1, 2), kind, size) WHERE missing=0",
            [],
        ) {
            // 建索引失败不致命（只是慢），但必须留下痕迹，别再静默。
            eprintln!("[db] 盘符聚合索引创建失败（不影响功能，仅影响星图首屏速度）：{error}");
        }
        let _ = c.execute("ALTER TABLE index_roots ADD COLUMN last_error TEXT NOT NULL DEFAULT ''", []);
        let _ = c.execute("ALTER TABLE index_roots ADD COLUMN discovered_files INTEGER NOT NULL DEFAULT 0", []);
        let _ = c.execute("ALTER TABLE index_roots ADD COLUMN discovered_bytes INTEGER NOT NULL DEFAULT 0", []);
        // 历史遗留：早期版本按原始字符串存 path，用户粘贴 C:/a/b 与 walkdir 的 C:\a\b
        // 会被当成两个文件。这里把正斜杠统一成反斜杠；若统一后撞了 UNIQUE，
        // 说明是重复行，直接删掉后来者（保留 id 小的、状态更完整的）。
        c.execute(
            "UPDATE OR IGNORE files SET path = replace(path, '/', '\\') \
             WHERE instr(path, '/') > 0",
            [],
        )?;
        c.execute(
            "DELETE FROM files WHERE id NOT IN \
             (SELECT MIN(id) FROM files GROUP BY replace(path, '/', '\\'))",
            [],
        )?;
        // FTS5：unicode61 + 应用层中文 2-gram。
        //
        // docs.rowid 直接等于 files.id，一条文件一行正文，天然一一对应。
        // 早期版本用 docs_map 间接映射，但 FTS5 虚表的 last_insert_rowid()
        // 不可靠，加上文件删除后 id 会被复用，映射会指到别人的正文上 ——
        // 表现为「搜到 A 文件，高亮片段却是 B 的内容」。这里直接砍掉映射表。
        let existing: Option<String> = c
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='docs'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let prev_ver: i64 = c
            .query_row("SELECT v FROM meta WHERE k='schema_version'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let need_rebuild = match &existing {
            None => true,
            Some(sql) => {
                prev_ver < FTS_SCHEMA_VERSION
                    || !sql.to_lowercase().contains("unicode61")
                    || !sql.contains("raw")
            }
        };
        if need_rebuild {
            c.execute_batch("DROP TABLE IF EXISTS docs; DROP TABLE IF EXISTS docs_map;")?;
            // 顺带把 skip 也重置：旧版本因为 "doc" 判定 bug，把整类文档都标成了
            // skip，光重建表不清这个状态，那些文件仍然永远不会被重新抽取。
            c.execute(
                "UPDATE files SET text_state='pending' WHERE text_state IN ('ok','skip')",
                [],
            )?;
            c.execute_batch(
                "CREATE VIRTUAL TABLE docs USING fts5(name, body, raw UNINDEXED, tokenize='unicode61');",
            )?;
        } else {
            // 文件删掉后正文还留着 = 孤儿，会命中已不存在的文件，顺手清掉。
            c.execute("DELETE FROM docs WHERE rowid NOT IN (SELECT id FROM files)", [])?;
        }
        c.execute(
            "INSERT INTO meta(k,v) VALUES('schema_version',?1) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![FTS_SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let c = self.conn.lock();
        c.query_row("SELECT v FROM meta WHERE k=?1", params![key], |row| row.get(0))
            .optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO meta(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            params![key, value],
        )?;
        Ok(())
    }

    // ---------------- 文件表
    pub fn upsert_file(&self, f: &FileRow) -> rusqlite::Result<i64> {
        let c = self.conn.lock();
        c.execute(
            r#"INSERT INTO files(path,dir,name,ext,kind,size,mtime,ctime,text_state,embed_state,category,tags,missing)
               VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'pending','pending',?9,?10,0)
               ON CONFLICT(path) DO UPDATE SET
                 dir=excluded.dir, name=excluded.name, ext=excluded.ext, kind=excluded.kind,
                 size=excluded.size, mtime=excluded.mtime, ctime=excluded.ctime, missing=0,
                 thumb_key=CASE WHEN files.size=excluded.size AND files.mtime=excluded.mtime THEN files.thumb_key ELSE NULL END,
                 text_len=0, text_state='pending', embed_state='pending'"#,
            params![f.path, f.dir, f.name, f.ext, f.kind, f.size, f.mtime, f.ctime, f.category, f.tags],
        )?;
        // A changed file must not keep stale full-text content while it waits
        // for the next incremental indexing pass.
        c.execute("DELETE FROM docs WHERE rowid=(SELECT id FROM files WHERE path=?1)", params![f.path])?;
        c.execute("DELETE FROM page_docs WHERE file_id=(SELECT id FROM files WHERE path=?1)", params![f.path])?;
        c.execute("DELETE FROM doc_pages WHERE file_id=(SELECT id FROM files WHERE path=?1)", params![f.path])?;
        c.query_row("SELECT id FROM files WHERE path=?1", params![f.path], |row| row.get(0))
    }

    pub fn get_by_path(&self, path: &str) -> rusqlite::Result<Option<FileRow>> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags
             FROM files WHERE path=?1",
            params![path],
            row_to_file,
        )
        .optional()
    }

    pub fn get_by_id(&self, id: i64) -> rusqlite::Result<Option<FileRow>> {
        let c = self.conn.lock();
        c.query_row(
            "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags
             FROM files WHERE id=?1",
            params![id],
            row_to_file,
        )
        .optional()
    }

    /// 批量取 (kind, ext)，供入队时判定工作类型。
    ///
    /// 早先 `Indexer::enqueue` 对每个 id 调一次 `get_by_id`：十万条待办就是
    /// 十万次 SQLite 查询加十万次连接锁竞争，光入队就要几秒。这里一次
    /// `json_each` 就够（同样规避 SQLite 999 个绑定变量的上限）。
    pub fn file_categories(&self, ids: &[i64]) -> rusqlite::Result<std::collections::HashMap<i64, (String, String)>> {
        let mut out = std::collections::HashMap::with_capacity(ids.len());
        if ids.is_empty() {
            return Ok(out);
        }
        let payload = serde_json::to_string(ids).unwrap_or_else(|_| "[]".into());
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT id, kind, ext FROM files WHERE id IN (SELECT value FROM json_each(?1))",
        )?;
        let rows = st.query_map(params![payload], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, kind, ext) = row?;
            out.insert(id, (kind, ext));
        }
        Ok(out)
    }

    pub fn count(&self) -> rusqlite::Result<i64> {
        let c = self.conn.lock();
        c.query_row("SELECT COUNT(*) FROM files WHERE missing=0", [], |r| r.get(0))
    }

    pub fn total_bytes(&self) -> rusqlite::Result<i64> {
        let c = self.conn.lock();
        c.query_row("SELECT COALESCE(SUM(size),0) FROM files WHERE missing=0", [], |r| r.get(0))
    }

    /// 各类别统计：kind -> (数量, 总大小)
    pub fn kind_stats(&self) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT kind, COUNT(*), COALESCE(SUM(size),0) FROM files WHERE missing=0 GROUP BY kind ORDER BY 2 DESC",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    pub fn update_fields(&self, old_path: &str, fields: &[(&str, String)]) -> rusqlite::Result<()> {
        // 白名单列名，避免 SQL 注入
        const ALLOWED: &[&str] = &[
            "path", "dir", "name", "ext", "kind", "size", "mtime", "ctime", "text_len",
            "text_state", "embed_state", "thumb_key", "category", "tags", "missing",
        ];
        let mut sets = Vec::new();
        let mut vals: Vec<rusqlite::types::Value> = Vec::new();
        for (k, v) in fields {
            if !ALLOWED.contains(k) {
                continue;
            }
            sets.push(format!("{k}=?"));
            vals.push(v.clone().into());
        }
        if sets.is_empty() {
            return Ok(());
        }
        let sql = format!("UPDATE files SET {} WHERE path=?", sets.join(","));
        vals.push(old_path.to_string().into());
        let c = self.conn.lock();
        c.execute(&sql, rusqlite::params_from_iter(vals))?;
        Ok(())
    }

    /// Update the indexed path after a file is renamed on disk. The FTS name
    /// column is updated in the same transaction so filename searches remain
    /// consistent with the files table.
    pub fn rename_file(&self, old_path: &str, new_path: &str) -> rusqlite::Result<()> {
        let new = Path::new(new_path);
        let dir = new
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let name = new
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext = new
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        let id: i64 = tx.query_row(
            "SELECT id FROM files WHERE path=?1",
            params![old_path],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE files SET path=?1, dir=?2, name=?3, ext=?4 WHERE id=?5",
            params![new_path, dir, name, ext, id],
        )?;
        tx.execute("UPDATE docs SET name=?1 WHERE rowid=?2", params![name, id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_missing(&self, path: &str) -> rusqlite::Result<()> {
        let c = self.conn.lock();
        c.execute("UPDATE files SET missing=1 WHERE path=?1", params![path])?;
        Ok(())
    }

    /// 整个根目录消失时，把它下面所有记录一次性标成 missing。
    pub fn mark_missing_prefix(&self, prefix: &str) -> rusqlite::Result<usize> {
        let c = self.conn.lock();
        let n = c.execute(
            "UPDATE files SET missing=1 WHERE missing=0 AND (path LIKE ?1 || '%')",
            params![prefix],
        )?;
        Ok(n)
    }

    /// 把某个扫描根下、本次扫描**没遇到**的记录标成 missing，返回受影响行数。
    ///
    /// 只在扫描完整跑完（未取消）时调用 —— 取消时还没走到的目录不该被误判成已删除。
    /// 用临时表而不是 `NOT IN (...)`：几十万 id 拼进 SQL 会超长，而且慢。
    pub fn mark_missing_except(
        &self,
        prefix: &str,
        seen: &std::collections::HashSet<i64>,
    ) -> rusqlite::Result<usize> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute("CREATE TEMP TABLE IF NOT EXISTS _scan_seen(id INTEGER PRIMARY KEY)", [])?;
        tx.execute("DELETE FROM _scan_seen", [])?;
        {
            let mut st = tx.prepare("INSERT OR IGNORE INTO _scan_seen(id) VALUES(?1)")?;
            for id in seen {
                st.execute(params![id])?;
            }
        }
        let n = tx.execute(
            "UPDATE files SET missing=1 \
             WHERE missing=0 AND path LIKE ?1 || '%' \
               AND id NOT IN (SELECT id FROM _scan_seen)",
            params![prefix],
        )?;
        tx.execute("DROP TABLE IF EXISTS _scan_seen", [])?;
        tx.commit()?;
        Ok(n)
    }

    pub fn delete_ids(&self, ids: &[i64]) -> rusqlite::Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        for id in ids {
            tx.execute("DELETE FROM docs WHERE rowid=?1", params![id])?;
            tx.execute("DELETE FROM page_docs WHERE file_id=?1", params![id.to_string()])?;
            tx.execute("DELETE FROM doc_pages WHERE file_id=?1", params![id])?;
            tx.execute("DELETE FROM files WHERE id=?1", params![id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Remove all indexed files below a scan root and return their ids so the
    /// in-memory vector stores can discard the same slots. Roots that remain
    /// configured are protected, which matters when users configure nested
    /// scan directories and later remove only the parent entry.
    pub fn remove_scan_root(&self, root: &str, keep_roots: &[String]) -> rusqlite::Result<Vec<i64>> {
        let (_, lower, upper) = index_root_bounds(root);
        let keep: Vec<String> = keep_roots
            .iter()
            .map(|item| crate::util::norm_path(item).to_ascii_lowercase())
            .collect();
        let c = self.conn.lock();
        let mut ids = Vec::new();
        {
            let mut st = c.prepare(
                "SELECT id,path FROM files WHERE path >= ?1 AND path < ?2",
            )?;
            let rows = st.query_map(params![lower, upper], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (id, path) = row?;
                let normalized = crate::util::norm_path(&path).to_ascii_lowercase();
                let protected = keep.iter().any(|candidate| {
                    normalized == *candidate
                        || normalized.starts_with(&format!("{}\\", candidate.trim_end_matches('\\')))
                });
                if !protected {
                    ids.push(id);
                }
            }
        }
        if ids.is_empty() {
            c.execute(
                "DELETE FROM index_roots WHERE lower(root)=lower(?1)",
                params![crate::util::norm_path(root)],
            )?;
            return Ok(ids);
        }
        let tx = c.unchecked_transaction()?;
        for id in &ids {
            tx.execute("DELETE FROM docs WHERE rowid=?1", params![id])?;
            tx.execute("DELETE FROM page_docs WHERE file_id=?1", params![id.to_string()])?;
            tx.execute("DELETE FROM doc_pages WHERE file_id=?1", params![id])?;
            tx.execute("DELETE FROM files WHERE id=?1", params![id])?;
        }
        tx.execute(
            "DELETE FROM index_roots WHERE lower(root)=lower(?1)",
            params![crate::util::norm_path(root)],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    // ---------------- 全文索引
    /// 写入正文。docs.rowid 恒等于 files.id，重复写入即整行替换。
    pub fn set_doc(&self, file_id: i64, name: &str, body: &str, raw: &str) -> rusqlite::Result<()> {
        let c = self.conn.lock();
        // FTS5 虚表对 INSERT OR REPLACE 的支持依赖 conflict 支持，直接
        // 先删后插最稳，也顺便让 rowid 不会漂移。
        c.execute("DELETE FROM docs WHERE rowid=?1", params![file_id])?;
        c.execute(
            "INSERT INTO docs(rowid, name, body, raw) VALUES(?1,?2,?3,?4)",
            params![file_id, name, body, raw],
        )?;
        // Keep a page-level copy alongside the file aggregate. Form-feed is
        // emitted by the PDF/PPTX extractors; long unpaged Office/text files
        // are split into stable bounded chunks so every document has a useful
        // page number without changing the existing file-level FTS contract.
        c.execute("DELETE FROM page_docs WHERE file_id=?1", params![file_id.to_string()])?;
        c.execute("DELETE FROM doc_pages WHERE file_id=?1", params![file_id])?;
        for (page_no, page) in page_chunks(raw).into_iter().enumerate() {
            let page_no = page_no as i64 + 1;
            let page_body = crate::util::index_text(&page);
            c.execute(
                "INSERT INTO doc_pages(file_id,page_no,body,raw) VALUES(?1,?2,?3,?4)",
                params![file_id, page_no, page_body, page],
            )?;
            c.execute(
                "INSERT INTO page_docs(file_id,page_no,name,body,raw) VALUES(?1,?2,?3,?4,?5)",
                params![file_id.to_string(), page_no.to_string(), name, page_body, page],
            )?;
        }
        c.execute(
            "UPDATE files SET text_len=?1, text_state='ok' WHERE id=?2",
            params![raw.chars().count() as i64, file_id],
        )?;
        Ok(())
    }

    /// Search the page-level FTS index. The caller can use `page_no` to
    /// surface the exact document page while retaining file-level ranking.
    pub fn search_pages_fts(&self, expr: &str, limit: usize) -> rusqlite::Result<Vec<(i64, i64, f64)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT CAST(p.file_id AS INTEGER), CAST(p.page_no AS INTEGER), bm25(page_docs,3.0,1.0,0.0) \
             FROM page_docs p JOIN files f ON f.id=CAST(p.file_id AS INTEGER) AND f.missing=0 \
             WHERE page_docs MATCH ?1 ORDER BY 3 LIMIT ?2",
        )?;
        let rows = st.query_map(params![expr, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    pub fn get_doc_raw(&self, file_id: i64) -> rusqlite::Result<Option<String>> {
        let c = self.conn.lock();
        c.query_row("SELECT raw FROM docs WHERE rowid=?1", params![file_id], |r| r.get(0))
            .optional()
    }

    /// FTS 检索，返回 (file_id, bm25 分数)，分数越小越相关，这里转成升序。
    ///
    /// 连接 files 表是为了挡掉 missing=1：文件在磁盘上删了、扫描后只被标了
    /// missing，正文还留在 docs 里，不挡的话它会一直出现在全文结果里。
    /// 注意不能给 docs 起别名，FTS5 的 MATCH 认的是表名。
    pub fn search_fts(&self, expr: &str, limit: usize) -> rusqlite::Result<Vec<(i64, f64)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT docs.rowid, bm25(docs,2.0,1.0,0.0) FROM docs
             JOIN files f ON f.id=docs.rowid AND f.missing=0
             WHERE docs MATCH ?1 ORDER BY 2 LIMIT ?2",
        )?;
        let rows = st.query_map(params![expr, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    pub fn set_state(&self, file_id: i64, what: &str, value: &str) -> rusqlite::Result<()> {
        let col = match what {
            "text" => "text_state",
            "embed" => "embed_state",
            _ => return Ok(()),
        };
        let c = self.conn.lock();
        c.execute(&format!("UPDATE files SET {col}=?1 WHERE id=?2"), params![value, file_id])?;
        Ok(())
    }

    /// 把全部（未缺失）文件的状态标回 `pending`，用于强制重建。
    /// 换嵌入模型后向量维度会变、旧向量被清空，但 embed_state 仍是 ok，
    /// 不重置的话「重建索引」会排队 0 个文件，用户以为功能坏了。
    pub fn reset_states(&self, what: &str) -> rusqlite::Result<usize> {
        let cols: &[&str] = match what {
            "text" => &["text_state"],
            "embed" => &["embed_state"],
            _ => &["text_state", "embed_state"],
        };
        let c = self.conn.lock();
        let mut n = 0;
        for col in cols {
            n = c.execute(
                &format!("UPDATE files SET {col}='pending' WHERE missing=0"),
                [],
            )?;
        }
        Ok(n)
    }

    pub fn reset_states_in_roots(&self, what: &str, roots: &[String]) -> rusqlite::Result<usize> {
        if roots.is_empty() {
            return Ok(0);
        }
        let col = match what {
            "text" => "text_state",
            "embed" => "embed_state",
            _ => return self.reset_states_for_roots(roots),
        };
        let mut sql = format!("UPDATE files SET {col}='pending' WHERE missing=0 AND (");
        let mut args: Vec<rusqlite::types::Value> = Vec::with_capacity(roots.len() * 3);
        for (i, root) in roots.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            let (exact, lower, upper) = index_root_bounds(root);
            let base = i * 3 + 1;
            sql.push_str(&format!("(path=?{base} COLLATE NOCASE OR (path COLLATE NOCASE>=?{} AND path COLLATE NOCASE<?{}))", base + 1, base + 2));
            args.extend([exact.into(), lower.into(), upper.into()]);
        }
        sql.push(')');
        let c = self.conn.lock();
        c.execute(&sql, rusqlite::params_from_iter(args))
    }

    fn reset_states_for_roots(&self, roots: &[String]) -> rusqlite::Result<usize> {
        if roots.is_empty() {
            return Ok(0);
        }
        let mut sql = String::from("UPDATE files SET text_state='pending',embed_state='pending' WHERE missing=0 AND (");
        let mut args: Vec<rusqlite::types::Value> = Vec::with_capacity(roots.len() * 3);
        for (i, root) in roots.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            let (exact, lower, upper) = index_root_bounds(root);
            let base = i * 3 + 1;
            sql.push_str(&format!("(path=?{base} COLLATE NOCASE OR (path COLLATE NOCASE>=?{} AND path COLLATE NOCASE<?{}))", base + 1, base + 2));
            args.extend([exact.into(), lower.into(), upper.into()]);
        }
        sql.push(')');
        let c = self.conn.lock();
        c.execute(&sql, rusqlite::params_from_iter(args))
    }

    pub fn set_thumb(&self, file_id: i64, key: &str) -> rusqlite::Result<()> {
        let c = self.conn.lock();
        c.execute("UPDATE files SET thumb_key=?1 WHERE id=?2", params![key, file_id])?;
        Ok(())
    }

    pub fn pending_ids(&self, what: &str, limit: usize) -> rusqlite::Result<Vec<i64>> {
        let cond = pending_cond(what);
        let c = self.conn.lock();
        let mut st = c.prepare(&format!(
            "SELECT id FROM files WHERE missing=0 AND ({cond}) LIMIT ?1"
        ))?;
        let rows = st.query_map(params![limit as i64], |r| r.get(0))?;
        rows.collect()
    }

    pub fn pending_ids_in_roots(
        &self,
        what: &str,
        limit: usize,
        roots: &[String],
    ) -> rusqlite::Result<Vec<i64>> {
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let cond = pending_cond(what);
        let mut sql = format!("SELECT id FROM files WHERE missing=0 AND ({cond}) AND (");
        let mut args: Vec<rusqlite::types::Value> = Vec::with_capacity(roots.len() * 3 + 1);
        for (i, root) in roots.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            let (exact, lower, upper) = index_root_bounds(root);
            let base = i * 3 + 1;
            sql.push_str(&format!("(path=?{base} COLLATE NOCASE OR (path COLLATE NOCASE>=?{} AND path COLLATE NOCASE<?{}))", base + 1, base + 2));
            args.extend([exact.into(), lower.into(), upper.into()]);
        }
        sql.push_str(&format!(") LIMIT ?{}", roots.len() * 3 + 1));
        args.push((limit as i64).into());
        let c = self.conn.lock();
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), |r| r.get(0))?;
        rows.collect()
    }

    pub fn file_ids_in_roots(&self, roots: &[String]) -> rusqlite::Result<Vec<i64>> {
        if roots.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::from("SELECT id FROM files WHERE missing=0 AND (");
        let mut args: Vec<rusqlite::types::Value> = Vec::with_capacity(roots.len() * 3);
        for (i, root) in roots.iter().enumerate() {
            if i > 0 {
                sql.push_str(" OR ");
            }
            let (exact, lower, upper) = index_root_bounds(root);
            let base = i * 3 + 1;
            sql.push_str(&format!("(path=?{base} COLLATE NOCASE OR (path COLLATE NOCASE>=?{} AND path COLLATE NOCASE<?{}))", base + 1, base + 2));
            args.extend([exact.into(), lower.into(), upper.into()]);
        }
        sql.push(')');
        let c = self.conn.lock();
        let mut st = c.prepare(&sql)?;
        let rows = st.query_map(rusqlite::params_from_iter(args), |r| r.get(0))?;
        rows.collect()
    }

    pub fn pending_ids_active(&self, what: &str, limit: usize) -> rusqlite::Result<Vec<i64>> {
        let (configured, roots) = {
            let c = self.conn.lock();
            let configured: i64 = c.query_row("SELECT COUNT(*) FROM index_roots", [], |r| r.get(0))?;
            let mut st = c.prepare("SELECT root FROM index_roots WHERE enabled=1 ORDER BY root")?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            (configured > 0, rows.collect::<rusqlite::Result<Vec<_>>>()?)
        };
        if !configured {
            return self.pending_ids(what, limit);
        }
        self.pending_ids_in_roots(what, limit, &roots)
    }

    pub fn count_state(&self, what: &str, state: &str) -> rusqlite::Result<i64> {
        let col = if what == "text" { "text_state" } else { "embed_state" };
        let c = self.conn.lock();
        c.query_row(
            &format!("SELECT COUNT(*) FROM files WHERE missing=0 AND {col}=?1"),
            params![state],
            |r| r.get(0),
        )
    }

    /// Return persisted indexing progress for the requested stage. A file is
    /// considered processed once its stage was attempted. Failed items count
    /// toward visible progress, but remain eligible for retry. This lets a
    /// newly-created Indexer restore its progress after an application restart
    /// without relying on in-memory counters.
    ///
    /// The denominator uses the same kind scope as `pending_cond`: files that
    /// are structurally unprocessable (archives, executables, unclassified)
    /// are never queued, so counting them would pin the bar below 100% forever.
    pub fn index_progress(&self, what: &str) -> rusqlite::Result<(i64, i64)> {
        let (kind_cond, done_cond) = progress_conds(what);
        let total_cond = format!("missing=0 AND {kind_cond}");
        // A combined auto task is complete for a file only after both stages
        // have been attempted. `skip` and `fail` are terminal for progress
        // purposes, while pending remains resumable.
        let done_cond = format!("{total_cond} AND ({done_cond})");
        let c = self.conn.lock();
        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM files WHERE {total_cond}"),
            [],
            |r| r.get(0),
        )?;
        let completed: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM files WHERE {done_cond}"),
            [],
            |r| r.get(0),
        )?;
        Ok((total, completed.min(total)))
    }

    /// Return progress for only the files below the requested scan roots.
    pub fn index_progress_in_roots(
        &self,
        what: &str,
        roots: &[String],
    ) -> rusqlite::Result<(i64, i64)> {
        if roots.is_empty() {
            return Ok((0, 0));
        }
        let (kind_cond, done_expr) = progress_conds(what);
        let mut scope = String::from("missing=0 AND ");
        scope.push_str(kind_cond);
        scope.push_str(" AND (");
        let mut args: Vec<rusqlite::types::Value> = Vec::with_capacity(roots.len() * 3);
        for (index, root) in roots.iter().enumerate() {
            if index > 0 {
                scope.push_str(" OR ");
            }
            let (exact, lower, upper) = index_root_bounds(root);
            let base = index * 3 + 1;
            scope.push_str(&format!(
                "(path=?{base} COLLATE NOCASE OR (path COLLATE NOCASE>=?{} AND path COLLATE NOCASE<?{}))",
                base + 1,
                base + 2
            ));
            args.extend([exact.into(), lower.into(), upper.into()]);
        }
        scope.push(')');

        let c = self.conn.lock();
        let total: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM files WHERE {scope}"),
            rusqlite::params_from_iter(args.clone()),
            |row| row.get(0),
        )?;
        let completed: i64 = c.query_row(
            &format!("SELECT COUNT(*) FROM files WHERE {scope} AND {done_expr}"),
            rusqlite::params_from_iter(args),
            |row| row.get(0),
        )?;
        Ok((total, completed.min(total)))
    }

    // ---------------- 规则 / 分类 / 日志
    pub fn list_rules(&self) -> rusqlite::Result<Vec<Rule>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT id,name,field,op,value,category,priority,enabled FROM rules ORDER BY priority,id",
        )?;
        let rows = st.query_map([], |r| {
            Ok(Rule {
                id: r.get(0)?,
                name: r.get(1)?,
                field: r.get(2)?,
                op: r.get(3)?,
                value: r.get(4)?,
                category: r.get(5)?,
                priority: r.get(6)?,
                enabled: r.get::<_, i64>(7)? != 0,
            })
        })?;
        rows.collect()
    }

    pub fn replace_rules(&self, rules: &[Rule]) -> rusqlite::Result<()> {
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM rules", [])?;
        for r in rules {
            tx.execute(
                "INSERT INTO rules(name,field,op,value,category,priority,enabled) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![r.name, r.field, r.op, r.value, r.category, r.priority, if r.enabled { 1 } else { 0 }],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn log(&self, kind: &str, msg: &str) -> rusqlite::Result<()> {
        let c = self.conn.lock();
        let ts = crate::util::now_ts();
        c.execute(
            "INSERT INTO activity(ts,kind,msg) VALUES(?1,?2,?3)",
            params![ts, kind, msg],
        )?;
        Ok(())
    }

    pub fn upsert_index_root(
        &self,
        root: &str,
        last_scan: i64,
        files: i64,
        bytes: i64,
        added: i64,
        updated: i64,
        skipped: i64,
        errors: i64,
        missing: i64,
        discovered_files: i64,
        discovered_bytes: i64,
        last_error: &str,
    ) -> rusqlite::Result<()> {
        let root = crate::util::norm_path(root);
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO index_roots(root,last_scan,files,bytes,added,updated,skipped,errors,missing,discovered_files,discovered_bytes,last_error)\
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)\
             ON CONFLICT(root) DO UPDATE SET last_scan=excluded.last_scan,files=excluded.files,\
             bytes=excluded.bytes,added=excluded.added,updated=excluded.updated,skipped=excluded.skipped,\
             errors=excluded.errors,missing=excluded.missing,discovered_files=excluded.discovered_files,\
             discovered_bytes=excluded.discovered_bytes,last_error=excluded.last_error",
            params![root, last_scan, files, bytes, added, updated, skipped, errors, missing, discovered_files, discovered_bytes, last_error],
        )?;
        Ok(())
    }

    pub fn set_index_root_enabled(&self, root: &str, enabled: bool) -> rusqlite::Result<()> {
        let root = crate::util::norm_path(root);
        let c = self.conn.lock();
        c.execute(
            "INSERT INTO index_roots(root,enabled) VALUES(?1,?2)\
             ON CONFLICT(root) DO UPDATE SET enabled=excluded.enabled",
            params![root, if enabled { 1 } else { 0 }],
        )?;
        Ok(())
    }

    pub fn ensure_index_root(&self, root: &str) -> rusqlite::Result<()> {
        let root = crate::util::norm_path(root);
        let c = self.conn.lock();
        c.execute("INSERT OR IGNORE INTO index_roots(root) VALUES(?1)", params![root])?;
        Ok(())
    }

    pub fn sync_index_roots(&self, roots: &[String], disabled: &[String]) -> rusqlite::Result<()> {
        let disabled: std::collections::HashSet<String> = disabled
            .iter()
            .map(|root| crate::util::norm_path(root).to_ascii_lowercase())
            .collect();
        let mut c = self.conn.lock();
        let tx = c.transaction()?;
        tx.execute("UPDATE index_roots SET enabled=0", [])?;
        for root in roots {
            let normalized = crate::util::norm_path(root);
            let enabled = !disabled.contains(&normalized.to_ascii_lowercase());
            tx.execute(
                "INSERT INTO index_roots(root,enabled) VALUES(?1,?2)\
                 ON CONFLICT(root) DO UPDATE SET enabled=excluded.enabled",
                params![normalized, if enabled { 1 } else { 0 }],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn index_roots(&self) -> rusqlite::Result<Vec<IndexRoot>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT root,enabled,last_scan,discovered_files,discovered_bytes,added,updated,skipped,errors,missing,last_error \
             FROM index_roots ORDER BY root",
        )?;
        let rows = st.query_map([], |r| {
            Ok(IndexRoot {
                root: r.get(0)?, enabled: r.get::<_, i64>(1)? != 0, last_scan: r.get(2)?,
                files: 0, bytes: 0, discovered_files: r.get(3)?, discovered_bytes: r.get(4)?,
                added: r.get(5)?, updated: r.get(6)?, skipped: r.get(7)?, errors: r.get(8)?,
                missing: r.get(9)?, text_ok: 0, last_error: r.get(10)?, embed_ok: 0,
                completed: 0, text_pending: 0, embed_pending: 0, kinds: Vec::new(),
            })
        })?;
        let mut roots = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        drop(st);
        drop(c);
        for root in &mut roots {
            let (files, bytes, text_ok, embed_ok, completed, text_pending, embed_pending, kinds) =
                self.index_root_file_stats(&root.root)?;
            root.files = files;
            root.bytes = bytes;
            root.text_ok = text_ok;
            root.embed_ok = embed_ok;
            root.completed = completed;
            root.text_pending = text_pending;
            root.embed_pending = embed_pending;
            root.kinds = kinds;
        }
        Ok(roots)
    }

    fn index_root_file_stats(&self, root: &str) -> rusqlite::Result<(i64, i64, i64, i64, i64, i64, i64, Vec<IndexKindStat>)> {
        let root = crate::util::norm_path(root);
        let (exact, lower, upper) = index_root_bounds(&root);
        let c = self.conn.lock();
        let mut st = c.prepare(
            r#"SELECT kind,COUNT(*),COALESCE(SUM(size),0),
               COALESCE(SUM(CASE WHEN text_state='ok' THEN 1 ELSE 0 END),0),
               COALESCE(SUM(CASE WHEN embed_state='ok' THEN 1 ELSE 0 END),0),
               COALESCE(SUM(CASE WHEN text_state!='pending' AND embed_state!='pending' THEN 1 ELSE 0 END),0),
               COALESCE(SUM(CASE WHEN text_state IN ('pending','fail') THEN 1 ELSE 0 END),0),
               COALESCE(SUM(CASE WHEN embed_state IN ('pending','fail') THEN 1 ELSE 0 END),0)
               FROM files
               WHERE missing=0 AND (path=?1 COLLATE NOCASE OR (path COLLATE NOCASE>=?2 AND path COLLATE NOCASE<?3))
               GROUP BY kind ORDER BY 2 DESC"#,
        )?;
        let rows = st.query_map(params![exact, lower, upper], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut totals = (0, 0, 0, 0, 0, 0, 0);
        let mut kinds = Vec::new();
        for row in rows {
            let (kind, count, bytes, text_ok, embed_ok, completed, text_pending, embed_pending) = row?;
            totals.0 += count;
            totals.1 += bytes;
            totals.2 += text_ok;
            totals.3 += embed_ok;
            totals.4 += completed;
            totals.5 += text_pending;
            totals.6 += embed_pending;
            kinds.push(IndexKindStat { kind, count, bytes });
        }
        Ok((totals.0, totals.1, totals.2, totals.3, totals.4, totals.5, totals.6, kinds))
    }

    pub fn recent_activity(&self, limit: usize, kind: Option<&str>) -> rusqlite::Result<Vec<Activity>> {
        let c = self.conn.lock();
        let mut out = Vec::new();
        match kind {
            Some(k) => {
                let mut st = c.prepare(
                    "SELECT id,ts,kind,msg FROM activity WHERE kind=?1 ORDER BY id DESC LIMIT ?2",
                )?;
                let rows = st.query_map(params![k, limit as i64], |r| {
                    Ok(Activity { id: r.get(0)?, ts: r.get(1)?, kind: r.get(2)?, msg: r.get(3)? })
                })?;
                for r in rows {
                    out.push(r?);
                }
            }
            None => {
                let mut st =
                    c.prepare("SELECT id,ts,kind,msg FROM activity ORDER BY id DESC LIMIT ?1")?;
                let rows = st.query_map(params![limit as i64], |r| {
                    Ok(Activity { id: r.get(0)?, ts: r.get(1)?, kind: r.get(2)?, msg: r.get(3)? })
                })?;
                for r in rows {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    pub fn clear_all(&self) -> rusqlite::Result<()> {
        let c = self.conn.lock();
        c.execute_batch(
            "DELETE FROM files; DELETE FROM docs; DELETE FROM page_docs; DELETE FROM doc_pages; DELETE FROM index_roots; DELETE FROM activity;",
        )?;
        Ok(())
    }

    pub fn vacuum(&self) -> rusqlite::Result<()> {
        let c = self.conn.lock();
        c.execute_batch("VACUUM;")?;
        Ok(())
    }

    /// 通用查询：返回 FileRow 列表（SQL 由调用方拼装，参数用占位符传入）。
    pub fn query_rows(&self, sql: &str, args: Vec<rusqlite::types::Value>) -> Vec<FileRow> {
        let c = self.conn.lock();
        let mut st = match c.prepare(sql) {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        let mut rows = match st.query(rusqlite::params_from_iter(args)) {
            Ok(r) => r,
            Err(_) => return vec![],
        };
        let mut out = Vec::new();
        while let Ok(Some(row)) = rows.next() {
            if let Ok(f) = row_to_file(row) {
                out.push(f);
            }
        }
        out
    }

    /// 某个目录下已入库的文件路径（工作区实时检索用它排除已索引文件）。
    pub fn paths_under(&self, root: &str) -> rusqlite::Result<Vec<String>> {
        let (_, lower, upper) = index_root_bounds(root);
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT path FROM files WHERE path >= ?1 AND path < ?2")?;
        let rows = st.query_map(params![lower, upper], |r| r.get::<_, String>(0))?;
        rows.collect()
    }

    /// 目录聚合：返回 (dir, 文件数, 总大小)
    pub fn dir_rollup(&self) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT dir, COUNT(*), COALESCE(SUM(size),0) FROM files WHERE missing=0 GROUP BY dir",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// 目录聚合（按路径前缀裁剪）：只统计某个目录之下的目录。
    ///
    /// 右上角「索引目录结构」面板每展开一层就查一次。全量 GROUP BY 在
    /// 14 万文件的库上约 140ms；带 `dir >= ? AND dir < ?` 的前缀裁剪只需
    /// 40~50ms，且随展开层级加深会更快。
    ///
    /// `prefix` 传 `D:\软件\` 这种「以反斜杠结尾」的形式；传空串表示全部。
    pub fn dir_rollup_under(&self, prefix: &str) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let c = self.conn.lock();
        let (lo, hi) = dir_prefix_bounds(prefix);
        if lo.is_empty() {
            let mut st = c.prepare(
                "SELECT dir, COUNT(*), COALESCE(SUM(size),0) FROM files WHERE missing=0 GROUP BY dir",
            )?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            return rows.collect();
        }
        // 注意：`dir = "D:\wx"` 本身排在 `"D:\wx\"` 之前（短串在前），
        // 只写区间会漏掉「该目录自己的直属文件」，所以要把等值单独并进来。
        let self_dir = crate::util::norm_path(prefix);
        let mut st = c.prepare(
            "SELECT dir, COUNT(*), COALESCE(SUM(size),0) FROM files \
             WHERE missing=0 AND (dir = ?3 OR (dir >= ?1 AND dir < ?2)) GROUP BY dir",
        )?;
        let rows =
            st.query_map(params![lo, hi, self_dir], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// 某目录的直属文件统计：(文件数, 总大小, 已抽正文数, 已建向量数)。
    ///
    /// 面板的叶子节点与「当前目录包含 N 个文件」都用它，避免把 14 万行
    /// 的文件级数据整表拉进内存。
    /// 只要「直接子目录」的聚合，不把全部后代目录拉回内存。
    ///
    /// 旧路径 `dir_rollup_under` 会把前缀下**每一层**目录都返回（1TB 库可达
    /// 数十万行），再在 Rust 侧建 HashMap 反推父聚合 —— 内存与哈希开销都在
    /// 用户点开目录这一瞬间一次性付清，目录一多就卡住界面。
    ///
    /// 这里用 SQL 的字符串切片算出「相对 prefix 后第一个反斜杠之前」的部分，
    /// 一次 GROUP BY 只回传直接子目录（通常 < 100 行），且 COUNT/SUM 天然是
    /// 该子目录的递归统计。配合 ix_files_dir_size 覆盖索引不回表。
    pub fn dir_child_rollup(&self, prefix: &str) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let (lo, hi) = dir_prefix_bounds(prefix);
        if lo.is_empty() {
            return Ok(Vec::new());
        }
        let lo_len = lo.chars().count() as i64;
        let c = self.conn.lock();
        let mut st = c.prepare(
            r#"SELECT CASE WHEN instr(substr(dir, ?4 + 1), ?3) = 0 THEN dir
                              ELSE substr(dir, 1, ?4 + instr(substr(dir, ?4 + 1), ?3) - 1) END AS child,
                         COUNT(*), COALESCE(SUM(size),0)
                  FROM files WHERE missing=0 AND dir >= ?1 AND dir < ?2
                  GROUP BY child ORDER BY 3 DESC"#
        )?;
        let rows = st.query_map(params![lo, hi, "\\", lo_len], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    /// 全库目录聚合：`dir -> (文件数, 总字节)`，供 graph_data 在内存里折叠层级。
    ///
    /// 为什么不是「按前缀查每个目录」：`dir_child_rollup` 单条要 270~295 ms
    /// （它的执行计划是 `SEARCH files USING INDEX ix_files_missing` + 
    /// `USE TEMP B-TREE FOR GROUP BY`，等于全表扫一遍再分组）。graph_data
    /// 首屏第 1 层 19 个目录 = 5.5 秒，第 3 层 242 个目录直接爆掉 —— 这就是
    /// 首屏 17 秒的来源。
    ///
    /// 一次性 `GROUP BY dir` 实测只要 63~66 ms（33,608 行），之后在调用方
    /// 内存里按前缀折叠，19 个子目录耗时 24 ms。目录树越深，这个方案越赚。
    ///
    /// 注意：返回的是**全库**结果，调用方自己按 root 前缀筛。
    pub fn all_dir_rollup(&self) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        const TTL: std::time::Duration = std::time::Duration::from_secs(3);
        {
            let cache = self.dir_rollup_cache.lock();
            if let Some((at, rows)) = cache.as_ref() {
                if at.elapsed() < TTL {
                    return Ok(rows.as_ref().clone());
                }
            }
        }
        let rows: Vec<(String, i64, i64)> = {
            let c = self.conn.lock();
            let mut st = c.prepare(
                "SELECT dir, COUNT(*), COALESCE(SUM(size),0) FROM files WHERE missing=0 GROUP BY dir",
            )?;
            let mapped = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        *self.dir_rollup_cache.lock() =
            Some((std::time::Instant::now(), std::sync::Arc::new(rows.clone())));
        Ok(rows)
    }

    /// 让目录聚合缓存立刻失效（扫描/写入后调用，避免面板看到过期数字）。
    pub fn invalidate_dir_rollup(&self) {
        *self.dir_rollup_cache.lock() = None;
    }

    /// 目录 × 类型聚合：`(dir, kind, 文件数, 总字节)`。
    ///
    /// 树状图（`tree_data`）要给每个节点标「这一类占多少」，原来是把
    /// `files` 表**整表** 28 万行拉进内存再逐行 reduce —— 1 TB 库是几百万行，
    /// 光搬运就要几秒。改成库里先按 (dir, kind) 聚合：28 万行库上 37,264 行
    /// / 约 310 ms，与文件总数无关地只跟目录数走。
    pub fn all_dir_kind_rollup(&self) -> rusqlite::Result<Vec<(String, String, i64, i64)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT dir, kind, COUNT(*), COALESCE(SUM(size),0) FROM files WHERE missing=0 GROUP BY dir, kind",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    }

    /// 一批目录的**直属**统计：dir → (文件数, 总字节)。
    ///
    /// 目录树每展开一层都要给每个子目录标「直属 N 个文件」。逐个调
    /// `dir_file_stats` 是 N 次 prepare/查询；这里用一条 IN 查询走
    /// `ix_files_dir`，子目录上百个也只有一次往返。
    pub fn dir_direct_counts(
        &self,
        dirs: &[String],
    ) -> rusqlite::Result<HashMap<String, (i64, i64)>> {
        let mut out = HashMap::with_capacity(dirs.len());
        if dirs.is_empty() {
            return Ok(out);
        }
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT dir, COUNT(*), COALESCE(SUM(size),0) FROM files WHERE missing=0 \
             AND dir IN (SELECT value FROM json_each(?1)) GROUP BY dir",
        )?;
        // json_each 需要 JSON 数组；sqlite3 自带 JSON1，不必逐项展开占位符，
        // 也就避开了 SQLite 999 个变量的上限（子目录上千个时同样安全）。
        let payload = serde_json::to_string(dirs).unwrap_or_else(|_| "[]".to_string());
        let rows = st.query_map(params![payload], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (dir, count, bytes) = row?;
            out.insert(dir, (count, bytes));
        }
        Ok(out)
    }

    /// 把一个目录串对齐到库里真实写入的大小写。
    ///
    /// `files.dir` 保留的是扫描时的原始大小写，而 SQLite 的 `dir = ?1` 是 BINARY
    /// 比较。用户在面板里手输小写路径、或从旧配置读到大小写变体的根时，直接等值
    /// 查询会一条都不命中，表现为「明明有数据却显示 0 个文件」。这里用 NOCASE
    /// 找一次真身；只在递归统计为 0 的异常路径上才会被调用，正常展开不付这份代价。
    pub fn canonical_dir(&self, dir: &str) -> Option<String> {
        let dir = crate::util::norm_path(dir);
        let dir = dir.trim_end_matches('\\').to_string();
        let c = self.conn.lock();
        let mut st = c
            .prepare("SELECT dir FROM files WHERE dir = ?1 COLLATE NOCASE LIMIT 1")
            .ok()?;
        st.query_row(params![dir], |r| r.get::<_, String>(0)).ok()
    }

    /// 目录名搜索：按「最后一段名字」包含 `needle` 匹配目录，返回
    /// (dir, 文件数, 总大小)，最多 `limit` 行。
    ///
    /// 旧实现在面板里用 `dir_rollup()` 把**全库** 3 万多个目录整表拉进内存再
    /// 逐行 `name.contains()`（28 万文件库 244ms，1TB 库可达秒级 + 数十万行内存）。
    /// 这里把过滤下推到 SQLite：`dir LIKE '%\needle%'` 命中的是「名字段」而不是
    /// 任意父目录，且 GROUP BY dir 可以顺着 ix_files_dir 索引流式聚合，不必建临时表。
    pub fn dir_name_search(
        &self,
        needle: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let needle = needle.trim();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        // LIKE 的通配符必须转义，否则用户搜「100%」会把 % 当成任意串。
        let escaped = needle
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%\\{escaped}%");
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT dir, COUNT(*), COALESCE(SUM(size),0) FROM files \
             WHERE missing=0 AND dir LIKE ?1 ESCAPE '\\' GROUP BY dir LIMIT ?2",
        )?;
        let rows = st.query_map(params![pattern, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    /// 有向量、未缺失文件的 (id, dir) 列表 —— 语义星图做分层取样用。
    ///
    /// 只取两列：10 万条大约 3MB，比"分批 IN 查询 10 万次"快一个数量级；
    /// 真正要展示的那 900 条再单独按 id 查完整元数据。
    pub fn embedded_id_dirs(&self) -> rusqlite::Result<Vec<(i64, String)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT id, COALESCE(dir,'') FROM files
             WHERE missing=0 AND embed_state='ok'",
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// 按 id 批量取语义星图节点的完整元数据。
    pub fn files_by_ids(&self, ids: &[i64]) -> Vec<FileRow> {
        if ids.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(ids.len());
        for batch in ids.chunks(400) {
            let mut sql = String::from(
                "SELECT id,path,dir,name,ext,kind,size,mtime,ctime,text_len,text_state,embed_state,thumb_key,category,tags                  FROM files WHERE id IN (",
            );
            for (i, id) in batch.iter().enumerate() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str(&id.to_string());
            }
            sql.push(')');
            out.extend(self.query_rows(&sql, Vec::new()));
        }
        out
    }

    /// 盘符聚合：磁盘盘符 → (文件数, 总字节)。星图的恒星层用它。
    ///
    /// 直接用表达式 `substr(dir,1,2)` 而不是生成列，配合
    /// ix_files_drive_kind(substr(dir,1,2), kind, size) 覆盖索引，无需回表。
    pub fn drive_stats(&self) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            r#"SELECT substr(dir, 1, 2) AS drive, COUNT(*), COALESCE(SUM(size),0)
                  FROM files WHERE missing=0 AND length(dir) >= 2
                  GROUP BY drive ORDER BY 3 DESC"#,
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// 「盘 × 类别」一次聚合：星图首屏用它，一条 SQL 出全部恒星的规模。
    pub fn drive_kind_stats(&self) -> rusqlite::Result<Vec<(String, String, i64, i64)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            r#"SELECT substr(dir, 1, 2) AS drive, kind, COUNT(*), COALESCE(SUM(size),0)
                  FROM files WHERE missing=0 AND length(dir) >= 2
                  GROUP BY drive, kind ORDER BY drive"#,
        )?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        rows.collect()
    }

    /// 某个目录前缀的**递归**统计（含自身与所有后代），只回一行。
    pub fn dir_recursive_stats(&self, prefix: &str) -> rusqlite::Result<(i64, i64)> {
        let (lo, hi) = dir_prefix_bounds(prefix);
        if lo.is_empty() {
            return Ok((0, 0));
        }
        let self_dir = crate::util::norm_path(prefix);
        let c = self.conn.lock();
        let mut st = c.prepare(
            r#"SELECT COUNT(*), COALESCE(SUM(size),0) FROM files
                  WHERE missing=0 AND (dir = ?3 OR (dir >= ?1 AND dir < ?2))"#,
        )?;
        st.query_row(params![lo, hi, self_dir], |r| Ok((r.get(0)?, r.get(1)?)))
    }

    /// 某个盘 + 某个类别下的**一级目录**聚合（星图「类别行星」展开成卫星用）。
    ///
    /// `E:\111\pixiv\x` 会折叠成 `E:\111`，只回一级目录，行数受顶层目录数限制。
    pub fn kind_dir_rollup(
        &self,
        drive: &str,
        kind: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<(String, i64, i64)>> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            r#"SELECT CASE WHEN instr(substr(dir, 4), ?4) = 0 THEN dir
                              ELSE substr(dir, 1, 3 + instr(substr(dir, 4), ?4) - 1) END AS top,
                         COUNT(*), COALESCE(SUM(size),0)
                  FROM files WHERE missing=0 AND kind = ?1 AND substr(dir, 1, 2) = ?2 AND length(dir) > 3
                  GROUP BY top ORDER BY 3 DESC LIMIT ?3"#,
        )?;
        let rows = st.query_map(params![kind, drive, limit as i64, "\\"], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    /// 某个类别在某个路径前缀下的聚合，只回一行，不把整类文件拉进内存。
    pub fn kind_prefix_stats(&self, kind: &str, lo: &str, hi: &str) -> rusqlite::Result<(i64, i64)> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            r#"SELECT COUNT(*), COALESCE(SUM(size),0) FROM files
                  WHERE missing=0 AND kind = ?1 AND path >= ?2 AND path < ?3"#,
        )?;
        st.query_row(params![kind, lo, hi], |r| Ok((r.get(0)?, r.get(1)?)))
    }

    pub fn dir_file_stats(&self, dir: &str) -> rusqlite::Result<(i64, i64, i64, i64)> {
        let c = self.conn.lock();
        let mut st = c.prepare(
            "SELECT COUNT(*), COALESCE(SUM(size),0),
                    COALESCE(SUM(CASE WHEN text_state='ok' THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN embed_state='ok' THEN 1 ELSE 0 END),0)
             FROM files WHERE missing=0 AND dir = ?1",
        )?;
        let row = st.query_row(params![dir], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        Ok(row)
    }
}

/// 把「全库目录聚合」折叠成「目录 → 其直接子目录 + 子树合计」。
///
/// 输入是 `all_dir_rollup()` 的 `(dir, count, bytes)` 全量行（28 万行库上
/// 33,608 行），输出覆盖 `root` 子树内的**每一层**：每个目录都能拿到自己的
/// 直接子目录列表，以及每个子目录**整棵子树**的文件数与字节数 —— 这正是
/// `dir_child_rollup` 单查时 `GROUP BY child` 给出的语义。
///
/// 做法：对每一条 `(dir, n, sz)`，沿着 dir 的祖先链（从 root 的直接子目录
/// 一直到 dir 自己）逐级累加。这样 root 下每个子目录拿到的就是它子树里所有
/// dir 行的合计，不用为每个目录各跑一次全表扫描。
///
/// 复杂度：行数 × 目录深度（实测 33,608 × ~10），全程只做 HashMap 累加，
/// 一次构建供 graph_data 的所有层级复用。
pub fn fold_dir_children(
    rows: &[(String, i64, i64)],
    root: &str,
) -> (HashMap<String, Vec<(String, i64, i64)>>, (i64, i64)) {
    // 全部 key 统一成「无尾反斜杠」的形式（D: / D:\wx），与 graph_data 里
    // 由 split_path_parts().join("\\") 得到的 dir id 完全一致。
    let root_key = dir_lookup_key(root);
    // root 为空表示「全库」视图：此时不设前缀，直接按盘符（第一段）分桶。
    let prefix = if root_key.is_empty() { String::new() } else { format!("{root_key}\\") };
    // root 自己的直属统计：顺手从同一批行里挑出来，省掉一次
    // dir_recursive_stats 的区间扫描（28 万行库上 142 ms）。
    let mut root_own: (i64, i64) = (0, 0);
    // 先攒 (父目录, 子目录) -> (count, bytes)，最后再按父目录分桶排序。
    let mut agg: HashMap<(String, String), (i64, i64)> = HashMap::new();
    for (dir, count, bytes) in rows {
        let dir = dir_lookup_key(dir);
        if !root_key.is_empty() && dir.eq_ignore_ascii_case(&root_key) {
            root_own.0 += count;
            root_own.1 += bytes;
            continue;
        }
        // 【必须用 get 而不是切片】目录名里有中文，`prefix.len()` 是字节数，
        // 直接 `&dir[..len]` 会切在多字节字符中间 → panic。而且这是
        // Tauri 命令线程里的 panic，非 unwinding 直接 abort 整个进程
        // （实测点开一个中文目录就整个窗口消失）。
        let Some(head) = dir.get(..prefix.len()) else { continue };
        if !prefix.is_empty() && !head.eq_ignore_ascii_case(&prefix) {
            continue;
        }
        // 从 root 的直接子目录开始，逐级往下，把这条 dir 行的数字加到
        // 「每一级祖先 → 它的那个直接子目录」上。
        let rest = dir.get(prefix.len()..).unwrap_or("");
        let mut parent = root_key.clone();
        for part in rest.split('\\') {
            if part.is_empty() {
                continue;
            }
            // 全库视图下 parent 从空串起，第一段就是盘符（D:），不能拼出 "D:"。
            let child = if parent.is_empty() {
                part.to_string()
            } else {
                format!("{parent}\\{part}")
            };
            let slot = agg.entry((parent.clone(), child.clone())).or_insert((0, 0));
            slot.0 += count;
            slot.1 += bytes;
            parent = child;
        }
    }
    let mut out: HashMap<String, Vec<(String, i64, i64)>> = HashMap::new();
    for ((parent, child), (count, bytes)) in agg {
        out.entry(parent).or_default().push((child, count, bytes));
    }
    // 与原实现一致：文件数降序（决定 max_nodes 预算先保谁），同数按大小、再按名字。
    for list in out.values_mut() {
        list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)).then_with(|| a.0.cmp(&b.0)));
    }
    (out, root_own)
}

/// 统一目录 key 的形式：正斜杠转反斜杠、丢尾反斜杠。`D:\` 与 `D:` 都变成 `D:`。
///
/// graph_data 里的目录 id 是 `split_path_parts(p).join("\\")` 的产物，
/// 盘符根就长成 `D:`（`:` 后没有反斜杠）。折叠表必须用同一套 key，
/// 否则 `dir_children.get(&dir_norm)` 永远取不到子目录、图上一片空白。
pub fn dir_lookup_key(p: &str) -> String {
    crate::util::norm_path(p)
        .trim_end_matches('\\')
        .to_string()
}

/// 增量索引队列与进度统计共用的 kind 范围。
///
/// 早先的 `pending_ids` 只看状态列、不看 kind，于是 `E:\111\pixiv` 下
/// 47,691 张图片全被排进 "text" 队列：`process_one` 对图片不做正文抽取，
/// 直接标 `text=skip`，只顺手做了缩略图。结果是跑满 140 秒推进 749 条，
/// 正文 0 条、向量 0 条，GPU 只有 10% 占用 —— 用户看到的就是"索引不动"。
///
/// 现在两边用同一个判定：没有对应能力的文件根本不入队，也不计入进度分母。
fn kind_scope(what: &str) -> &'static str {
    match what {
        "text" => "kind IN ('text','document')",
        "embed" => "kind IN ('image','audio','video','text','document')",
        _ => "kind IN ('text','document','image','audio','video')",
    }
}

/// 待处理条件：状态列 + kind 能力范围。
fn pending_cond(what: &str) -> String {
    let scope = kind_scope(what);
    let state = match what {
        "text" => "text_state IN ('pending','fail')",
        "embed" => "embed_state IN ('pending','fail')",
        _ => "(text_state IN ('pending','fail') OR embed_state IN ('pending','fail'))",
    };
    format!("{scope} AND {state}")
}

/// 进度统计：返回 (分母条件, 已完成条件)。
fn progress_conds(what: &str) -> (&'static str, &'static str) {
    let scope = kind_scope(what);
    let done = match what {
        "text" => "text_state!='pending'",
        "embed" => "embed_state!='pending'",
        _ => "text_state!='pending' AND embed_state!='pending'",
    };
    (scope, done)
}

/// 把一个目录前缀换算成 SQLite 的字符串区间 `[lo, hi)`。
///
/// SQLite 的 TEXT 比较是二进制字节序：`D:\软件\` 之后的所有路径都满足
/// `dir >= "D:\软件\" AND dir < "D:\软件]"`（`]` 是 `\` 的下一个字节）。
/// 传空串时返回两个空串，调用方据此走全量分支。
fn dir_prefix_bounds(prefix: &str) -> (String, String) {
    if prefix.trim().is_empty() {
        return (String::new(), String::new());
    }
    let mut lo = crate::util::norm_path(prefix);
    if !lo.ends_with('\\') {
        lo.push('\\');
    }
    let mut hi = lo.clone();
    hi.pop();
    hi.push(']');
    (lo, hi)
}

fn page_chunks(raw: &str) -> Vec<String> {
    const CHUNK_CHARS: usize = 4_000;
    let explicit: Vec<String> = raw
        .split('\u{c}')
        .map(|part| part.trim().to_owned())
        .collect();
    if raw.contains('\u{c}') {
        return explicit;
    }
    let source = explicit.first().map(String::as_str).unwrap_or(raw.trim());
    if source.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = source.chars().collect();
    chars
        .chunks(CHUNK_CHARS)
        .map(|chunk| chunk.iter().collect::<String>())
        .filter(|part| !part.trim().is_empty())
        .collect()
}

fn index_root_bounds(root: &str) -> (String, String, String) {
    let exact = crate::util::norm_path(root);
    let mut lower = exact.clone();
    if !lower.ends_with('\\') {
        lower.push('\\');
    }
    let mut upper = lower.clone();
    upper.pop();
    upper.push(']');
    (exact, lower, upper)
}

fn row_to_file(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    Ok(FileRow {
        id: r.get(0)?,
        path: r.get(1)?,
        dir: r.get(2)?,
        name: r.get(3)?,
        ext: r.get(4)?,
        kind: r.get(5)?,
        size: r.get(6)?,
        mtime: r.get(7)?,
        ctime: r.get(8)?,
        text_len: r.get(9)?,
        text_state: r.get(10)?,
        embed_state: r.get(11)?,
        thumb_key: r.get(12)?,
        category: r.get(13)?,
        tags: r.get(14)?,
    })
}

#[cfg(test)]
mod index_progress_tests {
    use super::*;

    #[test]
    fn progress_counts_attempted_files_once_and_persists_task_kind() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        // FileRow::new leaves kind at "other"; scanning is what assigns a real
        // kind. The progress denominator now skips unprocessable kinds, so the
        // fixture has to declare one.
        let mut first_row = FileRow::new("C:\\docs\\first.pdf");
        first_row.kind = "document".into();
        let mut second_row = FileRow::new("C:\\docs\\second.pdf");
        second_row.kind = "document".into();
        let first = db.upsert_file(&first_row).unwrap();
        let second = db.upsert_file(&second_row).unwrap();

        assert_eq!(db.index_progress("auto").unwrap(), (2, 0));
        db.set_state(first, "text", "ok").unwrap();
        assert_eq!(db.index_progress("auto").unwrap(), (2, 0));
        db.set_state(first, "embed", "fail").unwrap();
        db.set_state(second, "text", "skip").unwrap();
        db.set_state(second, "embed", "ok").unwrap();
        assert_eq!(db.index_progress("auto").unwrap(), (2, 2));
        assert_eq!(db.index_progress("text").unwrap(), (2, 2));

        assert_eq!(
            db.index_progress_in_roots("auto", &["C:\\docs".to_string()])
                .unwrap(),
            (2, 2)
        );
        assert_eq!(
            db.index_progress_in_roots("auto", &["C:\\other".to_string()])
                .unwrap(),
            (0, 0)
        );

        db.set_meta("index_progress_what", "embed").unwrap();
        assert_eq!(db.get_meta("index_progress_what").unwrap().as_deref(), Some("embed"));
    }

    #[test]
    fn changed_file_is_marked_pending_for_incremental_reindex() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut file = FileRow::new("C:\\docs\\changed.txt");
        file.kind = "text".into();
        let id = db.upsert_file(&file).unwrap();
        db.set_doc(id, "changed.txt", "old body", "old body").unwrap();
        db.set_state(id, "embed", "ok").unwrap();

        file.size = 42;
        file.mtime += 1;
        let same_id = db.upsert_file(&file).unwrap();
        assert_eq!(same_id, id);
        let saved = db.get_by_id(id).unwrap().unwrap();
        assert_eq!(saved.text_state, "pending");
        assert_eq!(saved.embed_state, "pending");
        assert_eq!(saved.text_len, 0);
        assert!(db.get_doc_raw(id).unwrap().is_none());
        assert_eq!(db.index_progress("auto").unwrap(), (1, 0));
    }

    #[test]
    fn pending_queue_skips_kinds_that_can_never_be_processed() {
        // Regression: the queue used to filter on state columns alone, so
        // E:\111\pixiv's 47k images were dequeued as "text" work, marked
        // text=skip one by one, and never advanced the bar. Those kinds must
        // not enter the queue at all.
        let db = Db::open(Path::new(":memory:")).unwrap();
        let mut image = FileRow::new("C:\\docs\\cover.png");
        image.kind = "image".into();
        let mut archive = FileRow::new("C:\\docs\\bundle.zip");
        archive.kind = "archive".into();
        let mut text = FileRow::new("C:\\docs\\notes.md");
        text.kind = "text".into();
        let image_id = db.upsert_file(&image).unwrap();
        let archive_id = db.upsert_file(&archive).unwrap();
        let text_id = db.upsert_file(&text).unwrap();

        let text_queue = db.pending_ids("text", 100).unwrap();
        assert_eq!(text_queue, vec![text_id]);
        // archives have neither text nor a vector representation
        let embed_queue = db.pending_ids("embed", 100).unwrap();
        assert_eq!(embed_queue.len(), 2);
        assert!(embed_queue.contains(&image_id));
        assert!(embed_queue.contains(&text_id));
        assert!(!embed_queue.contains(&archive_id));
        // the bar must not count the archive either
        assert_eq!(db.index_progress("text").unwrap(), (1, 0));
        assert_eq!(db.index_progress("embed").unwrap(), (2, 0));
        assert_eq!(db.index_progress("auto").unwrap(), (2, 0));
    }

    #[test]
    fn page_text_is_indexed_and_replaced_with_the_document() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let id = db.upsert_file(&FileRow::new("C:\\docs\\paged.pdf")).unwrap();

        db.set_doc(id, "paged.pdf", "page one\u{c}\u{c}page three", "page one\u{c}\u{c}page three")
            .unwrap();
        let first = db.search_pages_fts("three", 10).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, id);
        assert_eq!(first[0].1, 3);

        db.set_doc(id, "paged.pdf", "replacement", "replacement")
            .unwrap();
        assert!(db.search_pages_fts("three", 10).unwrap().is_empty());
        assert_eq!(db.search_pages_fts("replacement", 10).unwrap().len(), 1);
    }

    #[test]
    fn index_root_stats_are_isolated_for_similar_roots() {
        let db = Db::open(Path::new(":memory:")).unwrap();
        let roots = vec!["C:\\docs-a\\".to_string(), "C:\\docs-b".to_string()];
        db.sync_index_roots(&roots, &[]).unwrap();

        let mut first = FileRow::new("C:\\docs-a\\one.txt");
        first.size = 11;
        first.kind = "text".into();
        db.upsert_file(&first).unwrap();
        let mut second = FileRow::new("C:\\docs-a\\two.txt");
        second.size = 13;
        second.kind = "text".into();
        db.upsert_file(&second).unwrap();
        let mut other = FileRow::new("C:\\docs-b\\three.txt");
        other.size = 17;
        other.kind = "text".into();
        db.upsert_file(&other).unwrap();
        let mut sibling = FileRow::new("C:\\docs-ab\\outside.txt");
        sibling.size = 23;
        sibling.kind = "text".into();
        db.upsert_file(&sibling).unwrap();

        let stats = db.index_roots().unwrap();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].root, "C:\\docs-a\\");
        assert_eq!(stats[0].files, 2);
        assert_eq!(stats[0].bytes, 24);
        assert_eq!(stats[1].root, "C:\\docs-b");
        assert_eq!(stats[1].files, 1);
        assert_eq!(stats[1].bytes, 17);

        db.upsert_index_root("C:\\docs-a", 123, 2, 24, 1, 0, 0, 0, 0, 9, 99, "").unwrap();
        let stats = db.index_roots().unwrap();
        assert_eq!(stats[0].discovered_files, 9);
        assert_eq!(stats[0].discovered_bytes, 99);
    }
}
