// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Fuses reduce operators with parent operators if possible.
use mz_expr::{MirRelationExpr, MirScalarExpr};

use crate::{TransformCtx, TransformError};

/// Fuses reduce operators with parent operators if possible.
#[derive(Debug)]
pub struct Reduce;

impl crate::Transform for Reduce {
    fn name(&self) -> &'static str {
        "ReduceFusion"
    }

    #[mz_ore::instrument(
        target = "optimizer",
        level = "debug",
        fields(path.segment = "reduce_fusion")
    )]
    fn actually_perform_transform(
        &self,
        relation: &mut MirRelationExpr,
        _: &mut TransformCtx,
    ) -> Result<(), TransformError> {
        let result = relation.visit_pre_mut(|e| self.action(e));
        mz_repr::explain::trace_plan(&*relation);
        Ok(result)
    }
}

impl Reduce {
    /// Fuses reduce operators with parent operators if possible.
    pub fn action(&self, relation: &mut MirRelationExpr) {
        if let MirRelationExpr::Reduce {
            input,
            group_key,
            aggregates,
            monotonic: _,
            expected_group_size: _,
        } = relation
        {
            if let MirRelationExpr::Reduce {
                input: inner_input,
                group_key: inner_group_key,
                aggregates: inner_aggregates,
                monotonic: _,
                expected_group_size: _,
            } = &mut **input
            {
                // Collect all columns referenced by outer
                let mut outer_cols = vec![];
                for expr in group_key.iter() {
                    expr.visit_pre(|e| {
                        if let MirScalarExpr::Column(i, _) = e {
                            outer_cols.push(*i);
                        }
                    });
                }

                // We can fuse reduce operators as long as the outer one doesn't
                // group by an aggregation performed by the inner one.
                if outer_cols.iter().any(|c| *c >= inner_group_key.len()) {
                    return;
                }

                if aggregates.is_empty() && inner_aggregates.is_empty() {
                    // Replace inner reduce with map + project (no grouping)
                    let mut outputs = vec![];
                    let mut scalars = vec![];

                    let arity = inner_input.arity();
                    for e in inner_group_key {
                        if let MirScalarExpr::Column(i, _) = e {
                            outputs.push(*i);
                        } else {
                            outputs.push(arity + scalars.len());
                            scalars.push(e.clone());
                        }
                    }

                    **input = inner_input.take_dangerous().map(scalars).project(outputs);
                }
            }
        }
    }
}
