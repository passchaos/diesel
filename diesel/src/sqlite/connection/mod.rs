extern crate libsqlite3_sys as ffi;

#[doc(hidden)]
pub mod raw;
mod stmt;
mod statement_iterator;
mod sqlite_value;

pub use self::sqlite_value::SqliteValue;

use std::time::{Instant, Duration};
use std::os::raw as libc;
use std::rc::Rc;

use connection::*;
use query_builder::*;
use query_builder::bind_collector::RawBytesBindCollector;
use query_source::*;
use result::*;
use self::raw::RawConnection;
use self::statement_iterator::StatementIterator;
use self::stmt::{Statement, StatementUse};
use sqlite::{Sqlite, SqliteQueryBuilder};
use types::HasSqlType;

#[allow(missing_debug_implementations)]
pub struct SqliteConnection {
    statement_cache: StatementCache<Sqlite, Statement>,
    raw_connection: Rc<RawConnection>,
    transaction_manager: AnsiTransactionManager,
    is_log_query: bool,
    is_explain_query: bool,
}

// This relies on the invariant that RawConnection or Statement are never
// leaked. If a reference to one of those was held on a different thread, this
// would not be thread safe.
unsafe impl Send for SqliteConnection {}

impl SimpleConnection for SqliteConnection {
    fn batch_execute(&self, query: &str) -> QueryResult<()> {
        self.raw_connection.exec(query)
    }
}

impl Connection for SqliteConnection {
    type Backend = Sqlite;
    type TransactionManager = AnsiTransactionManager;

    fn establish(database_url: &str, config: Config) -> ConnectionResult<Self> {
        let password = config.password.clone();
        RawConnection::establish(database_url, password).map(|conn| {
            SqliteConnection {
                statement_cache: StatementCache::new(),
                raw_connection: Rc::new(conn),
                transaction_manager: AnsiTransactionManager::new(),
                is_log_query: config.is_log_query,
                is_explain_query: config.is_explain_query,
            }
        })
    }

    #[doc(hidden)]
    fn execute(&self, query: &str) -> QueryResult<usize> {
        let _logger = QueryLogger::new(query, self.is_log_query);
        try!(self.explain_query(query));
        try!(self.batch_execute(query));

        Ok(self.raw_connection.rows_affected_by_last_query())
    }

    #[doc(hidden)]
    fn query_by_index<T, U>(&self, source: T) -> QueryResult<Vec<U>>
    where
        T: AsQuery,
        T::Query: QueryFragment<Self::Backend> + QueryId,
        Self::Backend: HasSqlType<T::SqlType>,
        U: Queryable<T::SqlType, Self::Backend>,
    {
        let query = source.as_query();
        let mut statement = try!(self.prepare_query(&query));
        let statement_use = StatementUse::new(&mut statement);
        if self.is_log_query || self.is_explain_query {
            let query = try!(self.construct_and_explain_query(&query));
            let _query_logger = QueryLogger::new(&query, self.is_log_query);
        }
        let iter = StatementIterator::new(statement_use);
        iter.collect()
    }

    #[doc(hidden)]
    fn execute_returning_count<T>(&self, source: &T) -> QueryResult<usize>
    where
        T: QueryFragment<Self::Backend> + QueryId,
    {
        let mut statement = try!(self.prepare_query(source));
        let statement_use = StatementUse::new(&mut statement);
        if self.is_log_query || self.is_explain_query {
            let query = try!(self.construct_and_explain_query(source));
            let _query_logger = QueryLogger::new(&query, self.is_log_query);
        }
        try!(statement_use.run());
        Ok(self.raw_connection.rows_affected_by_last_query())
    }

    #[doc(hidden)]
    fn silence_notices<F: FnOnce() -> T, T>(&self, f: F) -> T {
        f()
    }

    #[doc(hidden)]
    fn transaction_manager(&self) -> &Self::TransactionManager {
        &self.transaction_manager
    }
}

impl SqliteConnection {
    fn prepare_query<T: QueryFragment<Sqlite> + QueryId>(
        &self,
        source: &T,
    ) -> QueryResult<MaybeCached<Statement>> {
        let mut statement = try!(self.cached_prepared_statement(source));

        let mut bind_collector = RawBytesBindCollector::<Sqlite>::new();
        try!(source.collect_binds(&mut bind_collector, &()));
        let metadata = bind_collector.metadata;
        let binds = bind_collector.binds;
        for (tpe, value) in metadata.into_iter().zip(binds) {
            try!(statement.bind(tpe, value));
        }

        Ok(statement)
    }

    fn cached_prepared_statement<T: QueryFragment<Sqlite> + QueryId>(
        &self,
        source: &T,
    ) -> QueryResult<MaybeCached<Statement>> {
        self.statement_cache.cached_statement(
            source,
            &[],
            |sql| Statement::prepare(&self.raw_connection, sql),
        )
    }

    pub fn change_password(&self, password: &str) -> QueryResult<()> {
        match self.raw_connection.rekey(password)? {
            ffi::SQLITE_OK => Ok(()),
            err_code => {
                let message = error_message(err_code);
                Err(Error::DatabaseError(DatabaseErrorKind::UnableToReEncrypt ,Box::new(message.to_string())))
            }
        }
    }

    /// Return String
    /// Elements in each row are separated by delimiter
    /// Rows are separated by `\n`
    pub fn execute_for_string(&self, query: &str, delimiter: &str) -> QueryResult<String> {
        self.raw_connection.execute_for_string(query, delimiter)
    }

    fn construct_and_explain_query(&self, source: &QueryFragment<Sqlite>) -> QueryResult<String> {
        let mut query_builder = SqliteQueryBuilder::new();
        try!(source.to_sql(&mut query_builder));
        let query = query_builder.finish();
        try!(self.explain_query(&query));

        Ok(query)
    }

    fn explain_query(&self, query: &str) -> QueryResult<()> {
        if query == "SELECT 1" {
            return Ok(())
        }
        if !self.is_explain_query {
            return Ok(())
        }
        let explain_query = format!("EXPLAIN QUERY PLAN {}", query);
        let explain = try!(self.execute_for_string(&explain_query, "|"));
        debug!("explain query: query= {}\nexplain= \n{}", query, explain);

        Ok(())
    }
}

#[derive(Default)]
struct QueryLogger {
    query: Option<String>,
    st: Option<Instant>,
}

impl QueryLogger {
    fn new(query: &str, is_log: bool) -> Self {
        if query == "SELECT 1" {
            return QueryLogger::default()
        }

        match is_log {
            true => {
                let st = Instant::now();
                QueryLogger {
                    query: Some(query.to_owned()),
                    st: Some(st),
                }
            }
            false => QueryLogger::default()
        }
    }
}

impl Drop for QueryLogger {
    fn drop(&mut self) {
        if let Some(st) = self.st {
            let duration = get_duration_millisecond(st.elapsed());
            debug!("execute query end: query= {:?} st={:?} cost= {:?}ms",
                   self.query, self.st, duration);
        }
    }
}

fn get_duration_millisecond(duration: Duration) -> u64
{
    (duration.as_secs() * 1_000) + (duration.subsec_nanos() / 1_000_000) as u64
}

fn error_message(err_code: libc::c_int) -> &'static str {
    ffi::code_to_str(err_code)
}

#[cfg(test)]
mod tests {
    use expression::AsExpression;
    use dsl::sql;
    use prelude::*;
    use super::*;
    use types::Integer;

    #[test]
    fn prepared_statements_are_cached_when_run() {
        let connection = SqliteConnection::establish(":memory:", Config::default()).unwrap();
        let query = ::select(AsExpression::<Integer>::as_expression(1));

        assert_eq!(Ok(1), query.get_result(&connection));
        assert_eq!(Ok(1), query.get_result(&connection));
        assert_eq!(1, connection.statement_cache.len());
    }

    #[test]
    fn sql_literal_nodes_are_not_cached() {
        let connection = SqliteConnection::establish(":memory:", Config::default()).unwrap();
        let query = ::select(sql::<Integer>("1"));

        assert_eq!(Ok(1), query.get_result(&connection));
        assert_eq!(0, connection.statement_cache.len());
    }

    #[test]
    fn queries_containing_sql_literal_nodes_are_not_cached() {
        let connection = SqliteConnection::establish(":memory:", Config::default()).unwrap();
        let one_as_expr = AsExpression::<Integer>::as_expression(1);
        let query = ::select(one_as_expr.eq(sql::<Integer>("1")));

        assert_eq!(Ok(true), query.get_result(&connection));
        assert_eq!(0, connection.statement_cache.len());
    }

    #[test]
    fn queries_containing_in_with_vec_are_not_cached() {
        let connection = SqliteConnection::establish(":memory:", Config::default()).unwrap();
        let one_as_expr = AsExpression::<Integer>::as_expression(1);
        let query = ::select(one_as_expr.eq_any(vec![1, 2, 3]));

        assert_eq!(Ok(true), query.get_result(&connection));
        assert_eq!(0, connection.statement_cache.len());
    }

    #[test]
    fn queries_containing_in_with_subselect_are_cached() {
        let connection = SqliteConnection::establish(":memory:", Config::default()).unwrap();
        let one_as_expr = AsExpression::<Integer>::as_expression(1);
        let query = ::select(one_as_expr.eq_any(::select(one_as_expr)));

        assert_eq!(Ok(true), query.get_result(&connection));
        assert_eq!(1, connection.statement_cache.len());
    }

    #[test]
    fn test_execute_for_string1() {
        let connection = SqliteConnection::establish(":memory:", Config::default()).unwrap();
        let query = "PRAGMA JOURNAL_MODE";
        let result = connection.execute_for_string(query, "").unwrap();

        assert_eq!("memory", result);
    }

    #[test]
    fn test_execute_for_string2() {
        let connection = SqliteConnection::establish(":memory:", Config::default()).unwrap();
        let query = "PRAGMA LOCKING_MODE";
        let result = connection.execute_for_string(query, "").unwrap();

        assert_eq!("normal", result);
    }

    #[test]
    fn test_execute_for_string3() {
        let connection = SqliteConnection::establish(":memory:", Config::default()).unwrap();
        let query = "PRAGMA CACHE_SIZE";
        let result = connection.execute_for_string(query, "").unwrap();

        assert_eq!("-2000", result);
    }
}
