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

use futures_async_stream::for_await;
use pgwire::pg_response::{PgResponse, StatementType};
use risingwave_common::error::Result;
use risingwave_sqlparser::ast::Statement;

use crate::binder::{Binder, BoundStatement};
use crate::handler::privilege::{check_privileges, resolve_privileges};
use crate::handler::util::{to_pg_field, to_pg_rows};
use crate::planner::Planner;
use crate::scheduler::{ExecutionContext, ExecutionContextRef};
use crate::session::{OptimizerContext, SessionImpl};

pub async fn handle_dml(context: OptimizerContext, stmt: Statement) -> Result<PgResponse> {
    let stmt_type = to_statement_type(&stmt);
    let session = context.session_ctx.clone();

    let bound = {
        let mut binder = Binder::new(&session);
        binder.bind(stmt)?
    };

    let check_items = resolve_privileges(&bound);
    check_privileges(&session, &check_items)?;

    let associated_mview_id = match &bound {
        BoundStatement::Insert(insert) => insert.table_source.associated_mview_id,
        BoundStatement::Update(update) => update.table_source.associated_mview_id,
        BoundStatement::Delete(delete) => delete.table_source.associated_mview_id,
        BoundStatement::Query(_) => unreachable!(),
    };

    let vnodes = context
        .session_ctx
        .env()
        .worker_node_manager()
        .get_table_mapping(&associated_mview_id);

    let (plan, pg_descs) = {
        // Subblock to make sure PlanRef (an Rc) is dropped before `await` below.
        let root = Planner::new(context.into()).plan(bound)?;
        let pg_descs = root.schema().fields().iter().map(to_pg_field).collect();
        let plan = root.gen_batch_query_plan()?;

        (plan.to_batch_prost(), pg_descs)
    };

    let execution_context: ExecutionContextRef = ExecutionContext::new(session.clone()).into();
    let query_manager = execution_context.session().env().query_manager().clone();

    let mut rows = vec![];
    #[for_await]
    for chunk in query_manager
        .schedule_single(execution_context, plan, vnodes)
        .await?
    {
        rows.extend(to_pg_rows(chunk?, false));
    }

    let rows_count = match stmt_type {
        // TODO(renjie): We need a better solution for this.
        StatementType::INSERT | StatementType::DELETE | StatementType::UPDATE => {
            let first_row = rows[0].values();
            let affected_rows_str = first_row[0]
                .as_ref()
                .expect("compute node should return affected rows in output");
            String::from_utf8(affected_rows_str.to_vec())
                .unwrap()
                .parse()
                .unwrap_or_default()
        }

        _ => unreachable!(),
    };

    // Implicitly flush the writes.
    if session.config().get_implicit_flush() {
        flush_for_write(&session, stmt_type).await?;
    }

    Ok(PgResponse::new(stmt_type, rows_count, rows, pg_descs, true))
}

async fn flush_for_write(session: &SessionImpl, stmt_type: StatementType) -> Result<()> {
    match stmt_type {
        StatementType::INSERT | StatementType::DELETE | StatementType::UPDATE => {
            let client = session.env().meta_client();
            let max_committed_epoch = client.flush().await?;
            session
                .env()
                .hummock_snapshot_manager()
                .update_epoch(max_committed_epoch);
        }
        _ => {}
    }
    Ok(())
}

fn to_statement_type(stmt: &Statement) -> StatementType {
    use StatementType::*;

    match stmt {
        Statement::Insert { .. } => INSERT,
        Statement::Delete { .. } => DELETE,
        Statement::Update { .. } => UPDATE,
        _ => unreachable!(),
    }
}
