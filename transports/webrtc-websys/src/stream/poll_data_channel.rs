use std::{
    cmp::min,
    io,
    pin::Pin,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    task::{Context, Poll},
};

use bytes::BytesMut;
use futures::{task::AtomicWaker, AsyncRead, AsyncWrite};
use libp2p_webrtc_utils::MAX_MSG_LEN;
use wasm_bindgen::prelude::*;
use web_sys::{Event, MessageEvent, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelState};

/// Keeps JS event-handler closures alive. Shared by all [`PollDataChannel`] clones on one SCTP stream.
struct ChannelHandlers {
    _on_open: Rc<Closure<dyn FnMut(RtcDataChannelEvent)>>,
    _on_write: Rc<Closure<dyn FnMut(Event)>>,
    _on_close: Rc<Closure<dyn FnMut(Event)>>,
    _on_message: Rc<Closure<dyn FnMut(MessageEvent)>>,
}

/// [`PollDataChannel`] is a wrapper around [`RtcDataChannel`] which implements [`AsyncRead`] and
/// [`AsyncWrite`].
///
/// Cloned by [`libp2p_webrtc_utils::Stream`] (io + drop listener). The last clone must detach JS
/// handlers before the closures are freed.
pub(crate) struct PollDataChannel {
    inner: RtcDataChannel,
    /// One per logical channel; used to know when to detach JS handlers on drop.
    shares: Rc<()>,
    new_data_waker: Rc<AtomicWaker>,
    read_buffer: Rc<Mutex<BytesMut>>,
    open_waker: Rc<AtomicWaker>,
    write_waker: Rc<AtomicWaker>,
    close_waker: Rc<AtomicWaker>,
    overloaded: Rc<AtomicBool>,
    _handlers: Rc<ChannelHandlers>,
}

fn detach_js_handlers(dc: &RtcDataChannel) {
    let _ = dc.set_onopen(None);
    let _ = dc.set_onmessage(None);
    let _ = dc.set_onbufferedamountlow(None);
    let _ = dc.set_onclose(None);
}

/// Wake the async task on a later turn of the event loop (never synchronously from `onmessage`).
fn defer_waker_wake(waker: Rc<AtomicWaker>) {
    let closure = Closure::once(move || {
        waker.wake();
    });
    if let Some(window) = web_sys::window() {
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            closure.as_ref().unchecked_ref(),
            0,
        );
    }
    closure.forget();
}

impl Clone for PollDataChannel {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shares: self.shares.clone(),
            new_data_waker: self.new_data_waker.clone(),
            read_buffer: self.read_buffer.clone(),
            open_waker: self.open_waker.clone(),
            write_waker: self.write_waker.clone(),
            close_waker: self.close_waker.clone(),
            overloaded: self.overloaded.clone(),
            _handlers: self._handlers.clone(),
        }
    }
}

impl Drop for PollDataChannel {
    fn drop(&mut self) {
        if Rc::strong_count(&self.shares) == 1 {
            detach_js_handlers(&self.inner);
        }
    }
}

impl PollDataChannel {
    pub(crate) fn id(&self) -> Option<u16> {
        self.inner.id().map(|id| id as u16)
    }

    pub(crate) fn ready_state(&self) -> RtcDataChannelState {
        self.inner.ready_state()
    }

    pub(crate) fn new(inner: RtcDataChannel) -> Self {
        let open_waker = Rc::new(AtomicWaker::new());
        let inner_for_open = inner.clone();
        let on_open_closure = Closure::new({
            let open_waker = open_waker.clone();
            let inner_for_open = inner_for_open.clone();

            move |_: RtcDataChannelEvent| {
                tracing::debug!(
                    target: "libp2p_webrtc_mux",
                    dc_id = ?inner_for_open.id(),
                    ready_state = ?inner_for_open.ready_state(),
                    "data channel opened"
                );
                defer_waker_wake(open_waker.clone());
            }
        });
        inner.set_onopen(Some(on_open_closure.as_ref().unchecked_ref()));

        let write_waker = Rc::new(AtomicWaker::new());
        inner.set_buffered_amount_low_threshold(0);
        let on_write_closure = Closure::new({
            let write_waker = write_waker.clone();

            move |_: Event| {
                tracing::trace!("DataChannel available for writing (again)");
                defer_waker_wake(write_waker.clone());
            }
        });
        inner.set_onbufferedamountlow(Some(on_write_closure.as_ref().unchecked_ref()));

        let close_waker = Rc::new(AtomicWaker::new());
        let on_close_closure = Closure::new({
            let close_waker = close_waker.clone();

            move |_: Event| {
                tracing::trace!("DataChannel closed");
                defer_waker_wake(close_waker.clone());
            }
        });
        inner.set_onclose(Some(on_close_closure.as_ref().unchecked_ref()));

        let new_data_waker = Rc::new(AtomicWaker::new());
        let read_buffer = Rc::new(Mutex::new(BytesMut::new()));
        let overloaded = Rc::new(AtomicBool::new(false));

        let on_message_closure = Closure::<dyn FnMut(_)>::new({
            let new_data_waker = new_data_waker.clone();
            let read_buffer = read_buffer.clone();
            let overloaded = overloaded.clone();

            move |ev: MessageEvent| {
                let data = js_sys::Uint8Array::new(&ev.data());

                let mut read_buffer = read_buffer.lock().unwrap();

                if read_buffer.len() + data.length() as usize > MAX_MSG_LEN {
                    overloaded.store(true, Ordering::SeqCst);
                    tracing::warn!("Remote is overloading us with messages, resetting stream",);
                    return;
                }

                read_buffer.extend_from_slice(&data.to_vec());
                defer_waker_wake(new_data_waker.clone());
            }
        });
        inner.set_onmessage(Some(on_message_closure.as_ref().unchecked_ref()));

        let handlers = Rc::new(ChannelHandlers {
            _on_open: Rc::new(on_open_closure),
            _on_write: Rc::new(on_write_closure),
            _on_close: Rc::new(on_close_closure),
            _on_message: Rc::new(on_message_closure),
        });

        Self {
            inner,
            shares: Rc::new(()),
            new_data_waker,
            read_buffer,
            open_waker,
            write_waker,
            close_waker,
            overloaded,
            _handlers: handlers,
        }
    }

    /// Returns the current [RtcDataChannel] BufferedAmount
    fn buffered_amount(&self) -> usize {
        self.inner.buffered_amount() as usize
    }

    /// Whether the data channel is ready for reading or writing.
    pub(crate) fn poll_ready(&mut self, cx: &mut Context) -> Poll<io::Result<()>> {
        match self.inner.ready_state() {
            RtcDataChannelState::Connecting => {
                self.open_waker.register(cx.waker());
                return Poll::Pending;
            }
            RtcDataChannelState::Closing | RtcDataChannelState::Closed => {
                return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
            }
            RtcDataChannelState::Open | RtcDataChannelState::__Invalid => {}
            _ => {}
        }

        if self.overloaded.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "remote overloaded us with messages",
            )));
        }

        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for PollDataChannel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        futures::ready!(this.poll_ready(cx))?;

        let mut read_buffer = this.read_buffer.lock().unwrap();

        if read_buffer.is_empty() {
            this.new_data_waker.register(cx.waker());
            return Poll::Pending;
        }

        let split_index = min(buf.len(), read_buffer.len());

        let bytes_to_return = read_buffer.split_to(split_index);
        let len = bytes_to_return.len();
        buf[..len].copy_from_slice(&bytes_to_return);

        Poll::Ready(Ok(len))
    }
}

impl AsyncWrite for PollDataChannel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        futures::ready!(this.poll_ready(cx))?;

        debug_assert!(this.buffered_amount() <= MAX_MSG_LEN);
        let remaining_space = MAX_MSG_LEN - this.buffered_amount();

        if remaining_space == 0 {
            this.write_waker.register(cx.waker());
            return Poll::Pending;
        }

        let bytes_to_send = min(buf.len(), remaining_space);

        if this
            .inner
            .send_with_u8_array(&buf[..bytes_to_send])
            .is_err()
        {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }

        Poll::Ready(Ok(bytes_to_send))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.buffered_amount() == 0 {
            return Poll::Ready(Ok(()));
        }

        self.write_waker.register(cx.waker());
        Poll::Pending
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.ready_state() == RtcDataChannelState::Closed {
            return Poll::Ready(Ok(()));
        }

        if self.ready_state() != RtcDataChannelState::Closing {
            self.inner.close();
        }

        self.close_waker.register(cx.waker());
        Poll::Pending
    }
}
