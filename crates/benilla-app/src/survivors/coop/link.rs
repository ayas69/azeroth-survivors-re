//! The line to the co-op relay (`wenilla-host`'s `/coop/{room}`): JSON text frames out and in.
//! On the desktop a blocking WebSocket runs on its own thread; in the browser, `web_sys`'s
//! WebSocket hands frames to callbacks on the event loop. Either way the game side only ever
//! `send`s and `drain`s, once a frame, and never waits.

use std::sync::{Arc, Mutex};

/// What the line has said since the last drain.
#[derive(Default)]
struct Inbox {
    frames: Vec<String>,
    open: bool,
    closed: bool,
}

pub(super) struct Link {
    inbox: Arc<Mutex<Inbox>>,
    #[cfg(not(target_arch = "wasm32"))]
    out: crossbeam_channel::Sender<String>,
    #[cfg(target_arch = "wasm32")]
    socket: web_sys::WebSocket,
    #[cfg(target_arch = "wasm32")]
    _callbacks: Vec<wasm_bindgen::closure::Closure<dyn FnMut(wasm_bindgen::JsValue)>>,
}

impl Link {
    /// Frames received since the last call, and whether the line has dropped.
    pub fn drain(&self) -> (Vec<String>, bool) {
        let mut inbox = self.inbox.lock().unwrap();
        (std::mem::take(&mut inbox.frames), inbox.closed)
    }

    pub fn is_open(&self) -> bool {
        let inbox = self.inbox.lock().unwrap();
        inbox.open && !inbox.closed
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Link {
    pub fn open(url: &str) -> Self {
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let (out, rx) = crossbeam_channel::unbounded::<String>();
        let url = url.to_owned();
        let shared = Arc::clone(&inbox);
        std::thread::Builder::new()
            .name("survivors-coop".into())
            .spawn(move || {
                if let Err(e) = run(&url, &shared, &rx) {
                    bevy::log::warn!("co-op link to {url} ended: {e}");
                }
                shared.lock().unwrap().closed = true;
            })
            .expect("spawn the co-op thread");
        Self { inbox, out }
    }

    pub fn send(&self, frame: String) {
        let _ = self.out.send(frame);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run(
    url: &str,
    inbox: &Mutex<Inbox>,
    out: &crossbeam_channel::Receiver<String>,
) -> anyhow::Result<()> {
    use std::time::Duration;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::Message;

    let (mut ws, _) = tungstenite::connect(url)?;
    // Short reads, so the same thread also writes what the game queues.
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_millis(5)))?;
        s.set_nodelay(true)?;
    }
    inbox.lock().unwrap().open = true;
    loop {
        while let Ok(frame) = out.try_recv() {
            ws.send(Message::text(frame))?;
        }
        match ws.read() {
            Ok(Message::Text(t)) => inbox.lock().unwrap().frames.push(t.to_string()),
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Link {
    pub fn open(url: &str) -> Self {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::{JsCast, JsValue};

        let inbox = Arc::new(Mutex::new(Inbox::default()));
        let socket = match web_sys::WebSocket::new(url) {
            Ok(s) => s,
            Err(_) => {
                inbox.lock().unwrap().closed = true;
                // A socket that never opens: the run carries on alone.
                return Self {
                    inbox,
                    socket: web_sys::WebSocket::new("ws://0.0.0.0:9/").unwrap(),
                    _callbacks: Vec::new(),
                };
            }
        };
        let on_open = {
            let inbox = Arc::clone(&inbox);
            Closure::<dyn FnMut(JsValue)>::new(move |_| inbox.lock().unwrap().open = true)
        };
        let on_message = {
            let inbox = Arc::clone(&inbox);
            Closure::<dyn FnMut(JsValue)>::new(move |ev: JsValue| {
                let ev: web_sys::MessageEvent = ev.unchecked_into();
                if let Some(t) = ev.data().as_string() {
                    inbox.lock().unwrap().frames.push(t);
                }
            })
        };
        let on_close = {
            let inbox = Arc::clone(&inbox);
            Closure::<dyn FnMut(JsValue)>::new(move |_| inbox.lock().unwrap().closed = true)
        };
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        socket.set_onerror(Some(on_close.as_ref().unchecked_ref()));
        Self {
            inbox,
            socket,
            _callbacks: vec![on_open, on_message, on_close],
        }
    }

    pub fn send(&self, frame: String) {
        let _ = self.socket.send_with_str(&frame);
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for Link {
    fn drop(&mut self) {
        self.socket.set_onclose(None);
        self.socket.set_onerror(None);
        self.socket.set_onmessage(None);
        self.socket.set_onopen(None);
        let _ = self.socket.close();
    }
}
