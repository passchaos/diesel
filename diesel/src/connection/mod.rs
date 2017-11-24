mod statement_cache;
mod transaction_manager;

use backend::Backend;
use query_builder::{AsQuery, QueryFragment, QueryId};
use query_source::Queryable;
use result::*;
use types::HasSqlType;

pub use self::transaction_manager::{AnsiTransactionManager, TransactionManager};
#[doc(hidden)]
pub use self::statement_cache::{MaybeCached, StatementCache, StatementCacheKey};

/// Perform simple operations on a backend.
pub trait SimpleConnection {
    /// Execute multiple SQL statements within the same string.
    ///
    /// This function is typically used in migrations where the statements to upgrade or
    /// downgrade the database are stored in SQL batch files.
    fn batch_execute(&self, query: &str) -> QueryResult<()>;
}

#[derive(Debug, Clone)]
pub struct Config {
    pub password: Option<String>,
    pub is_log_query: bool,
    pub is_explain_query: bool,
}

impl Config {
    pub fn builder() -> ConfigBuilder {
        ConfigBuilder::default()
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            password: None,
            is_log_query: false,
            is_explain_query: false,
        }
    }
}

#[derive(Default, Debug)]
pub struct ConfigBuilder {
    config: Config
}

impl ConfigBuilder {
    /// The password for this database
    /// default to None
    pub fn password(mut self, password: &str) -> Self {
        self.config.password = Some(password.to_owned());
        self
    }

    /// Is log query, start time, time cost for any execute
    /// default to false
    pub fn is_log_query(mut self, is_log_query: bool) -> Self {
        self.config.is_log_query = is_log_query;
        self
    }

    /// Is explain each query
    /// default to false
    pub fn is_explain_query(mut self, is_explain_query: bool) -> Self {
        self.config.is_explain_query = is_explain_query;
        self
    }

    pub fn build(self) -> Config {
        self.config
    }
}

/// Perform connections to a backend.
pub trait Connection: SimpleConnection + Sized + Send {
    /// The backend this connection represents.
    type Backend: Backend;
    #[doc(hidden)]
    type TransactionManager: TransactionManager<Self>;

    /// Establishes a new connection to the database at the given URL. The URL
    /// should be a valid connection string for a given backend. See the
    /// documentation for the specific backend for specifics.
    fn establish(database_url: &str, config: Config) -> ConnectionResult<Self>;

    /// Executes the given function inside of a database transaction. When
    /// a transaction is already occurring, savepoints will be used to emulate a nested
    /// transaction.
    ///
    /// The error returned from the function must implement
    /// `From<diesel::result::Error>`.
    ///
    /// # Examples:
    ///
    /// ```rust
    /// # #[macro_use] extern crate diesel;
    /// # #[macro_use] extern crate diesel_codegen;
    /// # include!("../doctest_setup.rs");
    /// # table!(
    /// #    users(id) {
    /// #        id -> Integer,
    /// #        name -> Varchar,
    /// #    }
    /// # );
    /// # #[derive(Queryable, Debug, PartialEq)]
    /// # struct User {
    /// #     id: i32,
    /// #     name: String,
    /// # }
    /// use diesel::result::Error;
    ///
    /// fn main() {
    ///     let conn = establish_connection();
    ///     let _ = conn.transaction::<_, Error, _>(|| {
    ///         let new_user = NewUser { name: "Ruby".into() };
    ///         diesel::insert_into(users::table).values(&new_user).execute(&conn)?;
    ///         assert_eq!(users::table.load::<User>(&conn), Ok(vec![
    ///             User { id: 1, name: "Sean".into() },
    ///             User { id: 2, name: "Tess".into() },
    ///             User { id: 3, name: "Ruby".into() },
    ///         ]));
    ///
    ///         Ok(())
    ///     });
    ///
    ///     let _ = conn.transaction::<(), Error, _>(|| {
    ///         let new_user = NewUser { name: "Pascal".into() };
    ///         diesel::insert_into(users::table).values(&new_user).execute(&conn)?;
    ///
    ///         assert_eq!(users::table.load::<User>(&conn), Ok(vec![
    ///             User { id: 1, name: "Sean".into() },
    ///             User { id: 2, name: "Tess".into() },
    ///             User { id: 3, name: "Ruby".into() },
    ///             User { id: 4, name: "Pascal".into() },
    ///         ]));
    ///
    ///         Err(Error::RollbackTransaction) // Oh noeees, something bad happened :(
    ///     });
    ///
    ///     assert_eq!(users::table.load::<User>(&conn), Ok(vec![
    ///         User { id: 1, name: "Sean".into() },
    ///         User { id: 2, name: "Tess".into() },
    ///         User { id: 3, name: "Ruby".into() },
    ///     ]));
    /// }
    /// ```
    fn transaction<T, E, F>(&self, f: F) -> Result<T, E>
    where
        F: FnOnce() -> Result<T, E>,
        E: From<Error>,
    {
        let transaction_manager = self.transaction_manager();
        try!(transaction_manager.begin_transaction(self));
        match f() {
            Ok(value) => {
                try!(transaction_manager.commit_transaction(self));
                Ok(value)
            }
            Err(e) => {
                try!(transaction_manager.rollback_transaction(self));
                Err(e)
            }
        }
    }

    /// Creates a transaction that will never be committed. This is useful for
    /// tests. Panics if called while inside of a transaction.
    fn begin_test_transaction(&self) -> QueryResult<()> {
        let transaction_manager = self.transaction_manager();
        assert_eq!(transaction_manager.get_transaction_depth(), 0);
        transaction_manager.begin_transaction(self)
    }

    /// Executes the given function inside a transaction, but does not commit
    /// it. Panics if the given function returns an `Err`.
    fn test_transaction<T, E, F>(&self, f: F) -> T
    where
        F: FnOnce() -> Result<T, E>,
    {
        let mut user_result = None;
        let _ = self.transaction::<(), _, _>(|| {
            user_result = f().ok();
            Err(Error::RollbackTransaction)
        });
        user_result.expect("Transaction did not succeed")
    }

    #[doc(hidden)]
    fn execute(&self, query: &str) -> QueryResult<usize>;

    #[doc(hidden)]
    fn query_by_index<T, U>(&self, source: T) -> QueryResult<Vec<U>>
    where
        T: AsQuery,
        T::Query: QueryFragment<Self::Backend> + QueryId,
        Self::Backend: HasSqlType<T::SqlType>,
        U: Queryable<T::SqlType, Self::Backend>;

    #[doc(hidden)]
    fn execute_returning_count<T>(&self, source: &T) -> QueryResult<usize>
    where
        T: QueryFragment<Self::Backend> + QueryId;

    #[doc(hidden)]
    fn silence_notices<F: FnOnce() -> T, T>(&self, f: F) -> T;
    #[doc(hidden)]
    fn transaction_manager(&self) -> &Self::TransactionManager;
}
