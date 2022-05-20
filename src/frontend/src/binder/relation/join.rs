// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use risingwave_common::error::{ErrorCode, Result};
use risingwave_common::types::DataType;
use risingwave_pb::plan_common::JoinType;
use risingwave_sqlparser::ast::{
    BinaryOperator, Expr, Ident, JoinConstraint, JoinOperator, TableFactor, TableWithJoins,
};

use crate::binder::{Binder, Relation};
use crate::expr::{Expr as _, ExprImpl};

#[derive(Debug)]
pub struct BoundJoin {
    pub join_type: JoinType,
    pub left: Relation,
    pub right: Relation,
    pub cond: ExprImpl,
}

impl Binder {
    pub(crate) fn bind_vec_table_with_joins(
        &mut self,
        from: Vec<TableWithJoins>,
    ) -> Result<Option<Relation>> {
        let mut from_iter = from.into_iter();
        let first = match from_iter.next() {
            Some(t) => t,
            None => return Ok(None),
        };
        let mut root = self.bind_table_with_joins(first)?;
        for t in from_iter {
            let right = self.bind_table_with_joins(t)?;
            root = Relation::Join(Box::new(BoundJoin {
                join_type: JoinType::Inner,
                left: root,
                right,
                cond: ExprImpl::literal_bool(true),
            }));
        }
        Ok(Some(root))
    }

    fn bind_table_with_joins(&mut self, table: TableWithJoins) -> Result<Relation> {
        let root_table_name = get_table_name(&table.relation);
        let mut root = self.bind_table_factor(table.relation)?;
        for join in table.joins {
            let right_table_name = get_table_name(&join.relation);
            let right = self.bind_table_factor(join.relation)?;
            let (constraint, join_type) = match join.join_operator {
                JoinOperator::Inner(constraint) => (constraint, JoinType::Inner),
                JoinOperator::LeftOuter(constraint) => (constraint, JoinType::LeftOuter),
                JoinOperator::RightOuter(constraint) => (constraint, JoinType::RightOuter),
                JoinOperator::FullOuter(constraint) => (constraint, JoinType::FullOuter),
                // Cross join equals to inner join with with no constraint.
                JoinOperator::CrossJoin => (JoinConstraint::None, JoinType::Inner),
            };
            let cond =
                self.bind_join_constraint(constraint, &root_table_name, &right_table_name)?;
            let join = BoundJoin {
                join_type,
                left: root,
                right,
                cond,
            };
            root = Relation::Join(Box::new(join));
        }

        Ok(root)
    }

    fn bind_join_constraint(
        &mut self,
        constraint: JoinConstraint,
        left_table: &Option<Ident>,
        right_table: &Option<Ident>,
    ) -> Result<ExprImpl> {
        Ok(match constraint {
            JoinConstraint::None => ExprImpl::literal_bool(true),
            JoinConstraint::Natural => {
                return Err(ErrorCode::NotImplemented("Natural join".into(), 1633.into()).into())
            }
            JoinConstraint::On(expr) => {
                let bound_expr = self.bind_expr(expr)?;
                if bound_expr.return_type() != DataType::Boolean {
                    return Err(ErrorCode::InternalError(format!(
                        "argument of ON must be boolean, not type {:?}",
                        bound_expr.return_type()
                    ))
                    .into());
                }
                bound_expr
            }
            JoinConstraint::Using(columns) => {
                let mut columns_iter = columns.into_iter();
                let first_column = columns_iter.next().unwrap();
                let mut binary_expr = Expr::BinaryOp {
                    left: Box::new(Expr::CompoundIdentifier(vec![
                        left_table.clone().unwrap(),
                        first_column.clone(),
                    ])),
                    op: BinaryOperator::Eq,
                    right: Box::new(Expr::CompoundIdentifier(vec![
                        right_table.clone().unwrap(),
                        first_column,
                    ])),
                };
                for column in columns_iter {
                    binary_expr = Expr::BinaryOp {
                        left: Box::new(binary_expr),
                        op: BinaryOperator::Eq,
                        right: Box::new(Expr::BinaryOp {
                            left: Box::new(Expr::CompoundIdentifier(vec![
                                left_table.clone().unwrap(),
                                column.clone(),
                            ])),
                            op: BinaryOperator::Eq,
                            right: Box::new(Expr::CompoundIdentifier(vec![
                                right_table.clone().unwrap(),
                                column.clone(),
                            ])),
                        }),
                    }
                }
                self.bind_expr(binary_expr)?
            }
        })
    }
}

fn get_table_name(table_factor: &TableFactor) -> Option<Ident> {
    match table_factor {
        TableFactor::Table {
            name,
            alias: _,
            args: _,
        } => Some(name.0[0].clone()),
        TableFactor::Derived {
            lateral: _,
            subquery: _,
            alias,
        } => alias.as_ref().map(|table_alias| table_alias.name.clone()),
        TableFactor::TableFunction { expr: _, alias } => {
            alias.as_ref().map(|table_alias| table_alias.name.clone())
        }
        TableFactor::NestedJoin(table_with_joins) => get_table_name(&table_with_joins.relation),
    }
}
