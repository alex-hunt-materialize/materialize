// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Maintains a catalog of valid casts between [`mz_repr::ScalarType`]s, as well as
//! other cast-related functions.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::LazyLock;

use dynfmt::{Format, SimpleCurlyFormat};
use itertools::Itertools;
use mz_expr::func::{CastArrayToJsonb, CastListToJsonb};
use mz_expr::{VariadicFunc, func};
use mz_repr::{ColumnName, ColumnType, Datum, RelationType, ScalarBaseType, ScalarType};

use crate::catalog::TypeCategory;
use crate::plan::error::PlanError;
use crate::plan::hir::{
    AbstractColumnType, CoercibleScalarExpr, CoercibleScalarType, HirScalarExpr, UnaryFunc,
};
use crate::plan::query::{ExprContext, QueryContext};
use crate::plan::scope::Scope;

/// Like func::sql_impl_func, but for casts.
fn sql_impl_cast(expr: &'static str) -> CastTemplate {
    let invoke = crate::func::sql_impl(expr);
    CastTemplate::new(move |ecx, _ccx, from_type, _to_type| {
        // Oddly, this needs to be able to gracefully fail so we can detect unmet dependencies.
        let mut out = invoke(ecx.qcx, vec![from_type.clone()]).ok()?;
        Some(move |e| {
            out.splice_parameters(&[e], 0);
            out
        })
    })
}

fn sql_impl_cast_per_context(casts: &[(CastContext, &'static str)]) -> CastTemplate {
    let casts: BTreeMap<CastContext, _> = casts
        .iter()
        .map(|(ccx, expr)| (ccx.clone(), crate::func::sql_impl(expr)))
        .collect();
    CastTemplate::new(move |ecx, ccx, from_type, _to_type| {
        let invoke = &casts[&ccx];
        let r = invoke(ecx.qcx, vec![from_type.clone()]);
        let mut out = r.ok()?;
        Some(move |e| {
            out.splice_parameters(&[e], 0);
            out
        })
    })
}

/// A cast is a function that takes a `ScalarExpr` to another `ScalarExpr`.
type Cast = Box<dyn FnOnce(HirScalarExpr) -> HirScalarExpr>;

/// A cast template is a function that produces a `Cast` given a concrete input
/// and output type. A template can return `None` to indicate that it is
/// incapable of producing a cast for the specified types.
///
/// Cast templates are used to share code for similar casts, where the input or
/// output type is of one "category" of type. For example, a single cast
/// template handles converting from strings to any list type. Without cast
/// templates, we'd have to enumerate every possible list -> list conversion,
/// which is impractical.
struct CastTemplate(
    Box<dyn Fn(&ExprContext, CastContext, &ScalarType, &ScalarType) -> Option<Cast> + Send + Sync>,
);

impl CastTemplate {
    fn new<T, C>(t: T) -> CastTemplate
    where
        T: Fn(&ExprContext, CastContext, &ScalarType, &ScalarType) -> Option<C>
            + Send
            + Sync
            + 'static,
        C: FnOnce(HirScalarExpr) -> HirScalarExpr + 'static,
    {
        CastTemplate(Box::new(move |ecx, ccx, from_ty, to_ty| {
            Some(Box::new(t(ecx, ccx, from_ty, to_ty)?))
        }))
    }
}

impl From<UnaryFunc> for CastTemplate {
    fn from(u: UnaryFunc) -> CastTemplate {
        CastTemplate::new(move |_ecx, _ccx, _from, _to| {
            let u = u.clone();
            Some(move |expr: HirScalarExpr| expr.call_unary(u))
        })
    }
}

impl<const N: usize> From<[UnaryFunc; N]> for CastTemplate {
    fn from(funcs: [UnaryFunc; N]) -> CastTemplate {
        CastTemplate::new(move |_ecx, _ccx, _from, _to| {
            let funcs = funcs.clone();
            Some(move |mut expr: HirScalarExpr| {
                for func in funcs {
                    expr = expr.call_unary(func.clone());
                }
                expr
            })
        })
    }
}

/// STRING to REG*
///
/// A reg* type represents a specific type of object by oid.
///
/// Casting from a string to a reg*:
/// - Accepts a string that looks like an OID and converts the value to the
///   specified reg* type. This is available in all cases except explicitly
///   casting text values to regclass (e.g. `SELECT '2'::text::regclass`)
/// - Resolves non-OID-appearing strings to objects. If this string resolves to
///   more than one OID (e.g. functions), it errors.
///
/// The below code provides a template to accomplish this for various reg*
/// types. Arguments in order are:
/// - 0: type catalog name this is casting to
/// - 1: the category of this reg for the error message
/// - 2: Whether or not to permit passing through numeric values as OIDs
const STRING_REG_CAST_TEMPLATE: &str = "
(SELECT
CASE
    WHEN $1 IS NULL THEN NULL
-- Handle OID-like input, if available via {2}
    WHEN {2} AND pg_catalog.substring($1, 1, 1) BETWEEN '0' AND '9' THEN
        $1::pg_catalog.oid::pg_catalog.{0}
    ELSE (
    -- String case; look up that the item exists
        SELECT o.oid
        FROM mz_unsafe.mz_error_if_null(
            (
                -- We need to ensure a distinct here in the case of e.g. functions,
                -- where multiple items share a GlobalId.
                SELECT DISTINCT id AS name_id
                FROM mz_internal.mz_resolve_object_name('{0}', $1)
            ),
            -- TODO: Support the correct error code for does not exist (42883).
            '{1} \"' || $1 || '\" does not exist'
        ) AS i (name_id),
        -- Lateral lets us error separately from DNE case
        LATERAL (
            SELECT
                CASE
            -- Handle too many OIDs
                WHEN mz_catalog.list_length(mz_catalog.list_agg(oid)) > 1 THEN
                    mz_unsafe.mz_error_if_null(
                        NULL::pg_catalog.{0},
                        'more than one {1} named \"' || $1 || '\"'
                    )
            -- Resolve object name's OID if we know there is only one
                ELSE
                    CAST(mz_catalog.list_agg(oid)[1] AS pg_catalog.{0})
                END
            FROM mz_catalog.mz_objects
            WHERE id = name_id
            GROUP BY id
        ) AS o (oid)
    )
END)";

static STRING_TO_REGCLASS_EXPLICIT: LazyLock<String> = LazyLock::new(|| {
    SimpleCurlyFormat
        .format(STRING_REG_CAST_TEMPLATE, ["regclass", "relation", "false"])
        .unwrap()
        .to_string()
});

static STRING_TO_REGCLASS_COERCED: LazyLock<String> = LazyLock::new(|| {
    SimpleCurlyFormat
        .format(STRING_REG_CAST_TEMPLATE, ["regclass", "relation", "true"])
        .unwrap()
        .to_string()
});

static STRING_TO_REGPROC: LazyLock<String> = LazyLock::new(|| {
    SimpleCurlyFormat
        .format(STRING_REG_CAST_TEMPLATE, ["regproc", "function", "true"])
        .unwrap()
        .to_string()
});

static STRING_TO_REGTYPE: LazyLock<String> = LazyLock::new(|| {
    SimpleCurlyFormat
        .format(STRING_REG_CAST_TEMPLATE, ["regtype", "type", "true"])
        .unwrap()
        .to_string()
});

const REG_STRING_CAST_TEMPLATE: &str = "(
SELECT
    COALESCE(mz_internal.mz_global_id_to_name(o.id), CAST($1 AS pg_catalog.oid)::pg_catalog.text)
    AS text
FROM
  (
        SELECT
          (
            SELECT DISTINCT id
            FROM
              mz_catalog.mz_objects AS o
                JOIN
                  mz_internal.mz_object_oid_alias AS a
                  ON o.type = a.object_type
            WHERE
              oid = CAST($1 AS pg_catalog.oid)
                AND
              a.oid_alias = '{0}'
          )
      )
      AS o
)";

static REGCLASS_TO_STRING: LazyLock<String> = LazyLock::new(|| {
    SimpleCurlyFormat
        .format(REG_STRING_CAST_TEMPLATE, ["regclass"])
        .unwrap()
        .to_string()
});

static REGPROC_TO_STRING: LazyLock<String> = LazyLock::new(|| {
    SimpleCurlyFormat
        .format(REG_STRING_CAST_TEMPLATE, ["regproc"])
        .unwrap()
        .to_string()
});

static REGTYPE_TO_STRING: LazyLock<String> = LazyLock::new(|| {
    SimpleCurlyFormat
        .format(REG_STRING_CAST_TEMPLATE, ["regtype"])
        .unwrap()
        .to_string()
});

/// Describes the context of a cast.
///
/// n.b. this type derived `Ord, PartialOrd` and the ordering of these values
/// has semantics meaning; casts are only permitted when the caller's cast
/// context is geq the ccx we store, which is the minimum required context.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum CastContext {
    /// Implicit casts are "no-brainer" casts that apply automatically in
    /// expressions. They are typically lossless, such as `ScalarType::Int32` to
    /// `ScalarType::Int64`.
    Implicit,
    /// Assignment casts are "reasonable" casts that make sense to apply
    /// automatically in `INSERT` statements, but are surprising enough that
    /// they don't apply implicitly in expressions.
    Assignment,
    /// Explicit casts are casts that are possible but may be surprising, like
    /// casting `ScalarType::Json` to `ScalarType::Int32`, and therefore they do
    /// not happen unless explicitly requested by the user with a cast operator.
    Explicit,
    /// Coerced casts permit different behavior when a type is coerced from a
    /// string literal vs. a value of type `pg_catalog::text`.
    ///
    /// The only call site that should pass this value in to this module is
    /// string coercion.
    Coerced,
}

/// The implementation of a cast.
struct CastImpl {
    template: CastTemplate,
    context: CastContext,
}

macro_rules! casts(
    {
        $(
            $from_to:expr => $cast_context:ident: $cast_template:expr
        ),+
    } => {{
        let mut m = BTreeMap::new();
        $(
            m.insert($from_to, CastImpl {
                template: $cast_template.into(),
                context: CastContext::$cast_context,
            });
        )+
        m
    }};
);

static VALID_CASTS: LazyLock<BTreeMap<(ScalarBaseType, ScalarBaseType), CastImpl>> = LazyLock::new(
    || {
        use ScalarBaseType::*;
        use UnaryFunc::*;

        casts! {
            // BOOL
            (Bool, Int32) => Explicit: CastBoolToInt32(func::CastBoolToInt32),
            (Bool, Int64) => Explicit: CastBoolToInt64(func::CastBoolToInt64),
            (Bool, String) => Assignment: CastBoolToString(func::CastBoolToString),

            //INT16
            (Int16, Int32) => Implicit: CastInt16ToInt32(func::CastInt16ToInt32),
            (Int16, Int64) => Implicit: CastInt16ToInt64(func::CastInt16ToInt64),
            (Int16, UInt16) => Assignment: CastInt16ToUint16(func::CastInt16ToUint16),
            (Int16, UInt32) => Assignment: CastInt16ToUint32(func::CastInt16ToUint32),
            (Int16, UInt64) => Assignment: CastInt16ToUint64(func::CastInt16ToUint64),
            (Int16, Float32) => Implicit: CastInt16ToFloat32(func::CastInt16ToFloat32),
            (Int16, Float64) => Implicit: CastInt16ToFloat64(func::CastInt16ToFloat64),
            (Int16, Numeric) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastInt16ToNumeric(func::CastInt16ToNumeric(s))))
            }),
            (Int16, Oid) => Implicit: [
                CastInt16ToInt32(func::CastInt16ToInt32),
                CastInt32ToOid(func::CastInt32ToOid),
            ],
            (Int16, RegClass) => Implicit: [
                CastInt16ToInt32(func::CastInt16ToInt32),
                CastInt32ToOid(func::CastInt32ToOid),
                CastOidToRegClass(func::CastOidToRegClass),
            ],
            (Int16, RegProc) => Implicit: [
                CastInt16ToInt32(func::CastInt16ToInt32),
                CastInt32ToOid(func::CastInt32ToOid),
                CastOidToRegProc(func::CastOidToRegProc),
            ],
            (Int16, RegType) => Implicit: [
                CastInt16ToInt32(func::CastInt16ToInt32),
                CastInt32ToOid(func::CastInt32ToOid),
                CastOidToRegType(func::CastOidToRegType),
            ],
            (Int16, String) => Assignment: CastInt16ToString(func::CastInt16ToString),

            //INT32
            (Int32, Bool) => Explicit: CastInt32ToBool(func::CastInt32ToBool),
            (Int32, Oid) => Implicit: CastInt32ToOid(func::CastInt32ToOid),
            (Int32, RegClass) => Implicit: [
                CastInt32ToOid(func::CastInt32ToOid),
                CastOidToRegClass(func::CastOidToRegClass),
            ],
            (Int32, RegProc) => Implicit: [
                CastInt32ToOid(func::CastInt32ToOid),
                CastOidToRegProc(func::CastOidToRegProc),
            ],
            (Int32, RegType) => Implicit: [
                CastInt32ToOid(func::CastInt32ToOid),
                CastOidToRegType(func::CastOidToRegType),
            ],
            (Int32, PgLegacyChar) => Explicit: CastInt32ToPgLegacyChar(func::CastInt32ToPgLegacyChar),
            (Int32, Int16) => Assignment: CastInt32ToInt16(func::CastInt32ToInt16),
            (Int32, Int64) => Implicit: CastInt32ToInt64(func::CastInt32ToInt64),
            (Int32, UInt16) => Assignment: CastInt32ToUint16(func::CastInt32ToUint16),
            (Int32, UInt32) => Assignment: CastInt32ToUint32(func::CastInt32ToUint32),
            (Int32, UInt64) => Assignment: CastInt32ToUint64(func::CastInt32ToUint64),
            (Int32, Float32) => Implicit: CastInt32ToFloat32(func::CastInt32ToFloat32),
            (Int32, Float64) => Implicit: CastInt32ToFloat64(func::CastInt32ToFloat64),
            (Int32, Numeric) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastInt32ToNumeric(func::CastInt32ToNumeric(s))))
            }),
            (Int32, String) => Assignment: CastInt32ToString(func::CastInt32ToString),

            // INT64
            (Int64, Bool) => Explicit: CastInt64ToBool(func::CastInt64ToBool),
            (Int64, Int16) => Assignment: CastInt64ToInt16(func::CastInt64ToInt16),
            (Int64, Int32) => Assignment: CastInt64ToInt32(func::CastInt64ToInt32),
            (Int64, UInt16) => Assignment: CastInt64ToUint16(func::CastInt64ToUint16),
            (Int64, UInt32) => Assignment: CastInt64ToUint32(func::CastInt64ToUint32),
            (Int64, UInt64) => Assignment: CastInt64ToUint64(func::CastInt64ToUint64),
            (Int64, Numeric) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastInt64ToNumeric(func::CastInt64ToNumeric(s))))
            }),
            (Int64, Float32) => Implicit: CastInt64ToFloat32(func::CastInt64ToFloat32),
            (Int64, Float64) => Implicit: CastInt64ToFloat64(func::CastInt64ToFloat64),
            (Int64, Oid) => Implicit: CastInt64ToOid(func::CastInt64ToOid),
            (Int64, RegClass) => Implicit: [
                CastInt64ToOid(func::CastInt64ToOid),
                CastOidToRegClass(func::CastOidToRegClass),
            ],
            (Int64, RegProc) => Implicit: [
                CastInt64ToOid(func::CastInt64ToOid),
                CastOidToRegProc(func::CastOidToRegProc),
            ],
            (Int64, RegType) => Implicit: [
                CastInt64ToOid(func::CastInt64ToOid),
                CastOidToRegType(func::CastOidToRegType),
            ],
            (Int64, String) => Assignment: CastInt64ToString(func::CastInt64ToString),

            // UINT16
            (UInt16, UInt32) => Implicit: CastUint16ToUint32(func::CastUint16ToUint32),
            (UInt16, UInt64) => Implicit: CastUint16ToUint64(func::CastUint16ToUint64),
            (UInt16, Int16) => Assignment: CastUint16ToInt16(func::CastUint16ToInt16),
            (UInt16, Int32) => Implicit: CastUint16ToInt32(func::CastUint16ToInt32),
            (UInt16, Int64) => Implicit: CastUint16ToInt64(func::CastUint16ToInt64),
            (UInt16, Numeric) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastUint16ToNumeric(func::CastUint16ToNumeric(s))))
            }),
            (UInt16, Float32) => Implicit: CastUint16ToFloat32(func::CastUint16ToFloat32),
            (UInt16, Float64) => Implicit: CastUint16ToFloat64(func::CastUint16ToFloat64),
            (UInt16, String) => Assignment: CastUint16ToString(func::CastUint16ToString),

            // UINT32
            (UInt32, UInt16) => Assignment: CastUint32ToUint16(func::CastUint32ToUint16),
            (UInt32, UInt64) => Implicit: CastUint32ToUint64(func::CastUint32ToUint64),
            (UInt32, Int16) => Assignment: CastUint32ToInt16(func::CastUint32ToInt16),
            (UInt32, Int32) => Assignment: CastUint32ToInt32(func::CastUint32ToInt32),
            (UInt32, Int64) => Implicit: CastUint32ToInt64(func::CastUint32ToInt64),
            (UInt32, Numeric) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastUint32ToNumeric(func::CastUint32ToNumeric(s))))
            }),
            (UInt32, Float32) => Implicit: CastUint32ToFloat32(func::CastUint32ToFloat32),
            (UInt32, Float64) => Implicit: CastUint32ToFloat64(func::CastUint32ToFloat64),
            (UInt32, String) => Assignment: CastUint32ToString(func::CastUint32ToString),

            // UINT64
            (UInt64, UInt16) => Assignment: CastUint64ToUint16(func::CastUint64ToUint16),
            (UInt64, UInt32) => Assignment: CastUint64ToUint32(func::CastUint64ToUint32),
            (UInt64, Int16) => Assignment: CastUint64ToInt16(func::CastUint64ToInt16),
            (UInt64, Int32) => Assignment: CastUint64ToInt32(func::CastUint64ToInt32),
            (UInt64, Int64) => Assignment: CastUint64ToInt64(func::CastUint64ToInt64),
            (UInt64, Numeric) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastUint64ToNumeric(func::CastUint64ToNumeric(s))))
            }),
            (UInt64, Float32) => Implicit: CastUint64ToFloat32(func::CastUint64ToFloat32),
            (UInt64, Float64) => Implicit: CastUint64ToFloat64(func::CastUint64ToFloat64),
            (UInt64, String) => Assignment: CastUint64ToString(func::CastUint64ToString),

            // MZ_TIMESTAMP
            (MzTimestamp, String) => Assignment: CastMzTimestampToString(func::CastMzTimestampToString),
            (MzTimestamp, Timestamp) => Assignment: CastMzTimestampToTimestamp(func::CastMzTimestampToTimestamp),
            (MzTimestamp, TimestampTz) => Assignment: CastMzTimestampToTimestampTz(func::CastMzTimestampToTimestampTz),
            (String, MzTimestamp) => Assignment: CastStringToMzTimestamp(func::CastStringToMzTimestamp),
            (UInt64, MzTimestamp) => Implicit: CastUint64ToMzTimestamp(func::CastUint64ToMzTimestamp),
            (UInt32, MzTimestamp) => Implicit: CastUint32ToMzTimestamp(func::CastUint32ToMzTimestamp),
            (Int64, MzTimestamp) => Implicit: CastInt64ToMzTimestamp(func::CastInt64ToMzTimestamp),
            (Int32, MzTimestamp) => Implicit: CastInt32ToMzTimestamp(func::CastInt32ToMzTimestamp),
            (Numeric, MzTimestamp) => Implicit: CastNumericToMzTimestamp(func::CastNumericToMzTimestamp),
            (Timestamp, MzTimestamp) => Implicit: CastTimestampToMzTimestamp(func::CastTimestampToMzTimestamp),
            (TimestampTz, MzTimestamp) => Implicit: CastTimestampTzToMzTimestamp(func::CastTimestampTzToMzTimestamp),
            (Date, MzTimestamp) => Implicit: CastDateToMzTimestamp(func::CastDateToMzTimestamp),

            // OID
            (Oid, Int32) => Assignment: CastOidToInt32(func::CastOidToInt32),
            (Oid, Int64) => Assignment: CastOidToInt32(func::CastOidToInt32),
            (Oid, String) => Explicit: CastOidToString(func::CastOidToString),
            (Oid, RegClass) => Implicit: CastOidToRegClass(func::CastOidToRegClass),
            (Oid, RegProc) => Implicit: CastOidToRegProc(func::CastOidToRegProc),
            (Oid, RegType) => Implicit: CastOidToRegType(func::CastOidToRegType),

            // REGCLASS
            (RegClass, Oid) => Implicit: CastRegClassToOid(func::CastRegClassToOid),
            (RegClass, String) => Explicit: sql_impl_cast(&REGCLASS_TO_STRING),

            // REGPROC
            (RegProc, Oid) => Implicit: CastRegProcToOid(func::CastRegProcToOid),
            (RegProc, String) => Explicit: sql_impl_cast(&REGPROC_TO_STRING),

            // REGTYPE
            (RegType, Oid) => Implicit: CastRegTypeToOid(func::CastRegTypeToOid),
            (RegType, String) => Explicit: sql_impl_cast(&REGTYPE_TO_STRING),

            // FLOAT32
            (Float32, Int16) => Assignment: CastFloat32ToInt16(func::CastFloat32ToInt16),
            (Float32, Int32) => Assignment: CastFloat32ToInt32(func::CastFloat32ToInt32),
            (Float32, Int64) => Assignment: CastFloat32ToInt64(func::CastFloat32ToInt64),
            (Float32, UInt16) => Assignment: CastFloat32ToUint16(func::CastFloat32ToUint16),
            (Float32, UInt32) => Assignment: CastFloat32ToUint32(func::CastFloat32ToUint32),
            (Float32, UInt64) => Assignment: CastFloat32ToUint64(func::CastFloat32ToUint64),
            (Float32, Float64) => Implicit: CastFloat32ToFloat64(func::CastFloat32ToFloat64),
            (Float32, Numeric) => Assignment: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastFloat32ToNumeric(func::CastFloat32ToNumeric(s))))
            }),
            (Float32, String) => Assignment: CastFloat32ToString(func::CastFloat32ToString),

            // FLOAT64
            (Float64, Int16) => Assignment: CastFloat64ToInt16(func::CastFloat64ToInt16),
            (Float64, Int32) => Assignment: CastFloat64ToInt32(func::CastFloat64ToInt32),
            (Float64, Int64) => Assignment: CastFloat64ToInt64(func::CastFloat64ToInt64),
            (Float64, UInt16) => Assignment: CastFloat64ToUint16(func::CastFloat64ToUint16),
            (Float64, UInt32) => Assignment: CastFloat64ToUint32(func::CastFloat64ToUint32),
            (Float64, UInt64) => Assignment: CastFloat64ToUint64(func::CastFloat64ToUint64),
            (Float64, Float32) => Assignment: CastFloat64ToFloat32(func::CastFloat64ToFloat32),
            (Float64, Numeric) => Assignment: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastFloat64ToNumeric(func::CastFloat64ToNumeric(s))))
            }),
            (Float64, String) => Assignment: CastFloat64ToString(func::CastFloat64ToString),

            // DATE
            (Date, Timestamp) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let p = to_type.unwrap_timestamp_precision();
                Some(move |e: HirScalarExpr| e.call_unary(CastDateToTimestamp(func::CastDateToTimestamp(p))))
            }),
            (Date, TimestampTz) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let p = to_type.unwrap_timestamp_precision();
                Some(move |e: HirScalarExpr| e.call_unary(CastDateToTimestampTz(func::CastDateToTimestampTz(p))))
            }),
            (Date, String) => Assignment: CastDateToString(func::CastDateToString),

            // TIME
            (Time, Interval) => Implicit: CastTimeToInterval(func::CastTimeToInterval),
            (Time, String) => Assignment: CastTimeToString(func::CastTimeToString),

            // TIMESTAMP
            (Timestamp, Date) => Assignment: CastTimestampToDate(func::CastTimestampToDate),
            (Timestamp, TimestampTz) => Implicit: CastTemplate::new(|_ecx, _ccx, from_type, to_type| {
                let from = from_type.unwrap_timestamp_precision();
                let to = to_type.unwrap_timestamp_precision();
                Some(move |e: HirScalarExpr| e.call_unary(CastTimestampToTimestampTz(func::CastTimestampToTimestampTz{from, to})))
            }),
            (Timestamp, Timestamp) => Assignment: CastTemplate::new(|_ecx, _ccx, from_type, to_type| {
                let from = from_type.unwrap_timestamp_precision();
                let to = to_type.unwrap_timestamp_precision();
                Some(move |e: HirScalarExpr| e.call_unary(AdjustTimestampPrecision(func::AdjustTimestampPrecision{from, to})))
            }),
            (Timestamp, Time) => Assignment: CastTimestampToTime(func::CastTimestampToTime),
            (Timestamp, String) => Assignment: CastTimestampToString(func::CastTimestampToString),

            // TIMESTAMPTZ
            (TimestampTz, Date) => Assignment: CastTimestampTzToDate(func::CastTimestampTzToDate),
            (TimestampTz, Timestamp) => Assignment: CastTemplate::new(|_ecx, _ccx, from_type, to_type| {
                let from = from_type.unwrap_timestamp_precision();
                let to = to_type.unwrap_timestamp_precision();
                Some(move |e: HirScalarExpr| e.call_unary(CastTimestampTzToTimestamp(func::CastTimestampTzToTimestamp{from, to})))
            }),
            (TimestampTz, TimestampTz) => Assignment: CastTemplate::new(|_ecx, _ccx, from_type, to_type| {
                let from = from_type.unwrap_timestamp_precision();
                let to = to_type.unwrap_timestamp_precision();
                Some(move |e: HirScalarExpr| e.call_unary(AdjustTimestampTzPrecision(func::AdjustTimestampTzPrecision{from, to})))
            }),
            (TimestampTz, Time) => Assignment: CastTimestampTzToTime(func::CastTimestampTzToTime),
            (TimestampTz, String) => Assignment: CastTimestampTzToString(func::CastTimestampTzToString),

            // INTERVAL
            (Interval, Time) => Assignment: CastIntervalToTime(func::CastIntervalToTime),
            (Interval, String) => Assignment: CastIntervalToString(func::CastIntervalToString),

            // BYTES
            (Bytes, String) => Assignment: CastBytesToString(func::CastBytesToString),

            // STRING
            (String, Bool) => Explicit: CastStringToBool(func::CastStringToBool),
            (String, Int16) => Explicit: CastStringToInt16(func::CastStringToInt16),
            (String, Int32) => Explicit: CastStringToInt32(func::CastStringToInt32),
            (String, Int64) => Explicit: CastStringToInt64(func::CastStringToInt64),
            (String, UInt16) => Explicit: CastStringToUint16(func::CastStringToUint16),
            (String, UInt32) => Explicit: CastStringToUint32(func::CastStringToUint32),
            (String, UInt64) => Explicit: CastStringToUint64(func::CastStringToUint64),
            (String, Oid) => Explicit: CastStringToOid(func::CastStringToOid),

            // STRING to REG*
            // A reg* type represents a specific type of object by oid.
            // Converting from string to reg* does a lookup of the object name
            // in the corresponding mz_catalog table and expects exactly one object to match it.
            // You can also specify (in postgres) a string that's a valid
            // int4 and it'll happily cast it (without verifying that the int4 matches
            // an object oid).
            // TODO: Support the correct error code for does not exist (42883).
            (String, RegClass) => Explicit: sql_impl_cast_per_context(
                &[
                    (CastContext::Explicit, &STRING_TO_REGCLASS_EXPLICIT),
                    (CastContext::Coerced, &STRING_TO_REGCLASS_COERCED)
                ]
            ),
            (String, RegProc) => Explicit: sql_impl_cast(&STRING_TO_REGPROC),
            (String, RegType) => Explicit: sql_impl_cast(&STRING_TO_REGTYPE),

            (String, Float32) => Explicit: CastStringToFloat32(func::CastStringToFloat32),
            (String, Float64) => Explicit: CastStringToFloat64(func::CastStringToFloat64),
            (String, Numeric) => Explicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToNumeric(func::CastStringToNumeric(s))))
            }),
            (String, Date) => Explicit: CastStringToDate(func::CastStringToDate),
            (String, Time) => Explicit: CastStringToTime(func::CastStringToTime),
            (String, Timestamp) => Explicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let p = to_type.unwrap_timestamp_precision();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToTimestamp(func::CastStringToTimestamp(p))))
            }),
            (String, TimestampTz) => Explicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let p = to_type.unwrap_timestamp_precision();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToTimestampTz(func::CastStringToTimestampTz(p))))
            }),
            (String, Interval) => Explicit: CastStringToInterval(func::CastStringToInterval),
            (String, Bytes) => Explicit: CastStringToBytes(func::CastStringToBytes),
            (String, Jsonb) => Explicit: CastStringToJsonb(func::CastStringToJsonb),
            (String, Uuid) => Explicit: CastStringToUuid(func::CastStringToUuid),
            (String, Array) => Explicit: CastTemplate::new(|ecx, ccx, from_type, to_type| {
                let return_ty = to_type.clone();
                let to_el_type = to_type.unwrap_array_element_type();
                let cast_expr = plan_hypothetical_cast(ecx, ccx, from_type, to_el_type)?;
                Some(|e: HirScalarExpr| e.call_unary(UnaryFunc::CastStringToArray(func::CastStringToArray {
                    return_ty,
                    cast_expr: Box::new(cast_expr),
                })))
            }),
            (String, List) => Explicit: CastTemplate::new(|ecx, ccx, from_type, to_type| {
                let return_ty = to_type.clone();
                let to_el_type = to_type.unwrap_list_element_type();
                let cast_expr = plan_hypothetical_cast(ecx, ccx, from_type, to_el_type)?;
                Some(|e: HirScalarExpr| e.call_unary(UnaryFunc::CastStringToList(func::CastStringToList {
                    return_ty,
                    cast_expr: Box::new(cast_expr),
                })))
            }),
            (String, Map) => Explicit: CastTemplate::new(|ecx, ccx, from_type, to_type| {
                let return_ty = to_type.clone();
                let to_val_type = to_type.unwrap_map_value_type();
                let cast_expr = plan_hypothetical_cast(ecx, ccx, from_type, to_val_type)?;
                Some(|e: HirScalarExpr| e.call_unary(UnaryFunc::CastStringToMap(func::CastStringToMap {
                    return_ty,
                    cast_expr: Box::new(cast_expr),
                })))
            }),
            (String, Range) => Explicit: CastTemplate::new(|ecx, ccx, from_type, to_type| {
                let return_ty = to_type.clone();
                let to_el_type = to_type.unwrap_range_element_type();
                let cast_expr = plan_hypothetical_cast(ecx, ccx, from_type, to_el_type)?;
                Some(|e: HirScalarExpr| e.call_unary(UnaryFunc::CastStringToRange(func::CastStringToRange {
                    return_ty,
                    cast_expr: Box::new(cast_expr),
                })))
            }),
            (String, Int2Vector) => Explicit: CastStringToInt2Vector(func::CastStringToInt2Vector),
            (String, Char) => Implicit: CastTemplate::new(|_ecx, ccx, _from_type, to_type| {
                let length = to_type.unwrap_char_length();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToChar(func::CastStringToChar {length, fail_on_len: ccx != CastContext::Explicit})))
            }),
            (String, VarChar) => Implicit: CastTemplate::new(|_ecx, ccx, _from_type, to_type| {
                let length = to_type.unwrap_varchar_max_length();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToVarChar(func::CastStringToVarChar {length, fail_on_len: ccx != CastContext::Explicit})))
            }),
            (String, PgLegacyChar) => Assignment: CastStringToPgLegacyChar(func::CastStringToPgLegacyChar),
            // CHAR
            (Char, String) => Implicit: CastCharToString(func::CastCharToString),
            (Char, Char) => Implicit: CastTemplate::new(|_ecx, ccx, _from_type, to_type| {
                let length = to_type.unwrap_char_length();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToChar(func::CastStringToChar {length, fail_on_len: ccx != CastContext::Explicit})))
            }),
            (Char, VarChar) => Implicit: CastTemplate::new(|_ecx, ccx, _from_type, to_type| {
                let length = to_type.unwrap_varchar_max_length();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToVarChar(func::CastStringToVarChar {length, fail_on_len: ccx != CastContext::Explicit})))
            }),
            (Char, PgLegacyChar) => Assignment: CastStringToPgLegacyChar(func::CastStringToPgLegacyChar),

            // VARCHAR
            (VarChar, String) => Implicit: CastVarCharToString(func::CastVarCharToString),
            (VarChar, Char) => Implicit: CastTemplate::new(|_ecx, ccx, _from_type, to_type| {
                let length = to_type.unwrap_char_length();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToChar(func::CastStringToChar {length, fail_on_len: ccx != CastContext::Explicit})))
            }),
            (VarChar, VarChar) => Implicit: CastTemplate::new(|_ecx, ccx, _from_type, to_type| {
                let length = to_type.unwrap_varchar_max_length();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToVarChar(func::CastStringToVarChar {length, fail_on_len: ccx != CastContext::Explicit})))
            }),
            (VarChar, PgLegacyChar) => Assignment: CastStringToPgLegacyChar(func::CastStringToPgLegacyChar),

            // PG LEGACY CHAR
            (PgLegacyChar, String) => Implicit: CastPgLegacyCharToString(func::CastPgLegacyCharToString),
            (PgLegacyChar, Char) => Assignment: CastPgLegacyCharToChar(func::CastPgLegacyCharToChar),
            (PgLegacyChar, VarChar) => Assignment: CastPgLegacyCharToVarChar(func::CastPgLegacyCharToVarChar),
            (PgLegacyChar, Int32) => Explicit: CastPgLegacyCharToInt32(func::CastPgLegacyCharToInt32),

            // PG LEGACY NAME
            // Under the hood VarChars and Name's are just Strings, so we can re-use existing methods
            // on Strings and VarChars instead of defining new ones.
            (PgLegacyName, String) => Implicit: CastVarCharToString(func::CastVarCharToString),
            (PgLegacyName, Char) => Assignment: CastTemplate::new(|_ecx, ccx, _from_type, to_type| {
                let length = to_type.unwrap_char_length();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToChar(func::CastStringToChar {length, fail_on_len: ccx != CastContext::Explicit})))
            }),
            (PgLegacyName, VarChar) => Assignment: CastTemplate::new(|_ecx, ccx, _from_type, to_type| {
                let length = to_type.unwrap_varchar_max_length();
                Some(move |e: HirScalarExpr| e.call_unary(CastStringToVarChar(func::CastStringToVarChar {length, fail_on_len: ccx != CastContext::Explicit})))
            }),
            (String, PgLegacyName) => Implicit: CastStringToPgLegacyName(func::CastStringToPgLegacyName),
            (Char, PgLegacyName) => Implicit: CastStringToPgLegacyName(func::CastStringToPgLegacyName),
            (VarChar, PgLegacyName) => Implicit: CastStringToPgLegacyName(func::CastStringToPgLegacyName),

            // RECORD
            (Record, String) => Assignment: CastTemplate::new(|_ecx, _ccx, from_type, _to_type| {
                let ty = from_type.clone();
                Some(|e: HirScalarExpr| e.call_unary(CastRecordToString(func::CastRecordToString { ty })))
            }),
            (Record, Record) => Implicit: CastTemplate::new(|ecx, ccx, from_type, to_type| {
                if from_type.unwrap_record_element_type().len() != to_type.unwrap_record_element_type().len() {
                    return None;
                }

                if let (l @ ScalarType::Record {custom_id: Some(..), ..}, r) = (from_type, to_type) {
                    // Changing `from`'s custom_id requires at least Assignment context
                    if ccx == CastContext::Implicit && l != r {
                        return None;
                    }
                }

                let cast_exprs = from_type.unwrap_record_element_type()
                    .iter()
                    .zip_eq(to_type.unwrap_record_element_type())
                    .map(|(f, t)| plan_hypothetical_cast(ecx, ccx, f, t))
                    .collect::<Option<Box<_>>>()?;
                let to = to_type.clone();
                Some(|e: HirScalarExpr| e.call_unary(CastRecord1ToRecord2(func::CastRecord1ToRecord2 { return_ty: to, cast_exprs })))
            }),

            // ARRAY
            (Array, String) => Assignment: CastTemplate::new(|_ecx, _ccx, from_type, _to_type| {
                let ty = from_type.clone();
                Some(|e: HirScalarExpr| e.call_unary(CastArrayToString(func::CastArrayToString { ty })))
            }),
            (Array, List) => Explicit: CastArrayToListOneDim(func::CastArrayToListOneDim),
            (Array, Array) => Explicit: CastTemplate::new(|ecx, ccx, from_type, to_type| {
                let inner_from_type = from_type.unwrap_array_element_type();
                let inner_to_type = to_type.unwrap_array_element_type();
                let cast_expr = plan_hypothetical_cast(ecx, ccx, inner_from_type, inner_to_type)?;
                let return_ty = to_type.clone();

                Some(move |e: HirScalarExpr| e.call_unary(CastArrayToArray(func::CastArrayToArray { return_ty, cast_expr: Box::new(cast_expr) })))
            }),

            // INT2VECTOR
            (Int2Vector, Array) => Implicit: CastTemplate::new(|_ecx, _ccx, _from_type, _to_type| {
                Some(|e: HirScalarExpr| e.call_unary(UnaryFunc::CastInt2VectorToArray(func::CastInt2VectorToArray)))
            }),
            (Int2Vector, String) => Explicit: CastInt2VectorToString(func::CastInt2VectorToString),

            // LIST
            (List, String) => Assignment: CastTemplate::new(|_ecx, _ccx, from_type, _to_type| {
                let ty = from_type.clone();
                Some(|e: HirScalarExpr| e.call_unary(CastListToString(func::CastListToString { ty })))
            }),
            (List, List) => Implicit: CastTemplate::new(|ecx, ccx, from_type, to_type| {

                if let (l @ ScalarType::List {custom_id: Some(..), ..}, r) = (from_type, to_type) {
                    // Changing `from`'s custom_id requires at least Assignment context
                    if ccx == CastContext::Implicit && !l.base_eq(r) {
                        return None;
                    }
                }

                let return_ty = to_type.clone();
                let from_el_type = from_type.unwrap_list_element_type();
                let to_el_type = to_type.unwrap_list_element_type();
                let cast_expr = plan_hypothetical_cast(ecx, ccx, from_el_type, to_el_type)?;
                Some(|e: HirScalarExpr| e.call_unary(UnaryFunc::CastList1ToList2(func::CastList1ToList2 {
                    return_ty,
                    cast_expr: Box::new(cast_expr),
                })))
            }),

            // MAP
            (Map, String) => Assignment: CastTemplate::new(|_ecx, _ccx, from_type, _to_type| {
                let ty = from_type.clone();
                Some(|e: HirScalarExpr| e.call_unary(CastMapToString(func::CastMapToString { ty })))
            }),

            // JSONB
            (Jsonb, Bool) => Explicit: CastJsonbToBool(func::CastJsonbToBool),
            (Jsonb, Int16) => Explicit: CastJsonbToInt16(func::CastJsonbToInt16),
            (Jsonb, Int32) => Explicit: CastJsonbToInt32(func::CastJsonbToInt32),
            (Jsonb, Int64) => Explicit: CastJsonbToInt64(func::CastJsonbToInt64),
            (Jsonb, Float32) => Explicit: CastJsonbToFloat32(func::CastJsonbToFloat32),
            (Jsonb, Float64) => Explicit: CastJsonbToFloat64(func::CastJsonbToFloat64),
            (Jsonb, Numeric) => Explicit: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let s = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| e.call_unary(CastJsonbToNumeric(func::CastJsonbToNumeric(s))))
            }),
            (Jsonb, String) => Assignment: CastJsonbToString(func::CastJsonbToString),

            // UUID
            (Uuid, String) => Assignment: CastUuidToString(func::CastUuidToString),

            // Numeric
            (Numeric, Numeric) => Assignment: CastTemplate::new(|_ecx, _ccx, _from_type, to_type| {
                let scale = to_type.unwrap_numeric_max_scale();
                Some(move |e: HirScalarExpr| match scale {
                    None => e,
                    Some(scale) => e.call_unary(UnaryFunc::AdjustNumericScale(func::AdjustNumericScale(scale))),
                })
            }),
            (Numeric, Float32) => Implicit: CastNumericToFloat32(func::CastNumericToFloat32),
            (Numeric, Float64) => Implicit: CastNumericToFloat64(func::CastNumericToFloat64),
            (Numeric, Int16) => Assignment: CastNumericToInt16(func::CastNumericToInt16),
            (Numeric, Int32) => Assignment: CastNumericToInt32(func::CastNumericToInt32),
            (Numeric, Int64) => Assignment: CastNumericToInt64(func::CastNumericToInt64),
            (Numeric, UInt16) => Assignment: CastNumericToUint16(func::CastNumericToUint16),
            (Numeric, UInt32) => Assignment: CastNumericToUint32(func::CastNumericToUint32),
            (Numeric, UInt64) => Assignment: CastNumericToUint64(func::CastNumericToUint64),
            (Numeric, String) => Assignment: CastNumericToString(func::CastNumericToString),

            // Range
            (Range, String) => Assignment: CastTemplate::new(|_ecx, _ccx, from_type, _to_type| {
                let ty = from_type.clone();
                Some(|e: HirScalarExpr| e.call_unary(CastRangeToString(func::CastRangeToString { ty })))
            }),

            // MzAclItem
            (MzAclItem, String) => Explicit: sql_impl_cast("(
                SELECT
                    (CASE
                        WHEN grantee_role_id = 'p' THEN ''
                        ELSE COALESCE(grantee_role.name, grantee_role_id)
                    END)
                    || '='
                    || mz_internal.mz_aclitem_privileges($1)
                    || '/'
                    || COALESCE(grantor_role.name, grantor_role_id)
                FROM
                    (SELECT mz_internal.mz_aclitem_grantee($1) AS grantee_role_id),
                    (SELECT mz_internal.mz_aclitem_grantor($1) AS grantor_role_id)
                LEFT JOIN mz_catalog.mz_roles AS grantee_role ON grantee_role_id = grantee_role.id
                LEFT JOIN mz_catalog.mz_roles AS grantor_role ON grantor_role_id = grantor_role.id
            )"),
            (MzAclItem, AclItem) => Explicit: sql_impl_cast("(
                SELECT makeaclitem(
                    (CASE mz_internal.mz_aclitem_grantee($1)
                        WHEN 'p' THEN 0
                        ELSE (SELECT oid FROM mz_catalog.mz_roles WHERE id = mz_internal.mz_aclitem_grantee($1))
                    END),
                    (SELECT oid FROM mz_catalog.mz_roles WHERE id = mz_internal.mz_aclitem_grantor($1)),
                    (SELECT array_to_string(mz_internal.mz_format_privileges(mz_internal.mz_aclitem_privileges($1)), ',')),
                    -- GRANT OPTION isn't implemented so we hardcode false.
                    false
                )
            )"),

            // AclItem
            (AclItem, String) => Explicit: sql_impl_cast("(
                SELECT
                    (CASE grantee_oid
                        WHEN 0 THEN ''
                        ELSE COALESCE(grantee_role.name, grantee_oid::text)
                    END)
                    || '='
                    || mz_internal.aclitem_privileges($1)
                    || '/'
                    || COALESCE(grantor_role.name, grantor_oid::text)
                FROM
                    (SELECT mz_internal.aclitem_grantee($1) AS grantee_oid),
                    (SELECT mz_internal.aclitem_grantor($1) AS grantor_oid)
                LEFT JOIN mz_catalog.mz_roles AS grantee_role ON grantee_oid = grantee_role.oid
                LEFT JOIN mz_catalog.mz_roles AS grantor_role ON grantor_oid = grantor_role.oid
            )"),
            (AclItem, MzAclItem) => Explicit: sql_impl_cast("(
                SELECT mz_internal.make_mz_aclitem(
                    (CASE mz_internal.aclitem_grantee($1)
                        WHEN 0 THEN 'p'
                        ELSE (SELECT id FROM mz_catalog.mz_roles WHERE oid = mz_internal.aclitem_grantee($1))
                    END),
                    (SELECT id FROM mz_catalog.mz_roles WHERE oid = mz_internal.aclitem_grantor($1)),
                    (SELECT array_to_string(mz_internal.mz_format_privileges(mz_internal.aclitem_privileges($1)), ','))
                )
            )")
        }
    },
);

/// Get casts directly between two [`ScalarType`]s, with control over the
/// allowed [`CastContext`].
fn get_cast(
    ecx: &ExprContext,
    ccx: CastContext,
    from: &ScalarType,
    to: &ScalarType,
) -> Option<Cast> {
    use CastContext::*;

    if from == to || (ccx == Implicit && from.base_eq(to)) {
        return Some(Box::new(|expr| expr));
    }

    let imp = VALID_CASTS.get(&(from.into(), to.into()))?;
    let template = if ccx >= imp.context {
        Some(&imp.template)
    } else {
        None
    };
    template.and_then(|template| (template.0)(ecx, ccx, from, to))
}

/// Converts an expression to `ScalarType::String`.
///
/// All types are convertible to string, so this never fails.
pub fn to_string(ecx: &ExprContext, expr: HirScalarExpr) -> HirScalarExpr {
    plan_cast(ecx, CastContext::Explicit, expr, &ScalarType::String).expect("cast known to exist")
}

/// Converts an expression to `ScalarType::Jsonb`.
///
/// The rules are as follows:
///   * `ScalarType::Boolean`s become JSON booleans.
///   * All numeric types are converted to `Float64`s, then become JSON numbers.
///   * Records are converted to a JSON object where the record's field names
///     are the keys of the object, and the record's fields are recursively
///     converted to JSON by `to_jsonb`.
///   * Other types are converted to strings by their usual cast function an
//      become JSON strings.
pub fn to_jsonb(ecx: &ExprContext, expr: HirScalarExpr) -> HirScalarExpr {
    use ScalarType::*;

    match ecx.scalar_type(&expr) {
        Bool | Jsonb | Numeric { .. } => {
            expr.call_unary(UnaryFunc::CastJsonbableToJsonb(func::CastJsonbableToJsonb))
        }
        Int16 | Int32 | Int64 | UInt16 | UInt32 | UInt64 | Float32 | Float64 => plan_cast(
            ecx,
            CastContext::Explicit,
            expr,
            &Numeric { max_scale: None },
        )
        .expect("cast known to exist")
        .call_unary(UnaryFunc::CastJsonbableToJsonb(func::CastJsonbableToJsonb)),
        Record { fields, .. } => {
            let mut exprs = vec![];
            for (i, (name, _ty)) in fields.iter().enumerate() {
                exprs.push(HirScalarExpr::literal(
                    Datum::String(name),
                    ScalarType::String,
                ));
                exprs.push(to_jsonb(
                    ecx,
                    expr.clone()
                        .call_unary(UnaryFunc::RecordGet(func::RecordGet(i))),
                ));
            }
            HirScalarExpr::call_variadic(VariadicFunc::JsonbBuildObject, exprs)
        }
        ref ty @ List {
            ref element_type, ..
        }
        | ref ty @ Array(ref element_type) => {
            // Construct a new expression context with one column whose type
            // is the container's element type.
            let qcx = QueryContext::root(ecx.qcx.scx, ecx.qcx.lifetime);
            let ecx = ExprContext {
                qcx: &qcx,
                name: "to_jsonb",
                scope: &Scope::empty(),
                relation_type: &RelationType::new(vec![element_type.clone().nullable(true)]),
                allow_aggregates: false,
                allow_subqueries: false,
                allow_parameters: false,
                allow_windows: false,
            };

            // Create an element-casting expression by calling `to_jsonb` on
            // an expression that references the first column in a row.
            let cast_element = to_jsonb(&ecx, HirScalarExpr::column(0));
            let cast_element = cast_element
                .lower_uncorrelated()
                .expect("to_jsonb does not produce correlated expressions on uncorrelated input");

            // The `Cast{Array|List}ToJsonb` functions take the element-casting
            // expression as an argument and evaluate the expression against
            // each element of the container at runtime.
            let func = match ty {
                List { .. } => UnaryFunc::CastListToJsonb(CastListToJsonb {
                    cast_element: Box::new(cast_element),
                }),
                Array { .. } => UnaryFunc::CastArrayToJsonb(CastArrayToJsonb {
                    cast_element: Box::new(cast_element),
                }),
                _ => unreachable!("validated above"),
            };

            expr.call_unary(func)
        }
        Date
        | Time
        | Timestamp { .. }
        | TimestampTz { .. }
        | Interval
        | PgLegacyChar
        | PgLegacyName
        | Bytes
        | String
        | Char { .. }
        | VarChar { .. }
        | Uuid
        | Oid
        | Map { .. }
        | RegProc
        | RegType
        | RegClass
        | Int2Vector
        | MzTimestamp
        | Range { .. }
        | MzAclItem
        | AclItem => to_string(ecx, expr)
            .call_unary(UnaryFunc::CastJsonbableToJsonb(func::CastJsonbableToJsonb)),
    }
}

/// Guesses the most-common type among a set of [`ScalarType`]s that all members
/// can be cast to. Returns `None` if a common type cannot be deduced.
///
/// Note that this function implements the type-determination components of
/// Postgres' ["`UNION`, `CASE`, and Related Constructs"][union-type-conv] type
/// conversion.
///
/// [union-type-conv]: https://www.postgresql.org/docs/12/typeconv-union-case.html
pub fn guess_best_common_type(
    ecx: &ExprContext,
    types: &[CoercibleScalarType],
) -> Result<ScalarType, PlanError> {
    // This function is a translation of `select_common_type` in PostgreSQL with
    // the addition of our near match logic, which supports Materialize
    // non-linear type promotions.
    // https://github.com/postgres/postgres/blob/d1b307eef/src/backend/parser/parse_coerce.c#L1288-L1308

    // If every type is a literal record with the same number of fields, the
    // best common type is a record with that number of fields. We recursively
    // guess the best type for each field.
    if let Some(CoercibleScalarType::Record(field_tys)) = types.first() {
        if types
            .iter()
            .all(|t| matches!(t, CoercibleScalarType::Record(fts) if field_tys.len() == fts.len()))
        {
            let mut fields = vec![];
            for i in 0..field_tys.len() {
                let name = ColumnName::from(format!("f{}", fields.len() + 1));
                let mut guesses = vec![];
                let mut nullable = false;
                for ty in types {
                    let field_ty = match ty {
                        CoercibleScalarType::Record(fts) => fts[i].clone(),
                        _ => unreachable!(),
                    };
                    if field_ty.nullable() {
                        nullable = true;
                    }
                    guesses.push(field_ty.scalar_type());
                }
                let guess = guess_best_common_type(ecx, &guesses)?;
                fields.push((name, guess.nullable(nullable)));
            }
            return Ok(ScalarType::Record {
                fields: fields.into(),
                custom_id: None,
            });
        }
    }

    // Remove unknown types, and collect them.
    let mut types: Vec<_> = types.into_iter().filter_map(|v| v.as_coerced()).collect();

    // In the case of mixed ints and uints, replace uints with their near match
    let contains_int = types
        .iter()
        .any(|t| matches!(t, ScalarType::Int16 | ScalarType::Int32 | ScalarType::Int64));

    for t in types.iter_mut() {
        if contains_int
            && matches!(
                t,
                ScalarType::UInt16 | ScalarType::UInt32 | ScalarType::UInt64
            )
        {
            *t = t.near_match().expect("unsigned ints have near matches")
        }
    }

    let mut types = types.iter();

    let mut candidate = match types.next() {
        // If no known types, fall back to `String`.
        None => return Ok(ScalarType::String),
        // Start by guessing the first type.
        Some(t) => t,
    };

    let preferred_type = TypeCategory::from_type(candidate).preferred_type();

    for typ in types {
        if TypeCategory::from_type(candidate) != TypeCategory::from_type(typ) {
            // The next type is in a different category; give up.
            sql_bail!(
                "{} types {} and {} cannot be matched",
                ecx.name,
                ecx.humanize_scalar_type(candidate, false),
                ecx.humanize_scalar_type(typ, false),
            );
        };

        // If this type is the preferred type, make it the candidate.
        if preferred_type.as_ref() != Some(candidate)
            && can_cast(ecx, CastContext::Implicit, candidate, typ)
            && !can_cast(ecx, CastContext::Implicit, typ, candidate)
        {
            // The current candidate is not the preferred type for its category
            // and the next type is implicitly convertible to the current
            // candidate, but not vice-versa, so take the next type as the new
            // candidate.
            candidate = typ;
        }
    }
    Ok(candidate.without_modifiers())
}

pub fn plan_coerce<'a>(
    ecx: &'a ExprContext,
    e: CoercibleScalarExpr,
    coerce_to: &ScalarType,
) -> Result<HirScalarExpr, PlanError> {
    use CoercibleScalarExpr::*;

    Ok(match e {
        Coerced(e) => e,

        LiteralNull => HirScalarExpr::literal_null(coerce_to.clone()),

        LiteralString(s) => {
            let lit = HirScalarExpr::literal(Datum::String(&s), ScalarType::String);
            // Per PostgreSQL, string literal explicitly casts to the base type.
            // The caller is responsible for applying any desired modifiers
            // (with either implicit or explicit semantics) via a separate call
            // to `plan_cast`.
            let coerce_to_base = &coerce_to.without_modifiers();
            plan_cast(ecx, CastContext::Coerced, lit, coerce_to_base)?
        }

        LiteralRecord(exprs) => {
            let arity = exprs.len();
            let coercions = match coerce_to {
                ScalarType::Record { fields, .. } if fields.len() == arity => fields
                    .iter()
                    .map(|(_name, ty)| &ty.scalar_type)
                    .cloned()
                    .collect(),
                _ => vec![ScalarType::String; exprs.len()],
            };
            let mut out = vec![];
            for (e, coerce_to) in exprs.into_iter().zip(coercions) {
                out.push(plan_coerce(ecx, e, &coerce_to)?);
            }
            HirScalarExpr::call_variadic(
                VariadicFunc::RecordCreate {
                    field_names: (0..arity)
                        .map(|i| ColumnName::from(format!("f{}", i + 1)))
                        .collect(),
                },
                out,
            )
        }

        Parameter(n) => {
            let prev = ecx.param_types().borrow_mut().insert(n, coerce_to.clone());
            if let Some(prev) = prev {
                if prev != *coerce_to {
                    sql_bail!(
                        "there are contradicting constraints for the type of parameter ${}: should be both {} and {}",
                        n,
                        ecx.humanize_scalar_type(&prev, false),
                        ecx.humanize_scalar_type(coerce_to, false),
                    );
                }
            }
            HirScalarExpr::parameter(n)
        }
    })
}

/// Similar to `plan_cast`, but for situations where you only know the type of
/// the input expression (`from`) and not the expression itself. The returned
/// expression refers to the first column of some imaginary row, where the first
/// column is assumed to have type `from`.
///
/// If casting from `from` to `to` is not possible, returns `None`.
pub fn plan_hypothetical_cast(
    ecx: &ExprContext,
    ccx: CastContext,
    from: &ScalarType,
    to: &ScalarType,
) -> Option<mz_expr::MirScalarExpr> {
    // Reconstruct an expression context where the expression is evaluated on
    // the "first column" of some imaginary row.
    let mut scx = ecx.qcx.scx.clone();
    scx.param_types = RefCell::new(BTreeMap::new());
    let qcx = QueryContext::root(&scx, ecx.qcx.lifetime);
    let relation_type = RelationType {
        column_types: vec![ColumnType {
            nullable: true,
            scalar_type: from.clone(),
        }],
        keys: vec![vec![0]],
    };
    let ecx = ExprContext {
        qcx: &qcx,
        name: "plan_hypothetical_cast",
        scope: &Scope::empty(),
        relation_type: &relation_type,
        allow_aggregates: false,
        allow_subqueries: true,
        allow_parameters: true,
        allow_windows: false,
    };

    let col_expr = HirScalarExpr::column(0);

    // Determine the `ScalarExpr` required to cast our column to the target
    // component type.
    plan_cast(&ecx, ccx, col_expr, to)
        .ok()?
        // TODO(jkosh44) Support casts that have correlated implementations.
        .lower_uncorrelated()
        .ok()
}

/// Plans a cast between [`ScalarType`]s, specifying which types of casts are
/// permitted using [`CastContext`].
///
/// # Errors
///
/// If a cast between the `ScalarExpr`'s base type and the specified type is:
/// - Not possible, e.g. `Bytes` to `Interval`
/// - Not permitted, e.g. implicitly casting from `Float64` to `Float32`.
/// - Not implemented yet
pub fn plan_cast(
    ecx: &ExprContext,
    ccx: CastContext,
    expr: HirScalarExpr,
    to: &ScalarType,
) -> Result<HirScalarExpr, PlanError> {
    let from = ecx.scalar_type(&expr);

    // Close over `ccx`, `from`, and `to` to simplify error messages in the
    // face of intermediate expressions.
    let cast_inner = |from, to, expr| match get_cast(ecx, ccx, from, to) {
        Some(cast) => Ok(cast(expr)),
        None => Err(PlanError::InvalidCast {
            name: ecx.name.into(),
            ccx,
            from: ecx.humanize_scalar_type(from, false),
            to: ecx.humanize_scalar_type(to, false),
        }),
    };

    // Get cast which might include parameter rewrites + generating intermediate
    // expressions.
    //
    // String-like types get special handling to match PostgreSQL.
    // See: https://github.com/postgres/postgres/blob/6b04abdfc/src/backend/parser/parse_coerce.c#L3205-L3223
    let from_category = TypeCategory::from_type(&from);
    let to_category = TypeCategory::from_type(to);
    if from_category == TypeCategory::String && to_category != TypeCategory::String {
        // Converting from stringlike to something non-stringlike. Handle as if
        // `from` were a `ScalarType::String.
        cast_inner(&ScalarType::String, to, expr)
    } else if from_category != TypeCategory::String && to_category == TypeCategory::String {
        // Converting from non-stringlike to something stringlike. Convert to a
        // `ScalarType::String` and then to the desired type.
        let expr = cast_inner(&from, &ScalarType::String, expr)?;
        cast_inner(&ScalarType::String, to, expr)
    } else {
        // Standard cast.
        cast_inner(&from, to, expr)
    }
}

/// Reports whether it is possible to perform a cast from the specified types.
pub fn can_cast(
    ecx: &ExprContext,
    ccx: CastContext,
    cast_from: &ScalarType,
    cast_to: &ScalarType,
) -> bool {
    get_cast(ecx, ccx, cast_from, cast_to).is_some()
}
