use crate::client::{InnerClient, Responses};
use crate::codec::FrontendMessage;
use crate::connection::RequestMessages;
use crate::prepare::get_type;
use crate::types::{BorrowToSql, IsNull};
use crate::{Column, Error, Portal, Row, Statement};
use bytes::{Bytes, BytesMut};
use fallible_iterator::FallibleIterator;
use futures_util::{ready, Stream};
use log::{debug, log_enabled, Level};
use pin_project_lite::pin_project;
use postgres_protocol::message::backend::{CommandCompleteBody, Message, RowDescriptionBody};
use postgres_protocol::message::frontend;
use postgres_types::Type;
use std::fmt;
use std::marker::PhantomPinned;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

struct BorrowToSqlParamsDebug<'a, T>(&'a [T]);

impl<'a, T> fmt::Debug for BorrowToSqlParamsDebug<'a, T>
where
    T: BorrowToSql,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|x| x.borrow_to_sql()))
            .finish()
    }
}

pub async fn query<P, I>(
    client: &InnerClient,
    statement: Statement,
    params: I,
) -> Result<RowStream, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let buf = if log_enabled!(Level::Debug) {
        let params = params.into_iter().collect::<Vec<_>>();
        debug!(
            "executing statement {} with parameters: {:?}",
            statement.name(),
            BorrowToSqlParamsDebug(params.as_slice()),
        );
        encode(client, &statement, params)?
    } else {
        encode(client, &statement, params)?
    };
    let responses = start(client, buf).await?;
    Ok(RowStream {
        statement: statement,
        responses,
        rows_affected: None,
        _p: PhantomPinned,
    })
}

enum QueryProcessingState {
    Empty,
    ParseCompleted,
    BindCompleted,
    ParameterDescribed,
    Final(Vec<Column>),
}

/// State machine for processing messages for `query_with_param_types`.
impl QueryProcessingState {
    pub async fn process_message(
        self,
        client: &Arc<InnerClient>,
        message: Message,
    ) -> Result<Self, Error> {
        match (self, message) {
            (QueryProcessingState::Empty, Message::ParseComplete) => {
                Ok(QueryProcessingState::ParseCompleted)
            }
            (QueryProcessingState::ParseCompleted, Message::BindComplete) => {
                Ok(QueryProcessingState::BindCompleted)
            }
            (QueryProcessingState::BindCompleted, Message::ParameterDescription(_)) => {
                Ok(QueryProcessingState::ParameterDescribed)
            }
            (
                QueryProcessingState::ParameterDescribed,
                Message::RowDescription(row_description),
            ) => Self::form_final(client, Some(row_description)).await,
            (QueryProcessingState::ParameterDescribed, Message::NoData) => {
                Self::form_final(client, None).await
            }
            (_, Message::ErrorResponse(body)) => Err(Error::db(body)),
            _ => Err(Error::unexpected_message()),
        }
    }

    async fn form_final(
        client: &Arc<InnerClient>,
        row_description: Option<RowDescriptionBody>,
    ) -> Result<Self, Error> {
        let mut columns = vec![];
        if let Some(row_description) = row_description {
            let mut it = row_description.fields();
            while let Some(field) = it.next().map_err(Error::parse)? {
                let type_ = get_type(client, field.type_oid()).await?;
                let column = Column {
                    name: field.name().to_string(),
                    table_oid: Some(field.table_oid()).filter(|n| *n != 0),
                    column_id: Some(field.column_id()).filter(|n| *n != 0),
                    r#type: type_,
                };
                columns.push(column);
            }
        }

        Ok(Self::Final(columns))
    }
}

pub async fn query_with_param_types<'a, P, I>(
    client: &Arc<InnerClient>,
    query: &str,
    params: I,
) -> Result<RowStream, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = (P, Type)>,
    I::IntoIter: ExactSizeIterator,
{
    let (params, param_types): (Vec<_>, Vec<_>) = params.into_iter().unzip();

    let params = params.into_iter();

    let param_oids = param_types.iter().map(|t| t.oid()).collect::<Vec<_>>();

    let params = params.into_iter();

    let buf = client.with_buf(|buf| {
        frontend::parse("", query, param_oids.into_iter(), buf).map_err(Error::parse)?;

        encode_bind_with_statement_name_and_param_types("", &param_types, params, "", buf)?;

        frontend::describe(b'S', "", buf).map_err(Error::encode)?;

        frontend::execute("", 0, buf).map_err(Error::encode)?;

        frontend::sync(buf);

        Ok(buf.split().freeze())
    })?;

    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    let mut state = QueryProcessingState::Empty;

    loop {
        let message = responses.next().await?;

        state = state.process_message(client, message).await?;

        if let QueryProcessingState::Final(columns) = state {
            return Ok(RowStream {
                statement: Statement::unnamed(vec![], columns),
                responses,
                rows_affected: None,
                _p: PhantomPinned,
            });
        }
    }
}

pub async fn query_portal(
    client: &InnerClient,
    portal: &Portal,
    max_rows: i32,
) -> Result<RowStream, Error> {
    let buf = client.with_buf(|buf| {
        frontend::execute(portal.name(), max_rows, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })?;

    let responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    Ok(RowStream {
        statement: portal.statement().clone(),
        responses,
        rows_affected: None,
        _p: PhantomPinned,
    })
}

/// Extract the number of rows affected from [`CommandCompleteBody`].
pub fn extract_row_affected(body: &CommandCompleteBody) -> Result<u64, Error> {
    let rows = body
        .tag()
        .map_err(Error::parse)?
        .rsplit(' ')
        .next()
        .unwrap()
        .parse()
        .unwrap_or(0);
    Ok(rows)
}

pub async fn execute<P, I>(
    client: &InnerClient,
    statement: Statement,
    params: I,
) -> Result<u64, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let buf = if log_enabled!(Level::Debug) {
        let params = params.into_iter().collect::<Vec<_>>();
        debug!(
            "executing statement {} with parameters: {:?}",
            statement.name(),
            BorrowToSqlParamsDebug(params.as_slice()),
        );
        encode(client, &statement, params)?
    } else {
        encode(client, &statement, params)?
    };
    let mut responses = start(client, buf).await?;

    let mut rows = 0;
    loop {
        match responses.next().await? {
            Message::DataRow(_) => {}
            Message::CommandComplete(body) => {
                rows = extract_row_affected(&body)?;
            }
            Message::EmptyQueryResponse => rows = 0,
            Message::ReadyForQuery(_) => return Ok(rows),
            _ => return Err(Error::unexpected_message()),
        }
    }
}

async fn start(client: &InnerClient, buf: Bytes) -> Result<Responses, Error> {
    let mut responses = client.send(RequestMessages::Single(FrontendMessage::Raw(buf)))?;

    match responses.next().await? {
        Message::BindComplete => {}
        _ => return Err(Error::unexpected_message()),
    }

    Ok(responses)
}

pub fn encode<P, I>(client: &InnerClient, statement: &Statement, params: I) -> Result<Bytes, Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    client.with_buf(|buf| {
        encode_bind(statement, params, "", buf)?;
        frontend::execute("", 0, buf).map_err(Error::encode)?;
        frontend::sync(buf);
        Ok(buf.split().freeze())
    })
}

pub fn encode_bind<P, I>(
    statement: &Statement,
    params: I,
    portal: &str,
    buf: &mut BytesMut,
) -> Result<(), Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    encode_bind_with_statement_name_and_param_types(
        statement.name(),
        statement.params(),
        params,
        portal,
        buf,
    )
}

fn encode_bind_with_statement_name_and_param_types<P, I>(
    statement_name: &str,
    param_types: &[Type],
    params: I,
    portal: &str,
    buf: &mut BytesMut,
) -> Result<(), Error>
where
    P: BorrowToSql,
    I: IntoIterator<Item = P>,
    I::IntoIter: ExactSizeIterator,
{
    let params = params.into_iter();

    if param_types.len() != params.len() {
        return Err(Error::parameters(params.len(), param_types.len()));
    }

    let (param_formats, params): (Vec<_>, Vec<_>) = params
        .zip(param_types.iter())
        .map(|(p, ty)| (p.borrow_to_sql().encode_format(ty) as i16, p))
        .unzip();

    let params = params.into_iter();

    let mut error_idx = 0;
    let r = frontend::bind(
        portal,
        statement_name,
        param_formats,
        params.zip(param_types).enumerate(),
        |(idx, (param, ty)), buf| match param.borrow_to_sql().to_sql_checked(ty, buf) {
            Ok(IsNull::No) => Ok(postgres_protocol::IsNull::No),
            Ok(IsNull::Yes) => Ok(postgres_protocol::IsNull::Yes),
            Err(e) => {
                error_idx = idx;
                Err(e)
            }
        },
        Some(1),
        buf,
    );
    match r {
        Ok(()) => Ok(()),
        Err(frontend::BindError::Conversion(e)) => Err(Error::to_sql(e, error_idx)),
        Err(frontend::BindError::Serialization(e)) => Err(Error::encode(e)),
    }
}

pin_project! {
    /// A stream of table rows.
    pub struct RowStream {
        statement: Statement,
        responses: Responses,
        rows_affected: Option<u64>,
        #[pin]
        _p: PhantomPinned,
    }
}

impl Stream for RowStream {
    type Item = Result<Row, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();
        loop {
            match ready!(this.responses.poll_next(cx)?) {
                Message::DataRow(body) => {
                    return Poll::Ready(Some(Ok(Row::new(this.statement.clone(), body)?)))
                }
                Message::CommandComplete(body) => {
                    *this.rows_affected = Some(extract_row_affected(&body)?);
                }
                Message::EmptyQueryResponse | Message::PortalSuspended => {}
                Message::ReadyForQuery(_) => return Poll::Ready(None),
                _ => return Poll::Ready(Some(Err(Error::unexpected_message()))),
            }
        }
    }
}

impl RowStream {
    /// Returns the number of rows affected by the query.
    ///
    /// This function will return `None` until the stream has been exhausted.
    pub fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }
}
