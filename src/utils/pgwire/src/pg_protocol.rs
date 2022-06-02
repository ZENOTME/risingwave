// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Versio&n 2.0 (the "License");
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

use std::collections::HashMap;
use std::io::{Error as IoError, ErrorKind, Result};
use std::str;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::error::PsqlError;
use crate::pg_extended::{pg_portal, pg_statement};
use crate::pg_field_descriptor::{PgFieldDescriptor, TypeOid};
use crate::pg_message::{
    BeCommandCompleteMessage, BeMessage, BeParameterStatusMessage, FeMessage, FeStartupMessage,
};
use crate::pg_response::PgResponse;
use crate::pg_server::{Session, SessionManager};

/// The state machine for each psql connection.
/// Read pg messages from tcp stream and write results back.
pub struct PgProtocol<S, SM>
where
    SM: SessionManager,
{
    /// Used for write/read message in tcp connection.
    stream: S,
    /// Write into buffer before flush to stream.
    buf_out: BytesMut,
    /// Current states of pg connection.
    state: PgProtocolState,
    /// Whether the connection is terminated.
    is_terminate: bool,

    session_mgr: Arc<SM>,
    session: Option<Arc<SM::Session>>,
}

/// States flow happened from top to down.
enum PgProtocolState {
    Startup,
    Regular,
}

// Truncate 0 from C string in Bytes and stringify it (returns slice, no allocations)
// PG protocol strings are always C strings.
fn cstr_to_str(b: &Bytes) -> Result<&str> {
    let without_null = if b.last() == Some(&0) {
        &b[..b.len() - 1]
    } else {
        &b[..]
    };
    std::str::from_utf8(without_null).map_err(|e| std::io::Error::new(ErrorKind::Other, e))
}

impl<S, SM> PgProtocol<S, SM>
where
    S: AsyncWrite + AsyncRead + Unpin,
    SM: SessionManager,
{
    pub fn new(stream: S, session_mgr: Arc<SM>) -> Self {
        Self {
            stream,
            is_terminate: false,
            state: PgProtocolState::Startup,
            buf_out: BytesMut::with_capacity(10 * 1024),
            session_mgr,
            session: None,
        }
    }

    pub async fn process(
        &mut self,
        unnamed_statement: &mut pg_statement,
        unnamed_portal: &mut pg_portal,
        named_statements: &mut HashMap<String, pg_statement>,
        named_portals: &mut HashMap<String, pg_portal>,
    ) -> Result<bool> {
        if self
            .do_process(
                unnamed_statement,
                unnamed_portal,
                named_statements,
                named_portals,
            )
            .await?
        {
            return Ok(true);
        }

        Ok(self.is_terminate())
    }

    async fn do_process(
        &mut self,
        unnamed_statement: &mut pg_statement,
        unnamed_portal: &mut pg_portal,
        named_statements: &mut HashMap<String, pg_statement>,
        named_portals: &mut HashMap<String, pg_portal>,
    ) -> Result<bool> {
        let msg = match self.read_message().await {
            Ok(msg) => msg,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    return Err(e);
                }
                tracing::error!("unable to read message: {}", e);
                self.write_message_no_flush(&BeMessage::ErrorResponse(Box::new(e)))?;
                self.write_message_no_flush(&BeMessage::ReadyForQuery)?;
                return Ok(false);
            }
        };
        match msg {
            FeMessage::Ssl => {
                self.write_message_no_flush(&BeMessage::EncryptionResponse)
                    .map_err(|e| {
                        tracing::error!("failed to handle ssl request: {}", e);
                        e
                    })?;
            }
            FeMessage::Startup(msg) => {
                self.process_startup_msg(msg).map_err(|e| {
                    tracing::error!("failed to set up pg session: {}", e);
                    e
                })?;
                self.state = PgProtocolState::Regular;
            }
            FeMessage::Query(query_msg) => {
                self.process_query_msg(query_msg.get_sql(), false).await?;
                self.write_message_no_flush(&BeMessage::ReadyForQuery)?;
            }
            FeMessage::CancelQuery => {
                self.write_message_no_flush(&BeMessage::ErrorResponse(Box::new(
                    PsqlError::cancel(),
                )))?;
            }
            FeMessage::Terminate => {
                self.process_terminate();
            }
            FeMessage::Parse(m) => {
                // Step 1: Create the types description
                let type_ids = m.type_ids;
                let mut types = Vec::new();
                for i in type_ids.into_iter() {
                    types.push(TypeOid::as_type(i).unwrap());
                }
                // Step 2: Create the row description
                let mut rows = Vec::new();
                for i in types.iter() {
                    let row = PgFieldDescriptor::new(String::new(), i.to_owned());
                    rows.push(row);
                }
                // Step 3: Create the statement
                let statement = pg_statement::new(
                    cstr_to_str(&m.statement_name).unwrap().to_string(),
                    m.query_string,
                    types,
                    rows,
                );
                // Step 4: Insert the statement
                let name = statement.get_name();
                if name.is_empty() {
                    *unnamed_statement = statement;
                } else {
                    named_statements.insert(name, statement);
                }
                // println!("{}", cstr_to_str(&unnamed_query_string).unwrap());
                self.write_message(&BeMessage::ParseComplete).await?;
            }
            FeMessage::Bind(m) => {
                let statement_name = cstr_to_str(&m.statement_name).unwrap().to_string();
                // Step 1 Get statement
                let statement = if statement_name.is_empty() {
                    unnamed_statement
                } else {
                    // NOTE Error handle method may need to modified
                    named_statements.get(&statement_name).unwrap()
                };
                // Step 2 instance
                let portal_name = cstr_to_str(&m.portal_name).unwrap().to_string();
                let portal = statement.instance(portal_name.clone(), &m.params);
                // Step 3 Store Portal
                if portal_name.is_empty() {
                    *unnamed_portal = portal;
                } else {
                    named_portals.insert(portal_name, portal);
                }
                self.write_message(&BeMessage::BindComplete).await?;
            }
            FeMessage::Execute(m) => {
                // Step 1 Get portal
                let portal_name = cstr_to_str(&m.portal_name).unwrap().to_string();
                let portal = if m.portal_name.is_empty() {
                    unnamed_portal
                } else {
                    // NOTE: error handle need modify later;
                    named_portals.get(&portal_name).unwrap()
                };
                // Step 2 Execute instance statement using portal
                self.process_query_msg(cstr_to_str(&portal.get_query_string()), true)
                    .await?;
                // NOTE there is no ReadyForQuery message.
            }
            FeMessage::Describe(m) => {
                // FIXME: Introduce parser to analyze statements and bind data type. Here just
                // hard-code a VARCHAR.
                // Step 1 Get statement
                let name = cstr_to_str(&m.query_name).unwrap().to_string();
                let statement = if name.is_empty() {
                    unnamed_statement
                } else {
                    // NOTE: error handle need modify later;
                    named_statements.get(&name).unwrap()
                };
                // Step 2 Send parameter description
                self.write_message(&BeMessage::ParameterDescription(&statement.get_type_desc()))
                    .await?;
                // Step 3 Send row description
                self.write_message(&BeMessage::RowDescription(&statement.get_row_desc()))
                    .await?;
            }
            FeMessage::Sync => {
                self.write_message(&BeMessage::ReadyForQuery).await?;
            }
            FeMessage::Close(m) => {
                let name = cstr_to_str(&m.query_name).unwrap().to_string();
                if m.kind == b'S' {
                    named_statements.remove_entry(&name);
                } else if m.kind == b'P' {
                    named_portals.remove_entry(&name);
                } else {
                    // NOTE: error handle need modify later;
                }
                self.write_message(&BeMessage::CloseComplete).await?;
            }
        }
        self.flush().await?;
        Ok(false)
    }

    async fn read_message(&mut self) -> Result<FeMessage> {
        match self.state {
            PgProtocolState::Startup => FeStartupMessage::read(&mut self.stream).await,
            PgProtocolState::Regular => FeMessage::read(&mut self.stream).await,
        }
    }

    fn process_startup_msg(&mut self, _msg: FeStartupMessage) -> Result<()> {
        // TODO: Replace `DEFAULT_DATABASE_NAME` with true database name in `FeStartupMessage`.
        self.session = Some(self.session_mgr.connect("dev").map_err(IoError::other)?);
        self.write_message_no_flush(&BeMessage::AuthenticationOk)?;
        self.write_message_no_flush(&BeMessage::ParameterStatus(
            BeParameterStatusMessage::ClientEncoding("utf8"),
        ))?;
        self.write_message_no_flush(&BeMessage::ParameterStatus(
            BeParameterStatusMessage::StandardConformingString("on"),
        ))?;
        self.write_message_no_flush(&BeMessage::ParameterStatus(
            BeParameterStatusMessage::ServerVersion("9.5.0"),
        ))?;
        self.write_message_no_flush(&BeMessage::ReadyForQuery)?;
        Ok(())
    }

    fn process_terminate(&mut self) {
        self.is_terminate = true;
    }

    async fn process_query_msg(
        &mut self,
        query_string: Result<&str>,
        extended: bool,
    ) -> Result<()> {
        match query_string {
            Ok(sql) => {
                tracing::trace!("receive query: {}", sql);
                let session = self.session.clone().unwrap();
                // execute query
                let process_res = session.run_statement(sql).await;
                match process_res {
                    Ok(res) => {
                        if res.is_empty() {
                            self.write_message_no_flush(&BeMessage::EmptyQueryResponse)?;
                        } else if res.is_query() {
                            self.process_query_with_results(res, extended).await?;
                        } else {
                            self.write_message_no_flush(&BeMessage::CommandComplete(
                                BeCommandCompleteMessage {
                                    stmt_type: res.get_stmt_type(),
                                    notice: res.get_notice(),
                                    rows_cnt: res.get_effected_rows_cnt(),
                                },
                            ))?;
                        }
                    }
                    Err(e) => {
                        self.write_message_no_flush(&BeMessage::ErrorResponse(e))?;
                    }
                }
            }
            Err(err) => {
                self.write_message_no_flush(&BeMessage::ErrorResponse(Box::new(err)))?;
            }
        };

        Ok(())
    }

    async fn process_query_with_results(&mut self, res: PgResponse, extended: bool) -> Result<()> {
        // The possible responses to Execute are the same as those described above for queries
        // issued via simple query protocol, except that Execute doesn't cause ReadyForQuery or
        // RowDescription to be issued.
        // Quoted from: https://www.postgresql.org/docs/current/protocol-flow.html#PROTOCOL-FLOW-EXT-QUERY
        if !extended {
            self.write_message(&BeMessage::RowDescription(&res.get_row_desc()))
                .await?;
        }

        let mut rows_cnt = 0;
        let iter = res.iter();
        for val in iter {
            self.write_message(&BeMessage::DataRow(val)).await?;
            rows_cnt += 1;
        }
        self.write_message_no_flush(&BeMessage::CommandComplete(BeCommandCompleteMessage {
            stmt_type: res.get_stmt_type(),
            notice: res.get_notice(),
            rows_cnt,
        }))?;
        Ok(())
    }

    fn is_terminate(&self) -> bool {
        self.is_terminate
    }

    fn write_message_no_flush(&mut self, message: &BeMessage<'_>) -> Result<()> {
        BeMessage::write(&mut self.buf_out, message)
    }

    async fn write_message(&mut self, message: &BeMessage<'_>) -> Result<()> {
        self.write_message_no_flush(message)?;
        self.flush().await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.stream.write_all(&self.buf_out).await?;
        self.buf_out.clear();
        self.stream.flush().await?;
        Ok(())
    }
}
