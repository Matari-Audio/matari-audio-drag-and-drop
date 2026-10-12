//! A real-connection interposer for the selection cleanup race regression.

use std::cell::Cell;
use std::io::IoSlice;

use x11rb::connection::{
    BufWithFds, Connection, DiscardMode, RawEventAndSeqNumber, ReplyOrError, RequestConnection,
    RequestKind, SequenceNumber,
};
use x11rb::cookie::{Cookie, CookieWithFds, VoidCookie};
use x11rb::errors::{ConnectionError, ParseError, ReplyOrIdError};
use x11rb::protocol::{
    Event,
    xproto::{ConnectionExt, Setup},
};
use x11rb::rust_connection::RustConnection;
use x11rb::utils::RawFdContainer;
use x11rb::x11_utils::{ExtensionInformation, TryParse, TryParseFd, X11Error};

pub struct SelectionRace<'a> {
    inner: &'a RustConnection,
    peer: &'a RustConnection,
    source: u32,
    peer_window: u32,
    selection: u32,
    ownership_query: Cell<Option<SequenceNumber>>,
    took_over: Cell<bool>,
    pub sent_explicit_clear: Cell<bool>,
}

impl<'a> SelectionRace<'a> {
    pub fn new(
        inner: &'a RustConnection,
        peer: &'a RustConnection,
        source: u32,
        peer_window: u32,
        selection: u32,
    ) -> Self {
        Self {
            inner,
            peer,
            source,
            peer_window,
            selection,
            ownership_query: Cell::new(None),
            took_over: Cell::new(false),
            sent_explicit_clear: Cell::new(false),
        }
    }

    fn take_over(&self) {
        if !self.took_over.replace(true) {
            self.peer
                .set_selection_owner(self.peer_window, self.selection, x11rb::CURRENT_TIME)
                .unwrap()
                .check()
                .unwrap();
            assert_eq!(
                self.peer
                    .get_selection_owner(self.selection)
                    .unwrap()
                    .reply()
                    .unwrap()
                    .owner,
                self.peer_window
            );
        }
    }

    pub fn took_over(&self) -> bool {
        self.took_over.get()
    }
}

fn word(request: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(request[offset..offset + 4].try_into().unwrap())
}

impl RequestConnection for SelectionRace<'_> {
    type Buf = <RustConnection as RequestConnection>::Buf;

    fn send_request_with_reply<R: TryParse>(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<Cookie<'_, Self, R>, ConnectionError> {
        let cookie = self.inner.send_request_with_reply::<R>(bufs, fds)?;
        let sequence = cookie.sequence_number();
        // Transfer the cookie's discard/reply responsibility to this wrapper.
        std::mem::forget(cookie);
        if bufs[0][0] == 23 && word(&bufs[0], 4) == self.selection {
            self.ownership_query.set(Some(sequence));
        }
        Ok(Cookie::new(self, sequence))
    }

    fn send_request_with_reply_with_fds<R: TryParseFd>(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<CookieWithFds<'_, Self, R>, ConnectionError> {
        let cookie = self
            .inner
            .send_request_with_reply_with_fds::<R>(bufs, fds)?;
        let sequence = cookie.sequence_number();
        std::mem::forget(cookie);
        Ok(CookieWithFds::new(self, sequence))
    }

    fn send_request_without_reply(
        &self,
        bufs: &[IoSlice<'_>],
        fds: Vec<RawFdContainer>,
    ) -> Result<VoidCookie<'_, Self>, ConnectionError> {
        let request = &bufs[0];
        if request[0] == 22 && word(request, 4) == x11rb::NONE && word(request, 8) == self.selection
        {
            self.sent_explicit_clear.set(true);
        }
        if request[0] == 4 && word(request, 4) == self.source {
            // Fixed cleanup has no GetOwner/clear boundary: race the source
            // destruction instead. Never restore a peer cleared by old code.
            self.take_over();
        }
        let cookie = self.inner.send_request_without_reply(bufs, fds)?;
        let sequence = cookie.sequence_number();
        std::mem::forget(cookie);
        Ok(VoidCookie::new(self, sequence))
    }

    fn wait_for_reply_or_raw_error(
        &self,
        sequence: SequenceNumber,
    ) -> Result<ReplyOrError<Self::Buf>, ConnectionError> {
        let reply = self.inner.wait_for_reply_or_raw_error(sequence)?;
        if self.ownership_query.get() == Some(sequence) {
            self.ownership_query.set(None);
            // The caller receives the old owner reply after a second client
            // has already taken ownership, deterministically exercising TOCTOU.
            self.take_over();
        }
        Ok(reply)
    }

    fn wait_for_reply(
        &self,
        sequence: SequenceNumber,
    ) -> Result<Option<Self::Buf>, ConnectionError> {
        self.inner.wait_for_reply(sequence)
    }

    fn wait_for_reply_with_fds_raw(
        &self,
        sequence: SequenceNumber,
    ) -> Result<ReplyOrError<BufWithFds<Self::Buf>, Self::Buf>, ConnectionError> {
        self.inner.wait_for_reply_with_fds_raw(sequence)
    }

    fn discard_reply(&self, sequence: SequenceNumber, kind: RequestKind, mode: DiscardMode) {
        self.inner.discard_reply(sequence, kind, mode);
    }

    fn prefetch_extension_information(&self, name: &'static str) -> Result<(), ConnectionError> {
        self.inner.prefetch_extension_information(name)
    }

    fn extension_information(
        &self,
        name: &'static str,
    ) -> Result<Option<ExtensionInformation>, ConnectionError> {
        self.inner.extension_information(name)
    }

    fn check_for_raw_error(
        &self,
        sequence: SequenceNumber,
    ) -> Result<Option<Self::Buf>, ConnectionError> {
        self.inner.check_for_raw_error(sequence)
    }

    fn prefetch_maximum_request_bytes(&self) {
        self.inner.prefetch_maximum_request_bytes();
    }
    fn maximum_request_bytes(&self) -> usize {
        self.inner.maximum_request_bytes()
    }
    fn parse_error(&self, error: &[u8]) -> Result<X11Error, ParseError> {
        self.inner.parse_error(error)
    }
    fn parse_event(&self, event: &[u8]) -> Result<Event, ParseError> {
        self.inner.parse_event(event)
    }
}

impl Connection for SelectionRace<'_> {
    fn wait_for_raw_event_with_sequence(
        &self,
    ) -> Result<RawEventAndSeqNumber<Self::Buf>, ConnectionError> {
        self.inner.wait_for_raw_event_with_sequence()
    }

    fn poll_for_raw_event_with_sequence(
        &self,
    ) -> Result<Option<RawEventAndSeqNumber<Self::Buf>>, ConnectionError> {
        self.inner.poll_for_raw_event_with_sequence()
    }

    fn flush(&self) -> Result<(), ConnectionError> {
        self.inner.flush()
    }
    fn setup(&self) -> &Setup {
        self.inner.setup()
    }
    fn generate_id(&self) -> Result<u32, ReplyOrIdError> {
        self.inner.generate_id()
    }
}
