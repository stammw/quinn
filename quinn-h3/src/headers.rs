use std::pin::Pin;
use std::task::Context;

use futures::{Future, Poll};
use quinn::SendStream;
use quinn_proto::StreamId;

use crate::{
    connection::ConnectionRef,
    frame::WriteFrame,
    proto::{frame::HeadersFrame, headers::Header},
    Error,
};

pub struct DecodeHeaders {
    frame: Option<HeadersFrame>,
    conn: ConnectionRef,
    stream_id: StreamId,
}

impl DecodeHeaders {
    pub(crate) fn new(frame: HeadersFrame, conn: ConnectionRef, stream_id: StreamId) -> Self {
        Self {
            conn,
            stream_id,
            frame: Some(frame),
        }
    }
}

impl Future for DecodeHeaders {
    type Output = Result<Header, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match self.frame {
            None => Poll::Ready(Err(crate::Error::Internal("frame none"))),
            Some(ref frame) => {
                let result = self
                    .conn
                    .h3
                    .lock()
                    .unwrap()
                    .decode_header(cx, self.stream_id, frame);

                match result {
                    Ok(None) => Poll::Pending,
                    Ok(Some(decoded)) => Poll::Ready(Ok(decoded)),
                    Err(e) => {
                        Poll::Ready(Err(Error::peer(format!("decoding header failed: {:?}", e))))
                    }
                }
            }
        }
    }
}

pub(crate) struct SendHeaders(WriteFrame);

impl SendHeaders {
    pub fn new(
        header: Header,
        conn: &ConnectionRef,
        send: SendStream,
        stream_id: StreamId,
    ) -> Result<Self, Error> {
        let frame = {
            let conn = &mut conn.h3.lock().unwrap();
            let (required_ref, header) = conn.inner.encode_header(stream_id, header)?; // TODO check if the task has been encoded
            conn.on_encoded(required_ref);
            header
        };

        Ok(Self(WriteFrame::new(send, frame)))
    }
}

impl<'a> Future for SendHeaders {
    type Output = Result<SendStream, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx).map_err(Into::into)
    }
}
