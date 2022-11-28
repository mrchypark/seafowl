use datafusion::common::DataFusionError;
use datafusion::error::Result;
use datafusion::scalar::ScalarValue;
use datafusion_expr::expr_visitor::{ExpressionVisitor, Recursion};
use datafusion_expr::{BinaryExpr, Expr, Operator};

pub struct FilterPushdown<T: FilterPushdownVisitor> {
    pub source: T,
    pub pushdown_supported: bool,
    // LIFO stack for keeping the intermediate SQL expression results to be used in interpolation
    // of the parent nodes. After a successful visit, it should contain exactly one element, which
    // represents the complete SQL statement corresponding to the given expression.
    pub sql_exprs: Vec<String>,
}

pub struct PostgresFilterPushdown {}
pub struct SQLiteFilterPushdown {}

impl FilterPushdownVisitor for PostgresFilterPushdown {}
impl FilterPushdownVisitor for SQLiteFilterPushdown {}

pub trait FilterPushdownVisitor {
    fn scalar_value_to_sql(&self, value: &ScalarValue) -> Option<String> {
        match value {
            ScalarValue::Utf8(Some(val)) => Some(format!("'{}'", val)),
            _ => Some(format!("{}", value)),
        }
    }

    fn op_to_sql(&self, op: Operator) -> Option<String> {
        Some(op.to_string())
    }
}

impl<T: FilterPushdownVisitor> ExpressionVisitor for FilterPushdown<T> {
    fn pre_visit(mut self, expr: &Expr) -> Result<Recursion<Self>> {
        match expr {
            Expr::Column(_) | Expr::Literal(_) => {}
            Expr::BinaryExpr(BinaryExpr { op, .. }) => {
                // Check if operator pushdown supported; left and right expressions will be checked
                // through further recursion.
                if self.source.op_to_sql(*op).is_none() {
                    return Ok(Recursion::Stop(self));
                }
            }
            _ => {
                // Expression is not supported, no need to visit any remaining nodes
                self.pushdown_supported = false;
                return Ok(Recursion::Stop(self));
            }
        };
        Ok(Recursion::Continue(self))
    }

    fn post_visit(mut self, expr: &Expr) -> Result<Self> {
        match expr {
            Expr::Column(col) => self.sql_exprs.push(col.name.clone()),
            Expr::Literal(val) => {
                let sql_val = self.source.scalar_value_to_sql(val).ok_or_else(|| {
                    DataFusionError::Execution(format!(
                        "Couldn't convert ScalarValue {:?} to a compatible one for the remote system",
                        val,
                    ))
                })?;
                self.sql_exprs.push(sql_val)
            }
            Expr::BinaryExpr(be @ BinaryExpr { .. }) => {
                // The visitor has been through left and right sides in that order, so the topmost
                // item on the SQL expression stack is the right expression
                let mut right_sql = self.sql_exprs.pop().unwrap_or_else(|| {
                    panic!("Missing right sub-expression of {}", expr)
                });
                let mut left_sql = self
                    .sql_exprs
                    .pop()
                    .unwrap_or_else(|| panic!("Missing left sub-expression of {}", expr));

                // Similar as in Display impl for BinaryExpr: since the Expr has an implicit operator
                // precedence we need to convert it to an explicit one using extra parenthesis if the
                // left/right expression is also a BinaryExpr of lower operator precedence.
                if let Expr::BinaryExpr(right_be @ BinaryExpr { .. }) = &*be.right {
                    let p = right_be.precedence();
                    if p == 0 || p < be.precedence() {
                        right_sql = format!("({})", right_sql)
                    }
                }
                if let Expr::BinaryExpr(left_be @ BinaryExpr { .. }) = &*be.left {
                    let p = left_be.precedence();
                    if p == 0 || p < be.precedence() {
                        left_sql = format!("({})", left_sql)
                    }
                }

                let op = self.source.op_to_sql(be.op).ok_or_else(|| {
                    DataFusionError::Execution(format!(
                        "Couldn't convert operator {:?} to a compatible one for the remote system",
                        be.op,
                    ))
                })?;

                self.sql_exprs
                    .push(format!("{} {} {}", left_sql, op, right_sql))
            }
            _ => {}
        };
        Ok(self)
    }
}
