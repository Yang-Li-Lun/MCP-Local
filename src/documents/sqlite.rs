use std::{
    borrow::Cow,
    collections::HashSet,
    ffi::{CStr, CString, c_char, c_int, c_uint, c_void},
    ptr::{null, null_mut},
    sync::OnceLock,
};

use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use libloading::os::windows::Library;
use serde::Serialize;
use serde_json::{Value, json};
use windows_sys::Win32::System::LibraryLoader::LOAD_LIBRARY_SEARCH_SYSTEM32;

use super::{MAX_UNITS, Page, spaced_json};

const INVALID: &str = "DOCUMENT_SQLITE：SQLite 快照無效或不支援。";
const LIMIT: &str = "DOCUMENT_SQLITE_LIMIT：SQLite 資源或資料量超限。";
const VIRTUAL_UNKNOWN: &str = "DOCUMENT_SQLITE：無法安全辨識此虛擬資料表的內部資料表。";
pub(super) const MAX_VALUE: c_int = 1024 * 1024;
const MAX_COLUMNS: c_int = 256;
const MAX_SQL_BYTES: c_int = 65_536;
const MAX_TABLES: usize = 1024;
const VM_INSTRUCTIONS_PER_CHECK: c_int = 1000;
const MAX_VM_CHECKS: u32 = 20_000;
const MIN_VERSION: c_int = 3_037_000;

const OK: c_int = 0;
const INTERRUPT: c_int = 9;
const TOO_BIG: c_int = 18;
const ROW: c_int = 100;
const DONE: c_int = 101;
const DESERIALIZE_READONLY: c_uint = 4;
const OPEN_MEMORY: c_int = 0x80;
const OPEN_READWRITE: c_int = 2;
const OPEN_CREATE: c_int = 4;
const OPEN_PRIVATECACHE: c_int = 0x40000;
const PREPARE_NO_VTAB: c_uint = 4;

type Db = *mut c_void;
type Stmt = *mut c_void;
type Progress = unsafe extern "C" fn(*mut c_void) -> c_int;
type Authorizer = unsafe extern "C" fn(
    *mut c_void,
    c_int,
    *const c_char,
    *const c_char,
    *const c_char,
    *const c_char,
) -> c_int;

macro_rules! api {
    ($($field:ident : $symbol:literal = $signature:ty),+ $(,)?) => {
        struct Api {
            _library: Library,
            $($field: $signature,)+
        }
        impl Api {
            fn load() -> Result<Self> {
                // Only the OS component from System32 is eligible. Keep it mapped for every pointer.
                let library = unsafe { Library::load_with_flags("winsqlite3.dll", LOAD_LIBRARY_SEARCH_SYSTEM32) }?;
                $(let $field: $signature = *unsafe { library.get(concat!($symbol, "\0").as_bytes()) }?;)+
                let api = Self { _library: library, $($field,)+ };
                ensure!(unsafe { (api.version)() } >= MIN_VERSION, "SQLite version too old");
                Ok(api)
            }
        }
    };
}

api! {
    version: "sqlite3_libversion_number" = unsafe extern "C" fn() -> c_int,
    open: "sqlite3_open_v2" = unsafe extern "C" fn(*const c_char, *mut Db, c_int, *const c_char) -> c_int,
    close: "sqlite3_close" = unsafe extern "C" fn(Db) -> c_int,
    config: "sqlite3_db_config" = unsafe extern "C" fn(Db, c_int, ...) -> c_int,
    deserialize: "sqlite3_deserialize" = unsafe extern "C" fn(Db, *const c_char, *mut u8, i64, i64, c_uint) -> c_int,
    limit: "sqlite3_limit" = unsafe extern "C" fn(Db, c_int, c_int) -> c_int,
    progress: "sqlite3_progress_handler" = unsafe extern "C" fn(Db, c_int, Option<Progress>, *mut c_void),
    authorizer: "sqlite3_set_authorizer" = unsafe extern "C" fn(Db, Option<Authorizer>, *mut c_void) -> c_int,
    drop_modules: "sqlite3_drop_modules" = unsafe extern "C" fn(Db, *const *const c_char) -> c_int,
    prepare: "sqlite3_prepare_v3" = unsafe extern "C" fn(Db, *const c_char, c_int, c_uint, *mut Stmt, *mut *const c_char) -> c_int,
    readonly: "sqlite3_stmt_readonly" = unsafe extern "C" fn(Stmt) -> c_int,
    step: "sqlite3_step" = unsafe extern "C" fn(Stmt) -> c_int,
    finalize: "sqlite3_finalize" = unsafe extern "C" fn(Stmt) -> c_int,
    count: "sqlite3_column_count" = unsafe extern "C" fn(Stmt) -> c_int,
    kind: "sqlite3_column_type" = unsafe extern "C" fn(Stmt, c_int) -> c_int,
    integer: "sqlite3_column_int64" = unsafe extern "C" fn(Stmt, c_int) -> i64,
    number: "sqlite3_column_double" = unsafe extern "C" fn(Stmt, c_int) -> f64,
    bytes: "sqlite3_column_bytes" = unsafe extern "C" fn(Stmt, c_int) -> c_int,
    text: "sqlite3_column_text" = unsafe extern "C" fn(Stmt, c_int) -> *const u8,
    blob: "sqlite3_column_blob" = unsafe extern "C" fn(Stmt, c_int) -> *const c_void,
}

pub(super) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| Api::load().is_ok())
}

fn check(code: c_int) -> Result<()> {
    match code {
        OK => Ok(()),
        INTERRUPT | TOO_BIG => bail!("{LIMIT}"),
        _ => bail!("{INVALID}"),
    }
}

struct Connection<'a> {
    api: &'a Api,
    db: Db,
    _data: Vec<u8>,
    _vm_checks: Box<u32>,
}

impl Drop for Connection<'_> {
    fn drop(&mut self) {
        if !self.db.is_null() {
            // Statements borrow this connection, and the snapshot remains allocated until close.
            unsafe {
                (self.api.close)(self.db);
            }
        }
    }
}

unsafe extern "C" fn progress(opaque: *mut c_void) -> c_int {
    let count = unsafe { &mut *opaque.cast::<u32>() };
    *count += 1;
    c_int::from(*count > MAX_VM_CHECKS)
}

unsafe extern "C" fn authorize(
    _: *mut c_void,
    action: c_int,
    first: *const c_char,
    _: *const c_char,
    database: *const c_char,
    trigger: *const c_char,
) -> c_int {
    const SQLITE_PRAGMA: c_int = 19;
    const SQLITE_READ: c_int = 20;
    const SQLITE_SELECT: c_int = 21;
    const SQLITE_DENY: c_int = 1;
    if !trigger.is_null() {
        return SQLITE_DENY;
    }
    match action {
        SQLITE_SELECT => OK,
        SQLITE_READ if !database.is_null() && unsafe { CStr::from_ptr(database) } == c"main" => OK,
        SQLITE_PRAGMA if !first.is_null() && unsafe { CStr::from_ptr(first) } == c"table_xinfo" => {
            OK
        }
        _ => SQLITE_DENY,
    }
}

impl<'a> Connection<'a> {
    fn new(api: &'a Api, snapshot: &[u8]) -> Result<Self> {
        // The C interface expects about 20 extra readable bytes for malformed snapshots.
        let mut data = Vec::with_capacity(snapshot.len() + 20);
        data.extend_from_slice(snapshot);
        data.resize(snapshot.len() + 20, 0);
        let mut db = null_mut();
        let flags = OPEN_MEMORY | OPEN_READWRITE | OPEN_CREATE | OPEN_PRIVATECACHE;
        let code = unsafe { (api.open)(c":memory:".as_ptr(), &mut db, flags, null()) };
        let mut connection = Self {
            api,
            db,
            _data: data,
            _vm_checks: Box::new(0),
        };
        check(code)?;
        ensure!(!db.is_null(), "{INVALID}");
        for (category, maximum) in [
            (0, MAX_VALUE),
            (1, MAX_SQL_BYTES),
            (2, MAX_COLUMNS),
            (3, 64),
            (7, 0),
        ] {
            unsafe {
                (api.limit)(db, category, maximum);
            }
            let actual = unsafe { (api.limit)(db, category, -1) };
            ensure!(
                (0..=maximum).contains(&actual) && (category != 3 || actual > 0),
                "{LIMIT}"
            );
        }
        // Disable triggers, extensions, double-quoted-string fallback, views, and trusted
        // schema. Defensive mode and the read-only deserialize flag independently block writes.
        for (option, value) in [
            (1003, 0),
            (1005, 0),
            (1010, 1),
            (1013, 0),
            (1014, 0),
            (1015, 0),
            (1017, 0),
        ] {
            let mut actual = -1;
            check(unsafe { (api.config)(db, option, value, &mut actual as *mut c_int) })?;
            ensure!(actual == value, "{INVALID}");
        }
        check(unsafe { (api.drop_modules)(db, null()) })?;
        let (address, length) = (connection._data.as_mut_ptr(), connection._data.len() as i64);
        check(unsafe {
            (api.deserialize)(
                db,
                c"main".as_ptr(),
                address,
                snapshot.len() as i64,
                length,
                DESERIALIZE_READONLY,
            )
        })?;
        ensure!(
            connection
                .statement("PRAGMA temp_store=MEMORY")?
                .row()?
                .is_none(),
            "{INVALID}"
        );
        ensure!(
            connection
                .statement("PRAGMA query_only=ON")?
                .row()?
                .is_none(),
            "{INVALID}"
        );
        check(unsafe { (api.authorizer)(db, Some(authorize), null_mut()) })?;
        unsafe {
            (api.progress)(
                db,
                VM_INSTRUCTIONS_PER_CHECK,
                Some(progress),
                (&mut *connection._vm_checks as *mut u32).cast(),
            );
        }
        Ok(connection)
    }

    fn statement(&self, sql: &str) -> Result<Statement<'_>> {
        let sql = CString::new(sql).map_err(|_| anyhow::anyhow!("{INVALID}"))?;
        let mut stmt = null_mut();
        let code = unsafe {
            (self.api.prepare)(
                self.db,
                sql.as_ptr(),
                -1,
                PREPARE_NO_VTAB,
                &mut stmt,
                null_mut(),
            )
        };
        let statement = Statement {
            api: self.api,
            stmt,
        };
        check(code)?;
        ensure!(
            !stmt.is_null() && unsafe { (self.api.readonly)(stmt) } != 0,
            "{INVALID}"
        );
        Ok(statement)
    }
}

struct Statement<'a> {
    api: &'a Api,
    stmt: Stmt,
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        if !self.stmt.is_null() {
            unsafe {
                (self.api.finalize)(self.stmt);
            }
        }
    }
}

impl Statement<'_> {
    fn row(&mut self) -> Result<Option<Vec<Value>>> {
        match unsafe { (self.api.step)(self.stmt) } {
            DONE => Ok(None),
            ROW => {
                let count = unsafe { (self.api.count)(self.stmt) };
                ensure!((0..=MAX_COLUMNS).contains(&count), "{LIMIT}");
                Ok(Some(
                    (0..count)
                        .map(|column| self.value(column))
                        .collect::<Result<_>>()?,
                ))
            }
            code => {
                check(code)?;
                bail!("{INVALID}");
            }
        }
    }

    fn value(&self, column: c_int) -> Result<Value> {
        let kind = unsafe { (self.api.kind)(self.stmt, column) };
        Ok(match kind {
            1 => json!(unsafe { (self.api.integer)(self.stmt, column) }),
            2 => {
                let number = unsafe { (self.api.number)(self.stmt, column) };
                ensure!(number.is_finite(), "DOCUMENT_NUMBER：非有限 SQLite 數字。");
                json!(number)
            }
            3 => {
                let ptr = unsafe { (self.api.text)(self.stmt, column) };
                json!(std::str::from_utf8(self.bytes(column, ptr)?)?)
            }
            4 => {
                let ptr = unsafe { (self.api.blob)(self.stmt, column) }.cast();
                json!({"type":"blob","encoding":"base64","data":STANDARD.encode(self.bytes(column, ptr)?)})
            }
            5 => Value::Null,
            _ => bail!("{INVALID}"),
        })
    }

    fn bytes(&self, column: c_int, ptr: *const u8) -> Result<&[u8]> {
        let length = unsafe { (self.api.bytes)(self.stmt, column) };
        ensure!((0..=MAX_VALUE).contains(&length), "{LIMIT}");
        if length == 0 {
            return Ok(&[]);
        }
        ensure!(!ptr.is_null(), "{INVALID}");
        // Column bytes are consumed before the next conversion, step, or reset.
        Ok(unsafe { std::slice::from_raw_parts(ptr, length as usize) })
    }
}

#[derive(Serialize)]
pub(super) struct Column {
    name: String,
    declared_type: String,
    primary_key: i64,
}

fn name(value: &Value) -> Result<&str> {
    let value = value.as_str().ok_or_else(|| anyhow::anyhow!("{INVALID}"))?;
    check_name(value)?;
    Ok(value)
}

fn check_name(value: &str) -> Result<()> {
    ensure!(
        !value.contains('\0') && value.chars().count() <= 256,
        "{LIMIT}"
    );
    Ok(())
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn schema_token<'a>(source: &mut &'a str) -> Option<&'a str> {
    loop {
        *source = source.trim_ascii_start();
        if let Some(comment) = source.strip_prefix("--") {
            *source = comment.split_once('\n')?.1;
        } else if let Some(comment) = source.strip_prefix("/*") {
            *source = comment.split_once("*/")?.1;
        } else {
            break;
        }
    }

    let bytes = source.as_bytes();
    let end = match *bytes.first()? {
        b'.' => 1,
        b'[' => source.find(']')? + 1,
        quote @ (b'\'' | b'"' | b'`') => {
            let mut offset = 1;
            loop {
                offset += bytes
                    .get(offset..)?
                    .iter()
                    .position(|byte| *byte == quote)?
                    + 1;
                if bytes.get(offset) != Some(&quote) {
                    break offset;
                }
                offset += 1;
            }
        }
        _ => bytes
            .iter()
            .take_while(|&&byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') || !byte.is_ascii()
            })
            .count(),
    };
    if end == 0 {
        return None;
    }
    let (token, rest) = source.split_at(end);
    *source = rest;
    Some(token)
}

fn identifier(token: &str) -> Cow<'_, str> {
    match token.as_bytes()[0] {
        b'[' => Cow::Borrowed(&token[1..token.len() - 1]),
        b'"' => Cow::Owned(token[1..token.len() - 1].replace("\"\"", "\"")),
        b'\'' => Cow::Owned(token[1..token.len() - 1].replace("''", "'")),
        b'`' => Cow::Owned(token[1..token.len() - 1].replace("``", "`")),
        _ => Cow::Borrowed(token),
    }
}

fn virtual_module<'a>(mut declaration: &'a str, table: &str) -> Option<Cow<'a, str>> {
    for expected in ["CREATE", "VIRTUAL", "TABLE"] {
        if !schema_token(&mut declaration)?.eq_ignore_ascii_case(expected) {
            return None;
        }
    }
    let mut name = schema_token(&mut declaration)?;
    if name.eq_ignore_ascii_case("IF") {
        if !schema_token(&mut declaration)?.eq_ignore_ascii_case("NOT")
            || !schema_token(&mut declaration)?.eq_ignore_ascii_case("EXISTS")
        {
            return None;
        }
        name = schema_token(&mut declaration)?;
    }
    let mut using = schema_token(&mut declaration)?;
    if using == "." {
        if !identifier(name).eq_ignore_ascii_case("main") {
            return None;
        }
        name = schema_token(&mut declaration)?;
        using = schema_token(&mut declaration)?;
    }
    if !identifier(name).eq_ignore_ascii_case(table) || !using.eq_ignore_ascii_case("USING") {
        return None;
    }
    Some(identifier(schema_token(&mut declaration)?))
}

fn shadow_suffixes(module: &str) -> Option<&'static [&'static str]> {
    // These are the built-in modules' xShadowName suffixes. Classify the
    // schema without restoring modules or invoking any virtual-table callbacks.
    match module.to_ascii_lowercase().as_str() {
        "fts3" | "fts4" => Some(&["content", "docsize", "segdir", "segments", "stat"]),
        "fts5" => Some(&["config", "content", "data", "docsize", "idx"]),
        "rtree" | "rtree_i32" | "geopoly" => Some(&["node", "parent", "rowid"]),
        _ => None,
    }
}

fn ordinary_tables(db: &Connection<'_>) -> Result<Vec<String>> {
    let mut catalog = db.statement(
        "SELECT type, name, rootpage, CASE WHEN rootpage = 0 AND type = 'table' THEN sql END \
         FROM main.sqlite_schema WHERE type IN ('table','view') LIMIT 1025",
    )?;
    let mut entries = 0;
    let mut tables = Vec::new();
    let mut shadows = HashSet::new();
    while let Some(row) = catalog.row()? {
        entries += 1;
        ensure!(entries <= MAX_TABLES, "{LIMIT}");
        if row[0] != "table" {
            continue;
        }
        let name = row[1].as_str().ok_or_else(|| anyhow::anyhow!(INVALID))?;
        match row[2].as_i64() {
            Some(0) => {
                let declaration = row[3]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!(VIRTUAL_UNKNOWN))?;
                ensure!(declaration.len() <= MAX_SQL_BYTES as usize, "{LIMIT}");
                let module = virtual_module(declaration, name)
                    .ok_or_else(|| anyhow::anyhow!(VIRTUAL_UNKNOWN))?;
                let suffixes =
                    shadow_suffixes(&module).ok_or_else(|| anyhow::anyhow!(VIRTUAL_UNKNOWN))?;
                let base = self::name(&row[1])?.to_ascii_lowercase();
                shadows.extend(suffixes.iter().map(|suffix| format!("{base}_{suffix}")));
            }
            Some(root) if root > 0 => tables.push(name.to_owned()),
            _ => bail!("{INVALID}"),
        }
    }
    tables.retain(|name| {
        !name.starts_with("sqlite_") && !shadows.contains(&name.to_ascii_lowercase())
    });
    for name in &tables {
        check_name(name)?;
    }
    tables.sort_unstable();
    Ok(tables)
}

pub(super) fn read(
    snapshot: &[u8],
    table: Option<&str>,
    page: &mut Page,
) -> Result<Option<Vec<Column>>> {
    ensure!(
        snapshot.get(18..20) == Some(b"\x01\x01") && snapshot.len() >= 100,
        "DOCUMENT_SQLITE：拒絕 WAL 或無效 SQLite 快照；需獨立完整資料庫。"
    );
    let encoded = u16::from_be_bytes([snapshot[16], snapshot[17]]);
    let size = if encoded == 1 {
        65_536
    } else {
        usize::from(encoded)
    };
    ensure!(
        (512..=65_536).contains(&size) && size.is_power_of_two() && snapshot.len() % size == 0,
        "{INVALID}"
    );
    let api = Api::load().map_err(|_| {
        anyhow::anyhow!("DOCUMENT_RUNTIME：此 Windows SQLite 元件缺少安全快照 API。")
    })?;
    let db = Connection::new(&api, snapshot)?;
    for name in ordinary_tables(&db)? {
        if table.is_some_and(|table| table != name) {
            continue;
        }
        let mut details = db.statement(&format!("PRAGMA main.table_xinfo({})", quoted(&name)))?;
        let mut columns = Vec::new();
        let mut hidden = false;
        while let Some(row) = details.row()? {
            if row[6] != 0 {
                hidden = true;
            }
            columns.push(Column {
                name: self::name(&row[1])?.into(),
                declared_type: self::name(&row[2])?.into(),
                primary_key: row[5]
                    .as_i64()
                    .ok_or_else(|| anyhow::anyhow!("{INVALID}"))?,
            });
        }
        ensure!(
            columns.len() <= MAX_COLUMNS as usize && !columns.is_empty(),
            "{LIMIT}"
        );
        if hidden {
            ensure!(
                table.is_none(),
                "DOCUMENT_SQLITE：不讀取 generated/hidden 欄位資料表。"
            );
            continue;
        }
        if table.is_none() {
            page.add(json!({"table": name}), &spaced_json(&columns)?)?;
            continue;
        }
        let selected = columns
            .iter()
            .map(|column| quoted(&column.name))
            .collect::<Vec<_>>()
            .join(",");
        let mut rows = db.statement(&format!(
            "SELECT {selected} FROM main.{} LIMIT 100001",
            quoted(&name)
        ))?;
        let mut index = 0;
        while let Some(values) = rows.row()? {
            ensure!(index < MAX_UNITS, "{LIMIT}");
            page.add(json!({"table":name, "row":index}), &spaced_json(&values)?)?;
            index += 1;
        }
        return Ok(Some(columns));
    }
    ensure!(
        table.is_none(),
        "DOCUMENT_SQLITE：不存在可讀取的一般資料表。"
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_module_comes_from_the_schema_clause_and_matches_its_table() {
        let statements = [
            ("CREATE VIRTUAL TABLE demo USING FTS5(text)", "DEMO", "FTS5"),
            (
                "CREATE /* note */ VIRTUAL TABLE IF NOT EXISTS [查詢 USING] USING /* tag */ [fts4](text)",
                "查詢 USING",
                "fts4",
            ),
            (
                "CREATE VIRTUAL TABLE main.\"a\"\"b\" -- checked name\n USING `rtree_i32`(id,x,y)",
                "a\"b",
                "rtree_i32",
            ),
            (
                "CREATE VIRTUAL TABLE 'a''b' USING 'geopoly'",
                "a'b",
                "geopoly",
            ),
        ];
        for (sql, name, expected) in statements {
            assert_eq!(virtual_module(sql, name).as_deref(), Some(expected));
            assert!(shadow_suffixes(expected).is_some());
            assert!(virtual_module(sql, "different name").is_none());
        }
        for sql in [
            "CREATE TABLE t(fts5)",
            "CREATE VIRTUAL TABLE t USING /* unfinished",
            "CREATE VIRTUAL TABLE t USING 'fts5",
            "CREATE VIRTUAL TABLE other.t USING fts5",
        ] {
            assert!(virtual_module(sql, "t").is_none());
        }
        let module = virtual_module("CREATE VIRTUAL TABLE t USING custom_module", "t").unwrap();
        assert!(shadow_suffixes(&module).is_none());
    }
}
