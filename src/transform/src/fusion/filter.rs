// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Fuses multiple `Filter` operators into one and canonicalizes predicates.
//!
//! If the `Filter` operator is empty, removes it.
//!
//! ```rust
//! use mz_expr::{MirRelationExpr, MirScalarExpr};
//! use mz_repr::{ColumnType, Datum, RelationType, ScalarType};
//! use mz_repr::optimize::OptimizerFeatures;
//! use mz_transform::{typecheck, Transform, TransformCtx};
//! use mz_transform::dataflow::DataflowMetainfo;
//!
//! use mz_transform::fusion::filter::Filter;
//!
//! let input = MirRelationExpr::constant(vec![], RelationType::new(vec![
//!     ScalarType::Bool.nullable(false),
//! ]));
//!
//! let predicate0 = MirScalarExpr::column(0);
//! let predicate1 = MirScalarExpr::column(0);
//! let predicate2 = MirScalarExpr::column(0);
//!
//! let mut expr = input
//!     .clone()
//!     .filter(vec![predicate0.clone()])
//!     .filter(vec![predicate1.clone()])
//!     .filter(vec![predicate2.clone()]);
//!
//! let features = OptimizerFeatures::default();
//! let typecheck_ctx = typecheck::empty_context();
//! let mut df_meta = DataflowMetainfo::default();
//! let mut transform_ctx = TransformCtx::local(&features, &typecheck_ctx, &mut df_meta, None, None);
//!
//! // Filter.transform() will deduplicate any predicates
//! Filter.transform(&mut expr, &mut transform_ctx);
//!
//! let correct = input.filter(vec![predicate0]);
//!
//! assert_eq!(expr, correct);
//! ```

use mz_expr::MirRelationExpr;

use crate::TransformCtx;

/// Fuses multiple `Filter` operators into one and deduplicates predicates.
#[derive(Debug)]
pub struct Filter;

impl crate::Transform for Filter {
    fn name(&self) -> &'static str {
        "FilterFusion"
    }

    #[mz_ore::instrument(
        target = "optimizer",
        level = "debug",
        fields(path.segment = "filter_fusion")
    )]
    fn actually_perform_transform(
        &self,
        relation: &mut MirRelationExpr,
        _: &mut TransformCtx,
    ) -> Result<(), crate::TransformError> {
        relation.visit_pre_mut(Self::action);
        mz_repr::explain::trace_plan(&*relation);
        Ok(())
    }
}

impl Filter {
    /// Fuses multiple `Filter` operators into one and canonicalizes predicates.
    pub fn action(relation: &mut MirRelationExpr) {
        if let MirRelationExpr::Filter { input, predicates } = relation {
            // consolidate nested filters.
            while let MirRelationExpr::Filter {
                input: inner,
                predicates: p2,
            } = &mut **input
            {
                predicates.append(p2);
                *input = Box::new(inner.take_dangerous());
            }

            mz_expr::canonicalize::canonicalize_predicates(predicates, &input.typ().column_types);

            // remove the Filter stage if empty.
            if predicates.is_empty() {
                *relation = input.take_dangerous();
            }
        }
    }
}
