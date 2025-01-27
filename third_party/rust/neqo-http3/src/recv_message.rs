// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use crate::hframe::{HFrame, HFrameReader};
use crate::push_controller::PushController;
use crate::qlog;
use crate::RecvMessageEvents;
use crate::RecvStream;
use crate::{Error, Header, Res};

use neqo_common::{qdebug, qinfo, qtrace};
use neqo_qpack::decoder::QPackDecoder;
use neqo_transport::{AppError, Connection};
use std::cell::RefCell;
use std::cmp::min;
use std::collections::VecDeque;
use std::convert::TryFrom;
use std::fmt::Debug;
use std::rc::Rc;

/*
 * Response stream state:
 *    WaitingForResponseHeaders : we wait for headers. in this state we can
 *                                also get a PUSH_PROMISE frame.
 *    DecodingHeaders : In this step the headers will be decoded. The stream
 *                      may be blocked in this state on encoder instructions.
 *    WaitingForData : we got HEADERS, we are waiting for one or more data
 *                     frames. In this state we can receive one or more
 *                     PUSH_PROMIS frames or a HEADERS frame carrying trailers.
 *    ReadingData : we got a DATA frame, now we letting the app read payload.
 *                  From here we will go back to WaitingForData state to wait
 *                  for more data frames or to CLosed state
 *    ClosePending : waiting for app to pick up data, after that we can delete
 * the TransactionClient.
 *    Closed
 */
#[derive(Debug)]
enum RecvMessageState {
    WaitingForResponseHeaders { frame_reader: HFrameReader },
    DecodingHeaders { header_block: Vec<u8>, fin: bool },
    WaitingForData { frame_reader: HFrameReader },
    ReadingData { remaining_data_len: usize },
    WaitingForFinAfterTrailers { frame_reader: HFrameReader },
    ClosePending, // Close must first be read by application
    Closed,
}

#[derive(Debug)]
struct PushInfo {
    push_id: u64,
    header_block: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct RecvMessage {
    state: RecvMessageState,
    conn_events: Box<dyn RecvMessageEvents>,
    push_handler: Option<Rc<RefCell<PushController>>>,
    stream_id: u64,
    blocked_push_promise: VecDeque<PushInfo>,
}

impl ::std::fmt::Display for RecvMessage {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "RecvMessage stream_id:{}", self.stream_id)
    }
}

impl RecvMessage {
    pub fn new(
        stream_id: u64,
        conn_events: Box<dyn RecvMessageEvents>,
        push_handler: Option<Rc<RefCell<PushController>>>,
    ) -> Self {
        Self {
            state: RecvMessageState::WaitingForResponseHeaders {
                frame_reader: HFrameReader::new(),
            },
            conn_events,
            push_handler,
            stream_id,
            blocked_push_promise: VecDeque::new(),
        }
    }

    fn handle_headers_frame(
        &mut self,
        header_block: Vec<u8>,
        fin: bool,
        decoder: &mut QPackDecoder,
    ) -> Res<()> {
        match self.state {
            RecvMessageState::WaitingForResponseHeaders {..} => {
                if header_block.is_empty() {
                    self.add_headers(None, fin, decoder);
                } else {
                    self.state = RecvMessageState::DecodingHeaders { header_block, fin };
                }
             }
            RecvMessageState::WaitingForData { ..} => {
                // TODO implement trailers, for now just ignore them.
                self.state = RecvMessageState::WaitingForFinAfterTrailers{frame_reader: HFrameReader::new()};
            }
            RecvMessageState::WaitingForFinAfterTrailers {..} => {
                return Err(Error::HttpFrameUnexpected);
            }
            _ => unreachable!("This functions is only called in WaitingForResponseHeaders | WaitingForData | WaitingForFinAfterTrailers state.")
         }
        Ok(())
    }

    fn handle_data_frame(&mut self, len: u64, fin: bool) -> Res<()> {
        match self.state {
            RecvMessageState::WaitingForResponseHeaders {..} | RecvMessageState::WaitingForFinAfterTrailers {..} => {
                return Err(Error::HttpFrameUnexpected);
            }
            RecvMessageState::WaitingForData {..} => {
                if len > 0 {
                    if fin {
                        return Err(Error::HttpFrame);
                    }
                    self.state = RecvMessageState::ReadingData {
                        remaining_data_len: usize::try_from(len).or(Err(Error::HttpFrame))?,
                    };
                }
            }
            _ => unreachable!("This functions is only called in WaitingForResponseHeaders | WaitingForData | WaitingForFinAfterTrailers state.")
        }
        Ok(())
    }

    fn add_headers(&mut self, headers: Option<Vec<Header>>, fin: bool, decoder: &mut QPackDecoder) {
        if fin {
            self.conn_events.header_ready(self.stream_id, headers, true);
            self.set_closed(decoder);
        } else {
            self.conn_events
                .header_ready(self.stream_id, headers, false);
            self.state = RecvMessageState::WaitingForData {
                frame_reader: HFrameReader::new(),
            };
        }
    }

    fn set_state_to_close_pending(
        &mut self,
        decoder: &mut QPackDecoder,
        post_readable_event: bool,
    ) {
        // Stream has received fin. Depending on headers state set header_ready
        // or data_readable event so that app can pick up the fin.
        qtrace!([self], "set_state_to_close_pending: state={:?}", self.state);

        match self.state {
            RecvMessageState::WaitingForResponseHeaders { .. } => {
                self.conn_events.header_ready(self.stream_id, None, true);
                self.set_closed(decoder);
            }
            RecvMessageState::ReadingData { .. } => {}
            RecvMessageState::WaitingForData { .. }
            | RecvMessageState::WaitingForFinAfterTrailers { .. } => {
                if post_readable_event {
                    self.conn_events.data_readable(self.stream_id)
                }
            }
            _ => unreachable!("Closing an already closed transaction."),
        }
        if !matches!(self.state, RecvMessageState::Closed) {
            self.state = RecvMessageState::ClosePending;
        }
    }

    fn handle_push_promise(
        &mut self,
        push_id: u64,
        header_block: Vec<u8>,
        decoder: &mut QPackDecoder,
    ) -> Res<()> {
        if self.push_handler.is_none() {
            return Err(Error::HttpFrameUnexpected);
        }

        if !self.blocked_push_promise.is_empty() {
            self.blocked_push_promise.push_back(PushInfo {
                push_id,
                header_block,
            });
        } else if let Some(headers) = decoder.decode_header_block(&header_block, self.stream_id)? {
            self.push_handler
                .as_ref()
                .ok_or(Error::HttpFrameUnexpected)?
                .borrow_mut()
                .new_push_promise(push_id, self.stream_id, headers)?
        } else {
            self.blocked_push_promise.push_back(PushInfo {
                push_id,
                header_block,
            });
        }
        Ok(())
    }

    fn receive_internal(
        &mut self,
        conn: &mut Connection,
        decoder: &mut QPackDecoder,
        post_readable_event: bool,
    ) -> Res<()> {
        let label = ::neqo_common::log_subject!(::log::Level::Debug, self);
        loop {
            qdebug!([label], "state={:?}.", self.state);
            match &mut self.state {
                // In the following 3 states we need to read frames.
                RecvMessageState::WaitingForResponseHeaders { frame_reader }
                | RecvMessageState::WaitingForData { frame_reader }
                | RecvMessageState::WaitingForFinAfterTrailers { frame_reader } => {
                    match frame_reader.receive(conn, self.stream_id)? {
                        (None, true) => {
                            self.set_state_to_close_pending(decoder, post_readable_event);
                            break Ok(());
                        }
                        (None, false) => break Ok(()),
                        (Some(frame), fin) => {
                            qinfo!(
                                [self],
                                "A new frame has been received: {:?}; state={:?} fin={}",
                                frame,
                                self.state,
                                fin,
                            );
                            match frame {
                                HFrame::Headers { header_block } => {
                                    self.handle_headers_frame(header_block, fin, decoder)?
                                }
                                HFrame::Data { len } => self.handle_data_frame(len, fin)?,
                                HFrame::PushPromise {
                                    push_id,
                                    header_block,
                                } => self.handle_push_promise(push_id, header_block, decoder)?,
                                _ => break Err(Error::HttpFrameUnexpected),
                            }
                            if matches!(self.state, RecvMessageState::Closed) {
                                break Ok(());
                            }
                            if fin && !matches!(self.state, RecvMessageState::DecodingHeaders{..}) {
                                self.set_state_to_close_pending(decoder, post_readable_event);
                                break Ok(());
                            }
                        }
                    };
                }
                RecvMessageState::DecodingHeaders {
                    ref header_block,
                    fin,
                } => {
                    if decoder.refers_dynamic_table(header_block)?
                        && !self.blocked_push_promise.is_empty()
                    {
                        qinfo!(
                            [self],
                            "decoding header is blocked waiting for a push_promise header block."
                        );
                        break Ok(());
                    }
                    let done = *fin;
                    if let Some(headers) =
                        decoder.decode_header_block(header_block, self.stream_id)?
                    {
                        self.add_headers(Some(headers), done, decoder);
                        if done {
                            break Ok(());
                        }
                    } else {
                        qinfo!([self], "decoding header is blocked.");
                        break Ok(());
                    }
                }
                RecvMessageState::ReadingData { .. } => {
                    if post_readable_event {
                        self.conn_events.data_readable(self.stream_id);
                    }
                    break Ok(());
                }
                RecvMessageState::ClosePending | RecvMessageState::Closed => {
                    panic!("Stream readable after being closed!");
                }
            };
        }
    }

    fn set_closed(&mut self, decoder: &mut QPackDecoder) {
        if !self.blocked_push_promise.is_empty() {
            decoder.cancel_stream(self.stream_id);
        }
        self.state = RecvMessageState::Closed;
    }

    fn closing(&self) -> bool {
        matches!(
            self.state,
            RecvMessageState::ClosePending | RecvMessageState::Closed
        )
    }
}

impl RecvStream for RecvMessage {
    fn receive(&mut self, conn: &mut Connection, decoder: &mut QPackDecoder) -> Res<()> {
        self.receive_internal(conn, decoder, true)
    }

    fn header_unblocked(&mut self, conn: &mut Connection, decoder: &mut QPackDecoder) -> Res<()> {
        while let Some(p) = self.blocked_push_promise.front() {
            if let Some(headers) = decoder.decode_header_block(&p.header_block, self.stream_id)? {
                self.push_handler
                    .as_ref()
                    .ok_or(Error::HttpFrameUnexpected)?
                    .borrow_mut()
                    .new_push_promise(p.push_id, self.stream_id, headers)?;
                self.blocked_push_promise.pop_front();
            }
        }

        if self.blocked_push_promise.is_empty() {
            return self.receive_internal(conn, decoder, true);
        }
        Ok(())
    }

    fn done(&self) -> bool {
        matches!(self.state, RecvMessageState::Closed)
    }

    fn stream_reset_recv(&self, app_error: AppError, decoder: &mut QPackDecoder) {
        if !self.closing() || !self.blocked_push_promise.is_empty() {
            decoder.cancel_stream(self.stream_id);
        }
        self.conn_events.reset(self.stream_id, app_error);
    }

    fn stream_reset(&self, decoder: &mut QPackDecoder) {
        if !self.closing() || !self.blocked_push_promise.is_empty() {
            decoder.cancel_stream(self.stream_id);
        }
    }

    fn read_data(
        &mut self,
        conn: &mut Connection,
        decoder: &mut QPackDecoder,
        buf: &mut [u8],
    ) -> Res<(usize, bool)> {
        let mut written = 0;
        loop {
            match self.state {
                RecvMessageState::ReadingData {
                    ref mut remaining_data_len,
                } => {
                    let to_read = min(*remaining_data_len, buf.len() - written);
                    let (amount, fin) = conn
                        .stream_recv(self.stream_id, &mut buf[written..written + to_read])
                        .map_err(|e| Error::map_stream_recv_errors(&e))?;
                    qlog::h3_data_moved_up(conn.qlog_mut(), self.stream_id, amount);

                    debug_assert!(amount <= to_read);
                    *remaining_data_len -= amount;
                    written += amount;

                    if fin {
                        if *remaining_data_len > 0 {
                            return Err(Error::HttpFrame);
                        }
                        self.set_closed(decoder);
                        break Ok((written, fin));
                    } else if *remaining_data_len == 0 {
                        self.state = RecvMessageState::WaitingForData {
                            frame_reader: HFrameReader::new(),
                        };
                        self.receive_internal(conn, decoder, false)?;
                    } else {
                        break Ok((written, false));
                    }
                }
                RecvMessageState::ClosePending => {
                    self.set_closed(decoder);
                    break Ok((written, true));
                }
                _ => break Ok((written, false)),
            }
        }
    }
}
