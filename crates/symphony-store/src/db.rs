//! One small SQL interface over SQLite and PostgreSQL, so every operation (`ops`, `tasks`,
//! `leases`) is written once and tested against both databases.
//!
//! The dialect is the common subset: `?N` placeholders (rewritten to `$N` for PostgreSQL),
//! `RETURNING`, `ON CONFLICT ... DO UPDATE`, `CASE`, `COALESCE`, and `CAST(SUM(...) AS BIGINT)`
//! (PostgreSQL sums integers as `NUMERIC`). Values are integers (`BIGINT`), text or null;
//! timestamps are unix milliseconds and JSON is stored as text.

use std::collections::HashMap;

use crate::error::{Result, StoreError};

/// One bound or returned value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Val {
    Null,
    Int(i64),
    Text(String),
}

impl From<i64> for Val {
    fn from(value: i64) -> Self {
        Val::Int(value)
    }
}

impl From<u32> for Val {
    fn from(value: u32) -> Self {
        Val::Int(i64::from(value))
    }
}

impl From<&str> for Val {
    fn from(value: &str) -> Self {
        Val::Text(value.to_owned())
    }
}

impl From<String> for Val {
    fn from(value: String) -> Self {
        Val::Text(value)
    }
}

impl From<&String> for Val {
    fn from(value: &String) -> Self {
        Val::Text(value.clone())
    }
}

impl<T: Into<Val>> From<Option<T>> for Val {
    fn from(value: Option<T>) -> Self {
        value.map_or(Val::Null, Into::into)
    }
}

/// `[Val]` from a list of expressions.
macro_rules! vals {
    ($($value:expr),* $(,)?) => {
        [$($crate::db::Val::from($value)),*]
    };
}
pub(crate) use vals;

/// One result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row(pub(crate) Vec<Val>);

impl Row {
    fn get(&self, index: usize) -> Result<&Val> {
        self.0
            .get(index)
            .ok_or_else(|| StoreError::CorruptRow(format!("column {index} is missing")))
    }

    pub(crate) fn opt_int(&self, index: usize) -> Result<Option<i64>> {
        match self.get(index)? {
            Val::Null => Ok(None),
            Val::Int(value) => Ok(Some(*value)),
            Val::Text(_) => Err(StoreError::CorruptRow(format!(
                "column {index} is text, expected an integer"
            ))),
        }
    }

    pub(crate) fn int(&self, index: usize) -> Result<i64> {
        self.opt_int(index)?
            .ok_or_else(|| StoreError::CorruptRow(format!("column {index} is null")))
    }

    pub(crate) fn opt_text(&self, index: usize) -> Result<Option<String>> {
        match self.get(index)? {
            Val::Null => Ok(None),
            Val::Text(value) => Ok(Some(value.clone())),
            Val::Int(_) => Err(StoreError::CorruptRow(format!(
                "column {index} is an integer, expected text"
            ))),
        }
    }

    pub(crate) fn text(&self, index: usize) -> Result<String> {
        self.opt_text(index)?
            .ok_or_else(|| StoreError::CorruptRow(format!("column {index} is null")))
    }
}

/// Which database a connection talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    Sqlite,
    Postgres,
}

/// A connection the store's operations run on (always on the store's own thread).
pub(crate) trait Db: Send {
    /// Runs a statement; returns the number of changed rows.
    fn execute(&mut self, sql: &str, params: &[Val]) -> Result<u64>;
    /// Runs a query; returns every row.
    fn query(&mut self, sql: &str, params: &[Val]) -> Result<Vec<Row>>;
    /// Runs several statements without parameters (DDL).
    fn batch(&mut self, sql: &str) -> Result<()>;
    /// The database kind.
    fn backend(&self) -> Backend;

    /// The first row of a query.
    fn query_opt(&mut self, sql: &str, params: &[Val]) -> Result<Option<Row>> {
        Ok(self.query(sql, params)?.into_iter().next())
    }

    /// The single row of a query that always returns one (aggregates).
    fn query_one(&mut self, sql: &str, params: &[Val]) -> Result<Row> {
        self.query_opt(sql, params)?
            .ok_or_else(|| StoreError::Internal("the query returned no row".into()))
    }
}

/// Runs `body` in a transaction (SQLite takes the write lock up front), committing on `Ok` and
/// rolling back on `Err`.
pub(crate) fn in_transaction<T>(
    db: &mut dyn Db,
    body: impl FnOnce(&mut dyn Db) -> Result<T>,
) -> Result<T> {
    let begin = match db.backend() {
        Backend::Sqlite => "BEGIN IMMEDIATE",
        Backend::Postgres => "BEGIN",
    };
    db.batch(begin)?;
    match body(db) {
        Ok(value) => {
            db.batch("COMMIT")?;
            Ok(value)
        }
        Err(err) => {
            if let Err(rollback) = db.batch("ROLLBACK") {
                tracing::warn!(error = %rollback, "store rollback failed");
            }
            Err(err)
        }
    }
}

// ----- SQLite --------------------------------------------------------------------------------

/// [`Db`] over a rusqlite connection.
pub(crate) struct SqliteDb(pub(crate) rusqlite::Connection);

fn to_sqlite(value: &Val) -> rusqlite::types::Value {
    match value {
        Val::Null => rusqlite::types::Value::Null,
        Val::Int(value) => rusqlite::types::Value::Integer(*value),
        Val::Text(value) => rusqlite::types::Value::Text(value.clone()),
    }
}

impl Db for SqliteDb {
    fn execute(&mut self, sql: &str, params: &[Val]) -> Result<u64> {
        let mut statement = self.0.prepare_cached(sql)?;
        let changed =
            statement.execute(rusqlite::params_from_iter(params.iter().map(to_sqlite)))?;
        Ok(changed as u64)
    }

    fn query(&mut self, sql: &str, params: &[Val]) -> Result<Vec<Row>> {
        let mut statement = self.0.prepare_cached(sql)?;
        let columns = statement.column_count();
        let rows = statement.query_map(
            rusqlite::params_from_iter(params.iter().map(to_sqlite)),
            |row| {
                (0..columns)
                    .map(|i| {
                        Ok(match row.get::<_, rusqlite::types::Value>(i)? {
                            rusqlite::types::Value::Null => Val::Null,
                            rusqlite::types::Value::Integer(value) => Val::Int(value),
                            rusqlite::types::Value::Text(value) => Val::Text(value),
                            other => {
                                return Err(rusqlite::Error::FromSqlConversionFailure(
                                    i,
                                    other.data_type(),
                                    Box::new(StoreError::CorruptRow(format!(
                                        "column {i} has an unsupported type"
                                    ))),
                                ));
                            }
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map(Row)
            },
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn batch(&mut self, sql: &str) -> Result<()> {
        Ok(self.0.execute_batch(sql)?)
    }

    fn backend(&self) -> Backend {
        Backend::Sqlite
    }
}

// ----- PostgreSQL ----------------------------------------------------------------------------

/// `?N` placeholders as `$N` (outside string literals).
pub(crate) fn postgres_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut in_literal = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_literal = !in_literal;
                out.push(c);
            }
            '?' if !in_literal && chars.peek().is_some_and(char::is_ascii_digit) => out.push('$'),
            _ => out.push(c),
        }
    }
    out
}

/// [`Db`] over a synchronous PostgreSQL client, with a statement cache.
pub(crate) struct PostgresDb {
    pub(crate) client: postgres::Client,
    statements: HashMap<String, postgres::Statement>,
    /// Opens a new connection when the current one is lost (a database restart or failover, an
    /// idle connection dropped by the network).
    reconnect: Box<dyn FnMut() -> Result<postgres::Client> + Send>,
    /// Between `BEGIN` and `COMMIT`/`ROLLBACK`: a connection lost there takes the transaction
    /// with it, so the rest of it must fail rather than run on a new connection.
    in_transaction: bool,
    /// The last operation failed on the connection itself (not with an SQL error).
    lost: bool,
}

impl PostgresDb {
    pub(crate) fn new(
        client: postgres::Client,
        reconnect: Box<dyn FnMut() -> Result<postgres::Client> + Send>,
    ) -> Self {
        Self {
            client,
            statements: HashMap::new(),
            reconnect,
            in_transaction: false,
            lost: false,
        }
    }

    /// Replaces a lost connection (prepared statements belong to the old one).
    fn ensure_connected(&mut self) -> Result<()> {
        if !self.lost && !self.client.is_closed() {
            return Ok(());
        }
        if self.in_transaction {
            return Err(StoreError::Internal(
                "the PostgreSQL connection was lost during a transaction".into(),
            ));
        }
        self.client = (self.reconnect)()?;
        self.statements.clear();
        self.lost = false;
        tracing::warn!("Reconnected to PostgreSQL after the connection was lost");
        Ok(())
    }

    /// `result`, noting a failure of the connection itself (closed, an I/O error, or the
    /// server ending the session: SQLSTATE classes 08 and 57P) so the next operation reconnects.
    fn checked<T>(&mut self, result: std::result::Result<T, postgres::Error>) -> Result<T> {
        if let Err(err) = &result {
            let state = err.code().map(postgres::error::SqlState::code);
            self.lost |= err.is_closed()
                || self.client.is_closed()
                || match state {
                    Some(code) => code.starts_with("08") || code.starts_with("57P"),
                    None => err.as_db_error().is_none(),
                };
        }
        Ok(result?)
    }

    fn statement(&mut self, sql: &str) -> Result<postgres::Statement> {
        self.ensure_connected()?;
        if let Some(statement) = self.statements.get(sql) {
            return Ok(statement.clone());
        }
        let prepared = self.client.prepare(&postgres_sql(sql));
        let statement = self.checked(prepared)?;
        self.statements.insert(sql.to_owned(), statement.clone());
        Ok(statement)
    }
}

type PgParam = Box<dyn postgres::types::ToSql + Sync>;

fn to_postgres(value: &Val, ty: &postgres::types::Type) -> Result<PgParam> {
    use postgres::types::Type;
    let mismatch = || {
        StoreError::Internal(format!(
            "cannot bind {value:?} to a postgres {ty} parameter"
        ))
    };
    Ok(match (value, ty) {
        (Val::Null, &Type::INT8) => Box::new(None::<i64>),
        (Val::Null, &Type::INT4) => Box::new(None::<i32>),
        (Val::Null, _) => Box::new(None::<String>),
        (Val::Int(v), &Type::INT8) => Box::new(*v),
        (Val::Int(v), &Type::INT4) => Box::new(i32::try_from(*v).map_err(|_| mismatch())?),
        (Val::Int(v), &Type::TEXT | &Type::VARCHAR) => Box::new(v.to_string()),
        (Val::Text(v), &Type::TEXT | &Type::VARCHAR | &Type::BPCHAR | &Type::NAME) => {
            Box::new(v.clone())
        }
        _ => return Err(mismatch()),
    })
}

fn from_postgres(row: &postgres::Row, index: usize) -> Result<Val> {
    use postgres::types::Type;
    let ty = row.columns()[index].type_().clone();
    let value = match ty {
        Type::INT8 => row.try_get::<_, Option<i64>>(index)?.map(Val::Int),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(index)?
            .map(|v| Val::Int(i64::from(v))),
        Type::INT2 => row
            .try_get::<_, Option<i16>>(index)?
            .map(|v| Val::Int(i64::from(v))),
        Type::BOOL => row
            .try_get::<_, Option<bool>>(index)?
            .map(|v| Val::Int(i64::from(v))),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
            row.try_get::<_, Option<String>>(index)?.map(Val::Text)
        }
        other => {
            return Err(StoreError::CorruptRow(format!(
                "column {index} has the unsupported postgres type {other}"
            )));
        }
    };
    Ok(value.unwrap_or(Val::Null))
}

impl PostgresDb {
    fn bind(statement: &postgres::Statement, params: &[Val]) -> Result<Vec<PgParam>> {
        if statement.params().len() != params.len() {
            return Err(StoreError::Internal(format!(
                "the statement takes {} parameters, {} given",
                statement.params().len(),
                params.len()
            )));
        }
        statement
            .params()
            .iter()
            .zip(params)
            .map(|(ty, value)| to_postgres(value, ty))
            .collect()
    }
}

impl Db for PostgresDb {
    fn execute(&mut self, sql: &str, params: &[Val]) -> Result<u64> {
        let statement = self.statement(sql)?;
        let bound = Self::bind(&statement, params)?;
        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
            bound.iter().map(|b| b.as_ref()).collect();
        let executed = self.client.execute(&statement, &refs);
        self.checked(executed)
    }

    fn query(&mut self, sql: &str, params: &[Val]) -> Result<Vec<Row>> {
        let statement = self.statement(sql)?;
        let bound = Self::bind(&statement, params)?;
        let refs: Vec<&(dyn postgres::types::ToSql + Sync)> =
            bound.iter().map(|b| b.as_ref()).collect();
        let rows = self.client.query(&statement, &refs);
        self.checked(rows)?
            .iter()
            .map(|row| {
                (0..row.len())
                    .map(|i| from_postgres(row, i))
                    .collect::<Result<Vec<_>>>()
                    .map(Row)
            })
            .collect()
    }

    fn batch(&mut self, sql: &str) -> Result<()> {
        if matches!(sql, "COMMIT" | "ROLLBACK") {
            self.in_transaction = false;
            if self.lost || self.client.is_closed() {
                return Err(StoreError::Internal(
                    "the PostgreSQL connection was lost during a transaction".into(),
                ));
            }
        } else {
            self.ensure_connected()?;
        }
        let done = self.client.batch_execute(sql);
        self.checked(done)?;
        if sql == "BEGIN" {
            self.in_transaction = true;
        }
        Ok(())
    }

    fn backend(&self) -> Backend {
        Backend::Postgres
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_rewritten_outside_literals() {
        assert_eq!(
            postgres_sql("SELECT '?1', x FROM t WHERE a = ?1 AND b = ?12 AND c = '?' || ?2"),
            "SELECT '?1', x FROM t WHERE a = $1 AND b = $12 AND c = '?' || $2"
        );
    }

    #[test]
    fn sqlite_rows_round_trip_and_transactions_roll_back() {
        let mut db = SqliteDb(rusqlite::Connection::open_in_memory().unwrap());
        db.batch("CREATE TABLE t (a INTEGER, b TEXT)").unwrap();
        assert_eq!(
            db.execute("INSERT INTO t VALUES (?1, ?2)", &vals![7_i64, "x"])
                .unwrap(),
            1
        );
        db.execute(
            "INSERT INTO t VALUES (?1, ?2)",
            &vals![None::<i64>, None::<String>],
        )
        .unwrap();
        let rows = db.query("SELECT a, b FROM t ORDER BY a", &[]).unwrap();
        assert_eq!(rows[0].opt_int(0).unwrap(), None);
        assert_eq!(
            (rows[1].int(0).unwrap(), rows[1].text(1).unwrap()),
            (7, "x".into())
        );
        assert!(rows[1].text(0).is_err(), "types are checked");
        let failed: Result<()> = in_transaction(&mut db, |db| {
            db.execute("DELETE FROM t", &[])?;
            Err(StoreError::Internal("boom".into()))
        });
        assert!(failed.is_err());
        assert_eq!(
            db.query("SELECT a FROM t", &[]).unwrap().len(),
            2,
            "rolled back"
        );
    }
}
