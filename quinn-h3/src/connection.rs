use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::Cursor;
use std::mem;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

use bytes::BytesMut;
use futures::{AsyncRead, Future, Poll, Stream};
use quinn::{IncomingBiStreams, IncomingUniStreams, RecvStream, SendStream};
use quinn_proto::{Side, StreamId};

use crate::{
    frame::FrameStream,
    proto::{
        connection::{Connection, DecodeResult, Error as ProtoError},
        frame::{HeadersFrame, HttpFrame},
        headers::Header,
        StreamType,
    },
    streams::{NewUni, RecvUni, SendUni},
    Error, ErrorCode, Settings,
};

pub struct ConnectionDriver(pub(super) ConnectionRef);

impl Future for ConnectionDriver {
    type Output = Result<(), Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut conn = self.0.h3.lock().unwrap();
        println!("{:?} driving", conn.side);
        conn.poll_send_uni(cx)?;
        conn.poll_incoming_uni(cx)?;
        conn.poll_control(cx);
        conn.poll_encoder(cx)?;
        conn.poll_decoder(cx)?;
        conn.poll_incoming_bi(cx)?;

        if conn.driver_waker.is_none() {
            conn.driver_waker = Some(cx.waker().clone())
        }
        Poll::Pending
    }
}

pub(crate) struct ConnectionInner {
    pub inner: Connection,
    pub quic: quinn::Connection,
    pub requests: VecDeque<(SendStream, RecvStream)>,
    pub requests_waker: Option<Waker>,
    driver_waker: Option<Waker>,
    incoming_bi: IncomingBiStreams,
    incoming_uni: IncomingUniStreams,
    pending_uni: VecDeque<Option<RecvUni>>,
    control: Option<FrameStream>, // TODO rename recv_control
    recv_encoder: Option<(RecvStream, BytesMut)>,
    recv_decoder: Option<(RecvStream, BytesMut)>,
    send_control: SendUni,
    send_encoder: SendUni,
    send_decoder: SendUni,
    required_ref: usize,
    blocked_streams: BTreeMap<usize, Option<Waker>>,
    error: Option<(ErrorCode, String)>,
    side: Side,
}

#[derive(Clone)]
pub(crate) struct ConnectionRef {
    pub h3: Arc<Mutex<ConnectionInner>>,
    pub quic: quinn::Connection,
}

impl ConnectionRef {
    pub fn new(
        quic: quinn::Connection,
        settings: Settings,
        incoming_uni: IncomingUniStreams,
        incoming_bi: IncomingBiStreams,
        side: Side,
    ) -> Result<Self, ProtoError> {
        Ok(Self {
            h3: Arc::new(Mutex::new(ConnectionInner {
                inner: Connection::with_settings(settings)?,
                pending_uni: VecDeque::with_capacity(10),
                requests: VecDeque::with_capacity(16),
                requests_waker: None,
                driver_waker: None,
                quic: quic.clone(),
                control: None,
                recv_encoder: None,
                recv_decoder: None,
                send_control: SendUni::new(StreamType::CONTROL, quic.clone()),
                send_encoder: SendUni::new(StreamType::ENCODER, quic.clone()),
                send_decoder: SendUni::new(StreamType::DECODER, quic.clone()),
                required_ref: 0,
                blocked_streams: BTreeMap::new(),
                error: None,
                incoming_uni,
                incoming_bi,
                side,
            })),
            quic,
        })
    }
}
impl ConnectionInner {
    fn poll_send_uni(&mut self, cx: &mut Context) -> Result<(), Error> {
        if let Some(data) = self.inner.pending_control() {
            println!("{:?} sent control", self.side);
            self.send_control.push(data);
        }
        if let Some(data) = self.inner.pending_encoder() {
            println!("{:?} sending encoder: {:?}", self.side, data);
            self.send_encoder.push(data);
        }
        if let Some(data) = self.inner.pending_decoder() {
            println!("{:?} sending encoder: {:?}", self.side, data);
            self.send_decoder.push(data);
        }

        Pin::new(&mut self.send_control).poll(cx)?;
        Pin::new(&mut self.send_encoder).poll(cx)?;
        Pin::new(&mut self.send_decoder).poll(cx)?;
        Ok(())
    }

    pub fn on_encoded(&mut self, required_ref: usize) {
        if required_ref <= self.required_ref {
            return;
        }

        if let Some(t) = self.driver_waker.take() {
            println!("waking encoder");
            t.wake();
        }
    }

    fn poll_incoming_uni(&mut self, cx: &mut Context) -> Result<(), Error> {
        println!("{:?} poll incoming uni", self.side);
        loop {
            match Pin::new(&mut self.incoming_uni).poll_next(cx)? {
                Poll::Pending => break,
                Poll::Ready(None) => return Err(Error::Internal("incoming uni streams closed")),
                Poll::Ready(Some(recv)) => {
                    println!("{:?} got incoming uni", self.side);
                    self.pending_uni.push_back(Some(RecvUni::new(recv)));
                }
            }
        }

        while self.poll_recv_uni(cx) {}
        Ok(())
    }

    fn poll_recv_uni(&mut self, cx: &mut Context) -> bool {
        let resolved: Vec<(usize, Result<NewUni, Error>)> = self
            .pending_uni
            .iter_mut()
            .enumerate()
            .filter_map(|(i, x)| {
                let mut pending = x.take().unwrap();
                match Pin::new(&mut pending).poll(cx) {
                    Poll::Ready(y) => Some((i, y)),
                    Poll::Pending => {
                        std::mem::replace(x, Some(pending));
                        None
                    }
                }
            })
            .collect();

        let keep_going = !resolved.is_empty();
        let mut removed = 0;

        for (i, res) in resolved {
            self.pending_uni.remove(i - removed);
            removed += 1;
            println!("removed {}", i);
            match res {
                Err(Error::UnknownStream(ty)) => println!("unknown stream type {}", ty),
                Err(e) => {
                    self.set_error(ErrorCode::STREAM_CREATION_ERROR, format!("{:?}", e));
                }
                Ok(n) => match n {
                    NewUni::Control(stream) => match self.control {
                        None => self.control = Some(stream),
                        Some(_) => {
                            self.set_error(
                                ErrorCode::STREAM_CREATION_ERROR,
                                "control stream already open".into(),
                            );
                        }
                    },
                    NewUni::Decoder(s) => match self.recv_decoder {
                        None => {
                            println!("{:?} got decoder", self.side);
                            self.recv_decoder = Some((s, BytesMut::with_capacity(2048)));
                        }
                        Some(_) => {
                            self.set_error(
                                ErrorCode::STREAM_CREATION_ERROR,
                                "decoder stream already open".into(),
                            );
                        }
                    },
                    NewUni::Encoder(s) => match self.recv_encoder {
                        None => {
                            println!("{:?} got encoder", self.side);
                            self.recv_encoder = Some((s, BytesMut::with_capacity(2048)));
                        }
                        Some(_) => {
                            self.set_error(
                                ErrorCode::STREAM_CREATION_ERROR,
                                "encoder stream already open".into(),
                            );
                        }
                    },
                    NewUni::Push(_) => println!("push stream ignored"),
                },
            }
        }
        return keep_going;
    }

    fn poll_control(&mut self, cx: &mut Context) {
        let mut control = match self.control.as_mut() {
            None => return,
            Some(c) => c,
        };
        println!("{:?} polling control", self.side);

        loop {
            match Pin::new(&mut control).poll_next(cx) {
                Poll::Ready(None) => {
                    self.set_error(ErrorCode::CLOSED_CRITICAL_STREAM, "control closed".into());
                    break;
                }
                Poll::Ready(Some(Err(e))) => {
                    let (code, msg) = e.into();
                    self.set_error(code, msg);
                    break;
                }
                Poll::Ready(Some(Ok(frame))) => {
                    let has_remote_settings = self.inner.remote_settings().is_some();

                    match (has_remote_settings, self.side, frame) {
                        (_, _, HttpFrame::Settings(s)) => {
                            dbg!(&s);
                            self.inner.set_remote_settings(s);
                        }
                        (true, Side::Client, HttpFrame::Goaway(_)) => {
                            println!("GOAWAY frame ignored")
                        }
                        (true, Side::Server, HttpFrame::CancelPush(_)) => {
                            println!("CANCEL_PUSH frame ignored")
                        }
                        (true, Side::Server, HttpFrame::MaxPushId(_)) => {
                            println!("MAX_PUSH_ID frame ignored")
                        }
                        (false, Side::Server, HttpFrame::CancelPush(_))
                        | (false, Side::Server, HttpFrame::MaxPushId(_))
                        | (false, Side::Client, HttpFrame::Goaway(_)) => {
                            self.set_error(ErrorCode::MISSING_SETTINGS, "missing settings".into());
                            break;
                        }
                        _ => {
                            self.set_error(
                                ErrorCode::FRAME_UNEXPECTED,
                                "unexpected frame type on control stream".into(),
                            );
                            break;
                        }
                    }
                }
                Poll::Pending => return,
            }
        }
    }

    fn poll_encoder(&mut self, cx: &mut Context) -> Result<(), Error> {
        let (mut recv_encoder, mut buffer) = match self.recv_encoder.as_mut() {
            None => return Ok(()),
            Some((ref mut s, ref mut b)) => (s, b),
        };
        println!("{:?} polling encoder", self.side);

        loop {
            let mut my_array = [0; 2048];
            match Pin::new(&mut recv_encoder).poll_read(cx, &mut my_array[..])? {
                Poll::Ready(n) if n > 0 => {
                    buffer.extend_from_slice(&my_array[..n]);
                    println!("recieved encoder {:?}", buffer);
                    let (pos, max_recieved_ref) = {
                        let mut cur = Cursor::new(&mut buffer);
                        let max_recieved_ref = self.inner.on_recv_encoder(&mut cur)?;
                        (cur.position() as usize, max_recieved_ref)
                    };

                    buffer.advance(pos);
                    if buffer.is_empty() {
                        buffer.clear();
                    }

                    let blocked = self.blocked_streams.split_off(&max_recieved_ref);
                    let mut unblocked = mem::replace(&mut self.blocked_streams, blocked);
                    for stream in unblocked.values_mut() {
                        stream.take().unwrap().wake();
                    }
                }
                Poll::Ready(_) => {
                    return Err(Error::Peer("Encoder stream closed".into()));
                }
                Poll::Pending => {
                    println!("uni read nothing");
                    break;
                }
            }
        }
        Ok(())
    }

    fn poll_decoder(&mut self, cx: &mut Context) -> Result<(), Error> {
        let (mut recv_decoder, mut buffer) = match self.recv_decoder.as_mut() {
            None => return Ok(()),
            Some((ref mut s, ref mut b)) => (s, b),
        };
        println!("{:?} polling decoder", self.side);

        loop {
            let mut my_array = [0; 2048];
            match Pin::new(&mut recv_decoder).poll_read(cx, &mut my_array[..])? {
                Poll::Ready(n) if n > 0 => {
                    buffer.extend_from_slice(&my_array[..n]);
                    println!("recieved decoder {:?}", buffer);
                    let (pos, max_recieved_ref) = {
                        let mut cur = Cursor::new(&mut buffer);
                        let max_recieved_ref = self.inner.on_recv_decoder(&mut cur)?;
                        (cur.position() as usize, max_recieved_ref)
                    };

                    buffer.advance(pos);
                    if buffer.is_empty() {
                        buffer.clear();
                    }
                }
                Poll::Ready(_) => {
                    return Err(Error::Peer("Decoder stream closed".into()));
                }
                Poll::Pending => {
                    println!("uni read nothing");
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn decode_header(
        &mut self,
        cx: &mut Context,
        stream_id: StreamId,
        header: &HeadersFrame,
    ) -> Result<Option<Header>, Error> {
        let result = self.inner.decode_header(stream_id, header)?;
        if let Some(waker) = self.driver_waker.take() {
            waker.wake();
        }
        match result {
            DecodeResult::Decoded(h) => Ok(Some(h)),
            DecodeResult::MissingRefs(required_ref) => {
                self.blocked_streams
                    .insert(required_ref, Some(cx.waker().clone()));
                Ok(None)
            }
        }
    }

    fn poll_incoming_bi(&mut self, cx: &mut Context) -> Result<(), Error> {
        // TODO check side and error if not allowed
        loop {
            match Pin::new(&mut self.incoming_bi).poll_next(cx)? {
                Poll::Pending => return Ok(()),
                Poll::Ready(None) if self.side == Side::Server => {
                    return Err(Error::Peer("incoming bi stream Closed".into()));
                }
                Poll::Ready(None) => return Ok(()),
                Poll::Ready(Some((send, recv))) => {
                    self.requests.push_back((send, recv));
                    if let Some(t) = self.requests_waker.take() {
                        t.wake();
                    }
                }
            }
        }
    }

    fn set_error(&mut self, code: ErrorCode, msg: String) {
        self.error = Some((code, msg.into()));
    }
}
