use std::fmt;
use std::mem;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::Context;

use futures::{ready, Future, Poll, Stream};
use http::{response, HeaderMap, Request, Response};
use quinn::{EndpointBuilder, EndpointDriver, EndpointError, RecvStream, SendStream};
use quinn_proto::{Side, StreamId};

use crate::{
    body::{Body, BodyWriter, RecvBody},
    connection::{ConnectionDriver, ConnectionRef},
    frame::{FrameDecoder, FrameStream, WriteFrame},
    headers::{DecodeHeaders, SendHeaders},
    proto::{
        frame::{DataFrame, HttpFrame},
        headers::Header,
    },
    try_take, Error, Settings,
};

pub struct Builder {
    endpoint: EndpointBuilder,
    settings: Settings,
}

impl Builder {
    pub fn new(endpoint: EndpointBuilder) -> Self {
        Self {
            endpoint,
            settings: Settings::default(),
        }
    }

    pub fn settings(&mut self, settings: Settings) -> &mut Self {
        self.settings = settings;
        self
    }

    pub fn bind(
        self,
        addr: &SocketAddr,
    ) -> Result<(EndpointDriver, Server, IncomingConnection), EndpointError> {
        let (endpoint_driver, _endpoint, incoming) = self.endpoint.bind(addr)?;
        Ok((
            endpoint_driver,
            Server,
            IncomingConnection {
                incoming,
                settings: self.settings.clone(),
            },
        ))
    }
}

pub struct Server;

pub struct IncomingConnection {
    incoming: quinn::Incoming,
    settings: Settings,
}

impl Stream for IncomingConnection {
    type Item = Connecting;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        Poll::Ready(
            ready!(Pin::new(&mut self.incoming).poll_next(cx)).map(|c| Connecting {
                connecting: c,
                settings: self.settings.clone(),
            }),
        )
    }
}

pub struct Connecting {
    connecting: quinn::Connecting,
    settings: Settings,
}

impl Future for Connecting {
    type Output = Result<(quinn::ConnectionDriver, ConnectionDriver, IncomingRequest), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let quinn::NewConnection {
            driver,
            connection,
            bi_streams,
            uni_streams,
            ..
        } = ready!(Pin::new(&mut self.connecting).poll(cx))?;
        let conn_ref = ConnectionRef::new(
            connection,
            self.settings.clone(),
            uni_streams,
            bi_streams,
            Side::Server,
        )?;
        Poll::Ready(Ok((
            driver,
            ConnectionDriver(conn_ref.clone()),
            IncomingRequest(conn_ref),
        )))
    }
}

pub struct IncomingRequest(ConnectionRef);

impl Stream for IncomingRequest {
    type Item = RecvRequest;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let (send, recv) = {
            let conn = &mut self.0.h3.lock().unwrap();
            match conn.requests.pop_front() {
                Some(s) => s,
                None => {
                    conn.requests_waker = Some(cx.waker().clone());
                    return Poll::Pending;
                }
            }
        };
        Poll::Ready(Some(RecvRequest::new(recv, send, self.0.clone())))
    }
}

enum RecvRequestState {
    Receiving(FrameStream, SendStream),
    Decoding(DecodeHeaders),
    Ready,
}

pub struct RecvRequest {
    state: RecvRequestState,
    conn: ConnectionRef,
    stream_id: StreamId,
    streams: Option<(FrameStream, SendStream)>,
}

impl RecvRequest {
    fn new(recv: RecvStream, send: SendStream, conn: ConnectionRef) -> Self {
        Self {
            conn,
            stream_id: send.id(),
            streams: None,
            state: RecvRequestState::Receiving(FrameDecoder::stream(recv), send),
        }
    }

    fn build_request(
        &self,
        headers: Header,
        recv: FrameStream,
    ) -> Result<Request<RecvBody>, Error> {
        let (method, uri, headers) = headers.into_request_parts()?;
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .version(http::version::Version::HTTP_3)
            .body(RecvBody::new(recv, self.conn.clone(), self.stream_id))
            .unwrap();
        *request.headers_mut() = headers;
        Ok(request)
    }
}

impl Future for RecvRequest {
    type Output = Result<(Request<RecvBody>, Sender), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match self.state {
                RecvRequestState::Receiving(ref mut frames, _) => {
                    match ready!(Pin::new(frames).poll_next(cx)) {
                        Some(Err(e)) => return Poll::Ready(Err(e.into())),
                        None => return Poll::Ready(Err(Error::peer("received an empty request"))),
                        Some(Ok(HttpFrame::Headers(f))) => {
                            let decode = DecodeHeaders::new(f, self.conn.clone(), self.stream_id);
                            match mem::replace(&mut self.state, RecvRequestState::Decoding(decode))
                            {
                                RecvRequestState::Receiving(f, s) => self.streams = Some((f, s)),
                                _ => unreachable!("Invalid state"),
                            }
                        }
                        Some(Ok(_)) => {
                            return Poll::Ready(Err(Error::peer("first frame is not headers")))
                        }
                    }
                }
                RecvRequestState::Decoding(ref mut decode) => {
                    let header = ready!(Pin::new(decode).poll(cx))?;
                    self.state = RecvRequestState::Ready;
                    let (recv, send) = try_take(&mut self.streams, "Recv request invalid state")?;
                    return Poll::Ready(Ok((
                        self.build_request(header, recv)?,
                        Sender {
                            send,
                            stream_id: self.stream_id,
                            conn: self.conn.clone(),
                        },
                    )));
                }
                RecvRequestState::Ready => {
                    return Poll::Ready(Err(Error::peer("polled after ready")));
                }
            };
        }
    }
}

pub struct Sender {
    send: SendStream,
    conn: ConnectionRef,
    stream_id: StreamId,
}

impl Sender {
    pub fn response<T>(self, response: Response<T>) -> ResonsepBuilder<T> {
        ResonsepBuilder {
            response,
            sender: self,
            trailers: None,
        }
    }
}

pub struct ResonsepBuilder<T> {
    sender: Sender,
    response: Response<T>,
    trailers: Option<HeaderMap>,
}

impl<T> ResonsepBuilder<T>
where
    T: Into<Body>,
{
    pub fn trailers(mut self, trailers: HeaderMap) -> Self {
        self.trailers = Some(trailers);
        self
    }

    pub async fn send(self) -> Result<(), Error> {
        let Sender {
            send,
            stream_id,
            conn,
        } = self.sender;
        SendResponse::new(self.response, self.trailers, send, stream_id, conn)?.await?;
        Ok(())
    }

    pub async fn stream(self) -> Result<BodyWriter, Error> {
        let Sender {
            send,
            stream_id,
            conn,
        } = self.sender;

        let (
            response::Parts {
                status, headers, ..
            },
            body,
        ) = self.response.into_parts();

        let trailers = self.trailers;

        let send =
            SendHeaders::new(Header::response(status, headers), &conn, send, stream_id)?.await?;
        let send = match body.into() {
            Body::None => send,
            Body::Buf(payload) => WriteFrame::new(send, DataFrame { payload }).await?,
        };
        Ok(BodyWriter::new(send, conn, stream_id, trailers))
    }
}

enum SendResponseState {
    SendingHeader(SendHeaders),
    SendingBody(WriteFrame),
    SendingTrailers(SendHeaders),
    Finished,
}

impl fmt::Display for SendResponseState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendResponseState::SendingHeader(_) => write!(f, "SendResponseState::SendingHeader")?,
            SendResponseState::SendingBody(_) => write!(f, "SendResponseState::SendingBody")?,
            SendResponseState::SendingTrailers(_) => {
                write!(f, "SendResponseState::SendingTrailers")?
            }
            Finished => write!(f, "SendResponseState::Finished")?,
        }
        Ok(())
    }
}

pub struct SendResponse {
    state: SendResponseState,
    body: Option<Body>,
    trailer: Option<Header>,
    conn: ConnectionRef,
    stream_id: StreamId,
}

impl SendResponse {
    fn new<T: Into<Body>>(
        response: Response<T>,
        trailers: Option<HeaderMap>,
        send: SendStream,
        stream_id: StreamId,
        conn: ConnectionRef,
    ) -> Result<Self, Error> {
        let (
            response::Parts {
                status, headers, ..
            },
            body,
        ) = response.into_parts();

        let headers = Header::response(status, headers);
        let state =
            SendResponseState::SendingHeader(SendHeaders::new(headers, &conn, send, stream_id)?);

        Ok(Self {
            conn,
            state,
            stream_id,
            body: Some(body.into()),
            trailer: trailers.map(Header::trailer),
        })
    }
}

impl Future for SendResponse {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            println!("{}", self.state);
            match self.state {
                SendResponseState::Finished => panic!("polled after finished"),
                SendResponseState::SendingTrailers(ref mut write) => {
                    ready!(Pin::new(write).poll(cx))?; // drop send
                    self.state = SendResponseState::Finished;
                    return Poll::Ready(Ok(()));
                }
                SendResponseState::SendingHeader(ref mut write) => {
                    let send = ready!(Pin::new(write).poll(cx))?;
                    println!("SendResponse: header sent");
                    match self.body.take() {
                        Some(Body::Buf(payload)) => {
                            self.state = SendResponseState::SendingBody(WriteFrame::new(
                                send,
                                DataFrame { payload },
                            ));
                        }
                        _ => {
                            self.state = SendResponseState::Finished;
                            println!("{}", self.state);
                            return Poll::Ready(Ok(()));
                        }
                    };
                }
                SendResponseState::SendingBody(ref mut body) => {
                    let send = ready!(Pin::new(body).poll(cx))?;
                    match self.trailer.take() {
                        None => {
                            self.state = SendResponseState::Finished;
                            return Poll::Ready(Ok(()));
                        }
                        Some(trailer) => {
                            self.state = SendResponseState::SendingTrailers(SendHeaders::new(
                                trailer,
                                &self.conn,
                                send,
                                self.stream_id,
                            )?);
                        }
                    };
                }
            }
        }
    }
}
