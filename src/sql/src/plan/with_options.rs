// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Provides tooling to handle `WITH` options.

use std::time::Duration;

use mz_repr::adt::interval::Interval;
use mz_repr::bytes::ByteSize;
use mz_repr::{strconv, GlobalId};
use mz_sql_parser::ast::{Ident, KafkaBroker, RefreshOptionValue, ReplicaDefinition};
use mz_storage_types::connections::StringOrSecret;
use serde::{Deserialize, Serialize};

use crate::ast::{AstInfo, UnresolvedItemName, Value, WithOptionValue};
use crate::names::{ResolvedDataType, ResolvedItemName};
use crate::plan::{literal, Aug, PlanError};

pub trait TryFromValue<T>: Sized {
    fn try_from_value(v: T) -> Result<Self, PlanError>;
    fn name() -> String;
}

pub trait ImpliedValue: Sized {
    fn implied_value() -> Result<Self, PlanError>;
}

#[derive(Copy, Clone, Debug)]
pub struct Secret(GlobalId);

impl From<Secret> for GlobalId {
    fn from(secret: Secret) -> Self {
        secret.0
    }
}

impl TryFromValue<WithOptionValue<Aug>> for Secret {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        match StringOrSecret::try_from_value(v)? {
            StringOrSecret::Secret(id) => Ok(Secret(id)),
            _ => sql_bail!("must provide a secret value"),
        }
    }
    fn name() -> String {
        "secret".to_string()
    }
}

impl ImpliedValue for Secret {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a secret value")
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Object(GlobalId);

impl From<Object> for GlobalId {
    fn from(obj: Object) -> Self {
        obj.0
    }
}

impl From<&Object> for GlobalId {
    fn from(obj: &Object) -> Self {
        obj.0
    }
}

impl TryFromValue<WithOptionValue<Aug>> for Object {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        Ok(match v {
            WithOptionValue::Item(ResolvedItemName::Item { id, .. }) => Object(id),
            _ => sql_bail!("must provide an object"),
        })
    }
    fn name() -> String {
        "object reference".to_string()
    }
}

impl ImpliedValue for Object {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an object")
    }
}

impl TryFromValue<WithOptionValue<Aug>> for Ident {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        Ok(match v {
            WithOptionValue::Ident(name) => name,
            _ => sql_bail!("must provide an identifier"),
        })
    }
    fn name() -> String {
        "identifier".to_string()
    }
}

impl ImpliedValue for Ident {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an identifier")
    }
}

impl TryFromValue<WithOptionValue<Aug>> for UnresolvedItemName {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        Ok(match v {
            WithOptionValue::UnresolvedItemName(name) => name,
            _ => sql_bail!("must provide an object name"),
        })
    }
    fn name() -> String {
        "object name".to_string()
    }
}

impl ImpliedValue for UnresolvedItemName {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an object name")
    }
}

impl TryFromValue<WithOptionValue<Aug>> for ResolvedDataType {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        Ok(match v {
            WithOptionValue::DataType(ty) => ty,
            _ => sql_bail!("must provide a data type"),
        })
    }
    fn name() -> String {
        "data type".to_string()
    }
}

impl ImpliedValue for ResolvedDataType {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a data type")
    }
}

impl TryFromValue<WithOptionValue<Aug>> for StringOrSecret {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        Ok(match v {
            WithOptionValue::Secret(ResolvedItemName::Item { id, .. }) => {
                StringOrSecret::Secret(id)
            }
            v => StringOrSecret::String(String::try_from_value(v)?),
        })
    }
    fn name() -> String {
        "string or secret".to_string()
    }
}

impl ImpliedValue for StringOrSecret {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a string or secret value")
    }
}

impl TryFromValue<Value> for Duration {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        let interval = Interval::try_from_value(v)?;
        Ok(interval.duration()?)
    }
    fn name() -> String {
        "interval".to_string()
    }
}

impl ImpliedValue for Duration {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an interval value")
    }
}

impl TryFromValue<Value> for ByteSize {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Number(value) | Value::String(value) => Ok(value
                .parse::<ByteSize>()
                .map_err(|e| sql_err!("invalid bytes value: {e}"))?),
            _ => sql_bail!("cannot use value as bytes"),
        }
    }
    fn name() -> String {
        "bytes".to_string()
    }
}

impl ImpliedValue for ByteSize {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a value for bytes")
    }
}

impl TryFromValue<Value> for Interval {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Interval(value) => literal::plan_interval(&value),
            Value::Number(value) | Value::String(value) => Ok(strconv::parse_interval(&value)?),
            _ => sql_bail!("cannot use value as interval"),
        }
    }
    fn name() -> String {
        "interval".to_string()
    }
}

impl ImpliedValue for Interval {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an interval value")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Hash, Deserialize)]
pub struct OptionalDuration(pub Option<Duration>);

impl From<Duration> for OptionalDuration {
    fn from(i: Duration) -> OptionalDuration {
        // An interval of 0 disables the setting.
        let inner = if i == Duration::ZERO { None } else { Some(i) };
        OptionalDuration(inner)
    }
}

impl TryFromValue<Value> for OptionalDuration {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        Ok(match v {
            Value::Null => OptionalDuration(None),
            v => Duration::try_from_value(v)?.into(),
        })
    }
    fn name() -> String {
        "optional interval".to_string()
    }
}

impl ImpliedValue for OptionalDuration {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an interval value")
    }
}

impl TryFromValue<Value> for String {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::String(v) => Ok(v),
            _ => sql_bail!("cannot use value as string"),
        }
    }
    fn name() -> String {
        "text".to_string()
    }
}

impl ImpliedValue for String {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a string value")
    }
}

impl TryFromValue<Value> for bool {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Boolean(v) => Ok(v),
            _ => sql_bail!("cannot use value as boolean"),
        }
    }
    fn name() -> String {
        "bool".to_string()
    }
}

impl ImpliedValue for bool {
    fn implied_value() -> Result<Self, PlanError> {
        Ok(true)
    }
}

impl TryFromValue<Value> for f64 {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Number(v) => v
                .parse::<f64>()
                .map_err(|e| sql_err!("invalid numeric value: {e}")),
            _ => sql_bail!("cannot use value as number"),
        }
    }

    fn name() -> String {
        "float8".to_string()
    }
}

impl ImpliedValue for f64 {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a float value")
    }
}

impl TryFromValue<Value> for i32 {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Number(v) => v
                .parse::<i32>()
                .map_err(|e| sql_err!("invalid numeric value: {e}")),
            _ => sql_bail!("cannot use value as number"),
        }
    }
    fn name() -> String {
        "int".to_string()
    }
}

impl ImpliedValue for i32 {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an integer value")
    }
}

impl TryFromValue<Value> for i64 {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Number(v) => v
                .parse::<i64>()
                .map_err(|e| sql_err!("invalid numeric value: {e}")),
            _ => sql_bail!("cannot use value as number"),
        }
    }
    fn name() -> String {
        "int8".to_string()
    }
}

impl ImpliedValue for i64 {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an integer value")
    }
}

impl TryFromValue<Value> for u16 {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Number(v) => v
                .parse::<u16>()
                .map_err(|e| sql_err!("invalid numeric value: {e}")),
            _ => sql_bail!("cannot use value as number"),
        }
    }
    fn name() -> String {
        "uint2".to_string()
    }
}

impl ImpliedValue for u16 {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an integer value")
    }
}

impl TryFromValue<Value> for u32 {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Number(v) => v
                .parse::<u32>()
                .map_err(|e| sql_err!("invalid numeric value: {e}")),
            _ => sql_bail!("cannot use value as number"),
        }
    }
    fn name() -> String {
        "uint4".to_string()
    }
}

impl ImpliedValue for u32 {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an integer value")
    }
}

impl TryFromValue<Value> for u64 {
    fn try_from_value(v: Value) -> Result<Self, PlanError> {
        match v {
            Value::Number(v) => v
                .parse::<u64>()
                .map_err(|e| sql_err!("invalid unsigned numeric value: {e}")),
            _ => sql_bail!("cannot use value as number"),
        }
    }
    fn name() -> String {
        "uint8".to_string()
    }
}

impl ImpliedValue for u64 {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an unsigned integer value")
    }
}

impl<V: TryFromValue<WithOptionValue<Aug>>> TryFromValue<WithOptionValue<Aug>> for Vec<V> {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        match v {
            WithOptionValue::Sequence(a) => {
                let mut out = Vec::with_capacity(a.len());
                for i in a {
                    out.push(
                        V::try_from_value(i)
                            .map_err(|_| anyhow::anyhow!("cannot use value in array"))?,
                    )
                }
                Ok(out)
            }
            _ => sql_bail!("cannot use value as array"),
        }
    }
    fn name() -> String {
        format!("array of {}", V::name())
    }
}

impl<V: ImpliedValue> ImpliedValue for Vec<V> {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide an array value")
    }
}

impl<T: AstInfo, V: TryFromValue<WithOptionValue<T>>> TryFromValue<WithOptionValue<T>>
    for Option<V>
{
    fn try_from_value(v: WithOptionValue<T>) -> Result<Self, PlanError> {
        Ok(Some(V::try_from_value(v)?))
    }

    fn name() -> String {
        format!("optional {}", V::name())
    }
}

impl<V: ImpliedValue> ImpliedValue for Option<V> {
    fn implied_value() -> Result<Self, PlanError> {
        Ok(Some(V::implied_value()?))
    }
}

impl<V: TryFromValue<Value>, T: AstInfo + std::fmt::Debug> TryFromValue<WithOptionValue<T>> for V {
    fn try_from_value(v: WithOptionValue<T>) -> Result<Self, PlanError> {
        match v {
            WithOptionValue::Value(v) => V::try_from_value(v),
            WithOptionValue::Ident(i) => V::try_from_value(Value::String(i.into_string())),
            WithOptionValue::RetainHistoryFor(v) => V::try_from_value(v),
            WithOptionValue::Sequence(_)
            | WithOptionValue::Item(_)
            | WithOptionValue::UnresolvedItemName(_)
            | WithOptionValue::Secret(_)
            | WithOptionValue::DataType(_)
            | WithOptionValue::ClusterReplicas(_)
            | WithOptionValue::ConnectionKafkaBroker(_)
            | WithOptionValue::Refresh(_) => sql_bail!(
                "incompatible value types: cannot convert {} to {}",
                match v {
                    // The first few are unreachable because they are handled at the top of the outer match.
                    WithOptionValue::Value(_) => unreachable!(),
                    WithOptionValue::Ident(_) => unreachable!(),
                    WithOptionValue::RetainHistoryFor(_) => unreachable!(),
                    WithOptionValue::Sequence(_) => "sequences",
                    WithOptionValue::Item(_) => "object references",
                    WithOptionValue::UnresolvedItemName(_) => "object names",
                    WithOptionValue::Secret(_) => "secrets",
                    WithOptionValue::DataType(_) => "data types",
                    WithOptionValue::ClusterReplicas(_) => "cluster replicas",
                    WithOptionValue::ConnectionKafkaBroker(_) => "connection kafka brokers",
                    WithOptionValue::Refresh(_) => "refresh option values",
                },
                V::name()
            ),
        }
    }
    fn name() -> String {
        V::name()
    }
}

impl<T, V: TryFromValue<T> + ImpliedValue> TryFromValue<Option<T>> for V {
    fn try_from_value(v: Option<T>) -> Result<Self, PlanError> {
        match v {
            Some(v) => V::try_from_value(v),
            None => V::implied_value(),
        }
    }
    fn name() -> String {
        V::name()
    }
}

impl TryFromValue<WithOptionValue<Aug>> for Vec<ReplicaDefinition<Aug>> {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        match v {
            WithOptionValue::ClusterReplicas(replicas) => Ok(replicas),
            _ => sql_bail!("cannot use value as cluster replicas"),
        }
    }
    fn name() -> String {
        "cluster replicas".to_string()
    }
}

impl ImpliedValue for Vec<ReplicaDefinition<Aug>> {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a set of cluster replicas")
    }
}

impl TryFromValue<WithOptionValue<Aug>> for Vec<KafkaBroker<Aug>> {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        let mut out = vec![];
        match v {
            WithOptionValue::ConnectionKafkaBroker(broker) => {
                out.push(broker);
            }
            WithOptionValue::Sequence(values) => {
                for value in values {
                    out.extend(Self::try_from_value(value)?);
                }
            }
            _ => sql_bail!("cannot use value as a kafka broker"),
        }
        Ok(out)
    }
    fn name() -> String {
        "kafka broker".to_string()
    }
}

impl ImpliedValue for Vec<KafkaBroker<Aug>> {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a kafka broker")
    }
}

impl TryFromValue<WithOptionValue<Aug>> for RefreshOptionValue<Aug> {
    fn try_from_value(v: WithOptionValue<Aug>) -> Result<Self, PlanError> {
        if let WithOptionValue::Refresh(r) = v {
            Ok(r)
        } else {
            sql_bail!("cannot use value `{}` for a refresh option", v)
        }
    }

    fn name() -> String {
        "refresh option value".to_string()
    }
}

impl ImpliedValue for RefreshOptionValue<Aug> {
    fn implied_value() -> Result<Self, PlanError> {
        sql_bail!("must provide a refresh option value")
    }
}
