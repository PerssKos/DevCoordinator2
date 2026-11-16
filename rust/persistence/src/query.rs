//! Small, native PostgreSQL query helpers for the synchronous domain layer.
//!
//! SQL is passed verbatim to PostgreSQL. In particular, this module does not
//! translate SQLite syntax, emulate SQLite functions, or select a fallback DB.

use std::cell::{Cell, RefCell};
use std::error::Error as StdError;
use std::marker::PhantomData;
use std::ops::Deref;

use bytes::BytesMut;
use postgres::types::{IsNull, ToSql, Type as PgType, to_sql_checked};

use crate::{Error, PooledClient, Result};

pub mod types {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Type {
        Null,
        Integer,
        Real,
        Text,
        Blob,
        Boolean,
        Json,
    }

    /// A bound value. Unsigned values are checked when bound; they never wrap.
    #[derive(Clone, Debug, PartialEq)]
    pub enum Value {
        Null,
        Integer(i64),
        Unsigned(u64),
        Real(f64),
        Text(String),
        Blob(Vec<u8>),
        Bool(bool),
        Json(serde_json::Value),
        /// Exact PostgreSQL JSON text, including arbitrary-precision numbers.
        JsonText(String),
    }
}
pub use types::Value;

pub trait ToValue {
    fn to_value(&self) -> Value;
}
impl ToValue for Value {
    fn to_value(&self) -> Value {
        self.clone()
    }
}
impl<T: ToValue + ?Sized> ToValue for &T {
    fn to_value(&self) -> Value {
        (*self).to_value()
    }
}
impl<T: ToValue> ToValue for Option<T> {
    fn to_value(&self) -> Value {
        self.as_ref().map_or(Value::Null, ToValue::to_value)
    }
}
impl ToValue for str {
    fn to_value(&self) -> Value {
        Value::Text(self.to_owned())
    }
}
impl ToValue for String {
    fn to_value(&self) -> Value {
        Value::Text(self.clone())
    }
}
impl ToValue for std::borrow::Cow<'_, str> {
    fn to_value(&self) -> Value {
        Value::Text(self.as_ref().to_owned())
    }
}
impl ToValue for bool {
    fn to_value(&self) -> Value {
        Value::Bool(*self)
    }
}
impl ToValue for [u8] {
    fn to_value(&self) -> Value {
        Value::Blob(self.to_vec())
    }
}
impl ToValue for Vec<u8> {
    fn to_value(&self) -> Value {
        Value::Blob(self.clone())
    }
}
impl ToValue for serde_json::Value {
    fn to_value(&self) -> Value {
        Value::Json(self.clone())
    }
}
macro_rules! signed_values { ($($t:ty),*) => { $(impl ToValue for $t { fn to_value(&self) -> Value { Value::Integer(i64::from(*self)) } })* }; }
signed_values!(i8, i16, i32, i64, u8, u16, u32);
impl ToValue for u64 {
    fn to_value(&self) -> Value {
        Value::Unsigned(*self)
    }
}
impl ToValue for usize {
    fn to_value(&self) -> Value {
        Value::Unsigned(*self as u64)
    }
}
impl ToValue for isize {
    fn to_value(&self) -> Value {
        Value::Integer(*self as i64)
    }
}
impl ToValue for f32 {
    fn to_value(&self) -> Value {
        Value::Real(f64::from(*self))
    }
}
impl ToValue for f64 {
    fn to_value(&self) -> Value {
        Value::Real(*self)
    }
}

macro_rules! from_values { ($($t:ty),*) => { $(impl From<$t> for Value { fn from(v:$t) -> Self { v.to_value() } })* }; }
from_values!(
    i8,
    i16,
    i32,
    i64,
    u8,
    u16,
    u32,
    u64,
    usize,
    isize,
    bool,
    f32,
    f64,
    String,
    Vec<u8>,
    serde_json::Value
);
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        v.to_value()
    }
}

#[macro_export]
macro_rules! params {
    () => { Vec::<$crate::Value>::new() };
    ($($value:expr),+ $(,)?) => { vec![$($crate::ToValue::to_value(&$value)),+] };
}

pub trait Params {
    fn into_values(self) -> Vec<Value>;
}
impl Params for [Value; 0] {
    fn into_values(self) -> Vec<Value> {
        Vec::new()
    }
}
impl Params for () {
    fn into_values(self) -> Vec<Value> {
        Vec::new()
    }
}
impl Params for Vec<Value> {
    fn into_values(self) -> Vec<Value> {
        self
    }
}
impl Params for &[Value] {
    fn into_values(self) -> Vec<Value> {
        self.to_vec()
    }
}
macro_rules! array_params { ($($n:expr),*) => { $(impl<T: ToValue> Params for [T; $n] { fn into_values(self) -> Vec<Value> { self.iter().map(ToValue::to_value).collect() } })* }; }
array_params!(
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32
);
macro_rules! tuple_params { ($($t:ident:$i:tt),+) => {impl<$($t:ToValue),+> Params for ($($t,)+) {fn into_values(self)->Vec<Value>{vec![$(self.$i.to_value()),+]}}}; }
tuple_params!(A:0);
tuple_params!(A:0,B:1);
tuple_params!(A:0,B:1,C:2);
tuple_params!(A:0,B:1,C:2,D:3);
tuple_params!(A:0,B:1,C:2,D:3,E:4);
tuple_params!(A:0,B:1,C:2,D:3,E:4,F:5);
tuple_params!(A:0,B:1,C:2,D:3,E:4,F:5,G:6);
tuple_params!(A:0,B:1,C:2,D:3,E:4,F:5,G:6,H:7);
pub fn params_from_iter<I: IntoIterator>(values: I) -> Vec<Value>
where
    I::Item: ToValue,
{
    values.into_iter().map(|v| v.to_value()).collect()
}

fn numeric_integer(value: u64, negative: bool, out: &mut BytesMut) -> IsNull {
    let digits = value.to_string();
    let padding = (4 - digits.len() % 4) % 4;
    let padded = format!("{}{digits}", "0".repeat(padding));
    let groups = padded
        .as_bytes()
        .chunks(4)
        .map(|bytes| std::str::from_utf8(bytes).unwrap().parse::<i16>().unwrap())
        .collect::<Vec<_>>();
    out.extend_from_slice(&(groups.len() as i16).to_be_bytes());
    out.extend_from_slice(&((groups.len() - 1) as i16).to_be_bytes());
    out.extend_from_slice(&(if negative { 0x4000_i16 } else { 0_i16 }).to_be_bytes());
    out.extend_from_slice(&0_i16.to_be_bytes());
    for group in groups {
        out.extend_from_slice(&group.to_be_bytes());
    }
    IsNull::No
}

impl ToSql for Value {
    fn to_sql(
        &self,
        ty: &PgType,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn StdError + Send + Sync>> {
        let integer = |n: i64,
                       out: &mut BytesMut|
         -> std::result::Result<IsNull, Box<dyn StdError + Send + Sync>> {
            match *ty {
                PgType::INT8 => n.to_sql(ty, out),
                PgType::INT4 => i32::try_from(n)?.to_sql(ty, out),
                PgType::INT2 => i16::try_from(n)?.to_sql(ty, out),
                PgType::FLOAT8 => (n as f64).to_sql(ty, out),
                PgType::BOOL => match n {
                    0 => false.to_sql(ty, out),
                    1 => true.to_sql(ty, out),
                    _ => Err("boolean integer must be zero or one".into()),
                },
                PgType::NUMERIC => Ok(numeric_integer(n.unsigned_abs(), n < 0, out)),
                _ => Err(format!("integer cannot be bound as {ty}").into()),
            }
        };
        match self {
            Self::Null => Ok(IsNull::Yes),
            Self::Integer(n) => integer(*n, out),
            Self::Unsigned(n) if *ty == PgType::NUMERIC => Ok(numeric_integer(*n, false, out)),
            Self::Unsigned(n) => integer(i64::try_from(*n)?, out),
            Self::Bool(b) if *ty == PgType::BOOL => b.to_sql(ty, out),
            Self::Bool(b) => integer(i64::from(*b), out),
            Self::Real(n) if *ty == PgType::FLOAT4 => (*n as f32).to_sql(ty, out),
            Self::Real(n) => n.to_sql(ty, out),
            Self::Text(s) | Self::JsonText(s) if *ty == PgType::JSONB || *ty == PgType::JSON => {
                if *ty == PgType::JSONB {
                    out.extend_from_slice(&[1]);
                }
                out.extend_from_slice(s.as_bytes());
                Ok(IsNull::No)
            }
            Self::JsonText(s) => s.to_sql(ty, out),
            Self::Text(s) => s.to_sql(ty, out),
            Self::Blob(b) => b.to_sql(ty, out),
            Self::Json(j) if *ty == PgType::JSONB || *ty == PgType::JSON => j.to_sql(ty, out),
            Self::Json(j) => serde_json::to_string(j)?.to_sql(ty, out),
        }
    }
    fn accepts(_ty: &PgType) -> bool {
        true
    }
    to_sql_checked!();
}

/// A connection belongs to one pool lane for its whole checkout lifetime.
pub struct Connection {
    client: RefCell<PooledClient>,
    in_transaction: Cell<bool>,
}

impl Connection {
    pub(crate) fn new(client: PooledClient) -> Self {
        Self {
            client: RefCell::new(client),
            in_transaction: Cell::new(false),
        }
    }

    pub fn execute_batch(&self, sql: &str) -> Result<()> {
        self.client.borrow_mut().batch_execute(sql)?;
        Ok(())
    }
    pub fn execute<P: Params>(&self, sql: &str, params: P) -> Result<usize> {
        let values = params.into_values();
        let refs: Vec<&(dyn ToSql + Sync)> = values.iter().map(|v| v as _).collect();
        usize::try_from(self.client.borrow_mut().execute(sql, &refs)?)
            .map_err(|_| Error::InvalidQuery)
    }
    pub fn prepare(&self, sql: &str) -> Result<Statement<'_>> {
        let statement = self.client.borrow_mut().prepare(sql)?;
        Ok(Statement {
            connection: self,
            statement,
        })
    }
    pub fn prepare_cached(&self, sql: &str) -> Result<Statement<'_>> {
        self.prepare(sql)
    }
    pub fn query_row<T, P: Params, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        self.prepare(sql)?.query_row(params, map)
    }
    pub fn transaction(&mut self) -> Result<Transaction<'_>> {
        self.unchecked_transaction()
    }
    pub fn unchecked_transaction(&self) -> Result<Transaction<'_>> {
        if self.in_transaction.get() {
            return Err(Error::InvalidQuery);
        }
        self.execute_batch("BEGIN")?;
        self.in_transaction.set(true);
        Ok(Transaction {
            connection: self,
            finished: false,
        })
    }
    /// Native client access is reserved for schema/COPY/operational boundaries.
    pub fn with_client<T>(
        &self,
        work: impl FnOnce(&mut postgres::Client) -> Result<T>,
    ) -> Result<T> {
        work(&mut self.client.borrow_mut())
    }
}

pub struct Transaction<'a> {
    connection: &'a Connection,
    finished: bool,
}
impl Deref for Transaction<'_> {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        self.connection
    }
}
impl Transaction<'_> {
    pub fn commit(mut self) -> Result<()> {
        self.connection.execute_batch("COMMIT")?;
        self.connection.in_transaction.set(false);
        self.finished = true;
        Ok(())
    }
    pub fn rollback(mut self) -> Result<()> {
        self.connection.execute_batch("ROLLBACK")?;
        self.connection.in_transaction.set(false);
        self.finished = true;
        Ok(())
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.connection.execute_batch("ROLLBACK");
            self.connection.in_transaction.set(false);
        }
    }
}

pub struct Statement<'a> {
    connection: &'a Connection,
    statement: postgres::Statement,
}
impl Statement<'_> {
    pub fn column_count(&self) -> usize {
        self.statement.columns().len()
    }
    pub fn column_names(&self) -> Vec<&str> {
        self.statement.columns().iter().map(|c| c.name()).collect()
    }
    pub fn execute<P: Params>(&mut self, params: P) -> Result<usize> {
        let values = params.into_values();
        let refs: Vec<&(dyn ToSql + Sync)> = values.iter().map(|v| v as _).collect();
        usize::try_from(
            self.connection
                .client
                .borrow_mut()
                .execute(&self.statement, &refs)?,
        )
        .map_err(|_| Error::InvalidQuery)
    }
    pub fn query<P: Params>(&mut self, params: P) -> Result<Rows> {
        let values = params.into_values();
        let refs: Vec<&(dyn ToSql + Sync)> = values.iter().map(|v| v as _).collect();
        let rows = self
            .connection
            .client
            .borrow_mut()
            .query(&self.statement, &refs)?;
        Ok(Rows {
            rows: rows.into_iter().map(Row::new).collect(),
            position: 0,
        })
    }
    pub fn query_map<T, P: Params, F>(
        &mut self,
        params: P,
        mut map: F,
    ) -> Result<std::vec::IntoIter<Result<T>>>
    where
        F: FnMut(&Row<'_>) -> Result<T>,
    {
        let rows = self.query(params)?;
        Ok(rows
            .rows
            .iter()
            .map(&mut map)
            .collect::<Vec<_>>()
            .into_iter())
    }
    pub fn query_row<T, P: Params, F>(&mut self, params: P, map: F) -> Result<T>
    where
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        let mut rows = self.query(params)?;
        map(rows.next()?.ok_or(Error::QueryReturnedNoRows)?)
    }
}

pub struct Rows {
    rows: Vec<Row<'static>>,
    position: usize,
}
impl Rows {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<&Row<'static>>> {
        let row = self.rows.get(self.position);
        if row.is_some() {
            self.position += 1;
        }
        Ok(row)
    }
}

pub struct Row<'a> {
    inner: postgres::Row,
    _scope: PhantomData<&'a ()>,
}
struct RawJson(String);
impl<'a> postgres::types::FromSql<'a> for RawJson {
    fn from_sql(
        ty: &PgType,
        raw: &'a [u8],
    ) -> std::result::Result<Self, Box<dyn StdError + Send + Sync>> {
        let bytes = if *ty == PgType::JSONB {
            if raw.first() != Some(&1) {
                return Err("unsupported PostgreSQL JSONB version".into());
            }
            &raw[1..]
        } else {
            raw
        };
        Ok(Self(std::str::from_utf8(bytes)?.to_owned()))
    }
    fn accepts(ty: &PgType) -> bool {
        *ty == PgType::JSON || *ty == PgType::JSONB
    }
}
impl Row<'_> {
    fn new(inner: postgres::Row) -> Self {
        Self {
            inner,
            _scope: PhantomData,
        }
    }
    pub(crate) fn from_postgres(inner: postgres::Row) -> Self {
        Self::new(inner)
    }
    pub fn get<I: RowIndex, T: FromValue>(&self, index: I) -> Result<T> {
        let index = index.index(&self.inner)?;
        T::from_value(self.get_value(index)?, index)
    }
    pub fn get_value(&self, index: usize) -> Result<Value> {
        let column = self
            .inner
            .columns()
            .get(index)
            .ok_or(Error::InvalidColumnIndex(index))?;
        macro_rules! decode {
            ($t:ty,$map:expr) => {
                self.inner
                    .try_get::<_, Option<$t>>(index)?
                    .map($map)
                    .unwrap_or(Value::Null)
            };
        }
        Ok(match *column.type_() {
            PgType::INT8 => decode!(i64, Value::Integer),
            PgType::INT4 => decode!(i32, |v| Value::Integer(i64::from(v))),
            PgType::INT2 => decode!(i16, |v| Value::Integer(i64::from(v))),
            PgType::FLOAT8 => decode!(f64, Value::Real),
            PgType::FLOAT4 => decode!(f32, |v| Value::Real(f64::from(v))),
            PgType::BOOL => decode!(bool, Value::Bool),
            PgType::BYTEA => decode!(Vec<u8>, Value::Blob),
            PgType::JSON | PgType::JSONB => decode!(RawJson, |v| Value::JsonText(v.0)),
            PgType::TEXT | PgType::VARCHAR | PgType::BPCHAR | PgType::NAME | PgType::UNKNOWN => {
                decode!(String, Value::Text)
            }
            _ => {
                return Err(Error::FromSqlConversionFailure(
                    index,
                    types::Type::Text,
                    format!("unsupported PostgreSQL result type {}", column.type_()).into(),
                ));
            }
        })
    }
    pub fn column_count(&self) -> usize {
        self.inner.len()
    }
}
impl<'a> AsRef<Row<'a>> for Row<'a> {
    fn as_ref(&self) -> &Self {
        self
    }
}
pub trait RowIndex {
    fn index(&self, row: &postgres::Row) -> Result<usize>;
}
impl RowIndex for usize {
    fn index(&self, row: &postgres::Row) -> Result<usize> {
        if *self < row.len() {
            Ok(*self)
        } else {
            Err(Error::InvalidColumnIndex(*self))
        }
    }
}
impl RowIndex for &str {
    fn index(&self, row: &postgres::Row) -> Result<usize> {
        row.columns()
            .iter()
            .position(|c| c.name() == *self)
            .ok_or(Error::InvalidColumnName((*self).to_owned()))
    }
}

pub trait FromValue: Sized {
    fn from_value(value: Value, index: usize) -> Result<Self>;
}
impl FromValue for Value {
    fn from_value(value: Value, _: usize) -> Result<Self> {
        Ok(value)
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: Value, index: usize) -> Result<Self> {
        if value == Value::Null {
            Ok(None)
        } else {
            T::from_value(value, index).map(Some)
        }
    }
}
fn wrong_type(index: usize, value: &Value) -> Error {
    Error::FromSqlConversionFailure(
        index,
        match value {
            Value::Null => types::Type::Null,
            Value::Integer(_) | Value::Unsigned(_) => types::Type::Integer,
            Value::Real(_) => types::Type::Real,
            Value::Text(_) => types::Type::Text,
            Value::Blob(_) => types::Type::Blob,
            Value::Bool(_) => types::Type::Boolean,
            Value::Json(_) | Value::JsonText(_) => types::Type::Json,
        },
        "result type does not match requested Rust type".into(),
    )
}
macro_rules! integer_gets { ($($t:ty),*) => { $(impl FromValue for $t { fn from_value(value:Value,index:usize)->Result<Self> { match value { Value::Integer(n)=>Self::try_from(n).map_err(|_|Error::IntegralValueOutOfRange(index,n)),Value::Bool(v)=>Ok(Self::from(v)),_=>Err(wrong_type(index,&value)) } } })* }; }
integer_gets!(i16, i32, i64, u8, u16, u32, u64, usize, isize);
impl FromValue for String {
    fn from_value(value: Value, index: usize) -> Result<Self> {
        match value {
            Value::Text(s) => Ok(s),
            Value::JsonText(s) => Ok(s),
            Value::Json(j) => Ok(serde_json::to_string(&j).map_err(|e| {
                Error::FromSqlConversionFailure(index, types::Type::Json, Box::new(e))
            })?),
            _ => Err(wrong_type(index, &value)),
        }
    }
}
impl FromValue for bool {
    fn from_value(value: Value, index: usize) -> Result<Self> {
        match value {
            Value::Bool(v) => Ok(v),
            Value::Integer(0) => Ok(false),
            Value::Integer(1) => Ok(true),
            _ => Err(wrong_type(index, &value)),
        }
    }
}
impl FromValue for f64 {
    fn from_value(value: Value, index: usize) -> Result<Self> {
        match value {
            Value::Real(v) => Ok(v),
            Value::Integer(v) => Ok(v as f64),
            _ => Err(wrong_type(index, &value)),
        }
    }
}
impl FromValue for Vec<u8> {
    fn from_value(value: Value, index: usize) -> Result<Self> {
        match value {
            Value::Blob(v) => Ok(v),
            _ => Err(wrong_type(index, &value)),
        }
    }
}
impl FromValue for serde_json::Value {
    fn from_value(value: Value, index: usize) -> Result<Self> {
        match value {
            Value::Json(v) => Ok(v),
            Value::JsonText(v) => serde_json::from_str(&v).map_err(|e| {
                Error::FromSqlConversionFailure(index, types::Type::Json, Box::new(e))
            }),
            Value::Text(v) => serde_json::from_str(&v).map_err(|e| {
                Error::FromSqlConversionFailure(index, types::Type::Json, Box::new(e))
            }),
            _ => Err(wrong_type(index, &value)),
        }
    }
}

pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}
impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
