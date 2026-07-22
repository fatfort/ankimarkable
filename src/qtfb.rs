//! QTFB client for the reMarkable Paper Pro (portrait-only).
//!
//! Lifted from `fastterm/src/qtfb.rs` (wire layout verified against rm-appload's
//! common.h). AppLoad allocates the framebuffer and passes the key in `QTFB_KEY`;
//! we connect to `/tmp/qtfb.sock` (SOCK_SEQPACKET), receive the shm key + size,
//! mmap `/dev/shm/qtfb_<key>` RW, then push pixels + region/refresh messages.
//!
//! Anki uses the native portrait panel (1620×2160), so the rotation machinery
//! from fastterm is dropped here. Card pixels are composited via `blit_rgba`.

use std::cell::Cell;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::ptr;

// ── message types (common.h) ────────────────────────────────────────────────
pub const MESSAGE_INITIALIZE: u8 = 0;
pub const MESSAGE_UPDATE: u8 = 1;
pub const MESSAGE_TERMINATE: u8 = 3;
pub const MESSAGE_USERINPUT: u8 = 4;
pub const MESSAGE_SET_REFRESH_MODE: u8 = 5;
pub const MESSAGE_REQUEST_FULL_REFRESH: u8 = 6;

pub const FBFMT_RMPP_RGB888: u8 = 1;

pub const UPDATE_ALL: i32 = 0;
pub const UPDATE_PARTIAL: i32 = 1;

// refresh / waveform modes (rm-appload maps 0..4 → Mono/DU/A2/GC16/GL16)
pub const REFRESH_MODE_UFAST: i32 = 0; // Mono — fastest, coarsest (native pen path)
pub const REFRESH_MODE_FAST: i32 = 1; // DU (binary B/W) — noticeably laggy for ink
pub const REFRESH_MODE_ANIMATE: i32 = 2; // A2 — fastterm's "buttery" typing path; ink hot path
pub const REFRESH_MODE_CONTENT: i32 = 3; // GC16 — clean full grayscale/colour, settle only

// input event types (server → client, MESSAGE_USERINPUT)
pub const INPUT_TOUCH_PRESS: i32 = 0x10;
pub const INPUT_TOUCH_RELEASE: i32 = 0x11;
pub const INPUT_TOUCH_UPDATE: i32 = 0x12;
pub const INPUT_BTN_PRESS: i32 = 0x30;

#[derive(Clone, Copy, Debug)]
pub struct InputEvent {
    pub input_type: i32,
    pub dev_id: i32,
    pub x: i32,
    pub y: i32,
    pub d: i32,
}

pub const RMPP_WIDTH: usize = 1620;
pub const RMPP_HEIGHT: usize = 2160;

const SOCKET_PATH: &str = "/tmp/qtfb.sock";

#[repr(C)]
#[derive(Clone, Copy)]
struct InitMessageContents {
    framebuffer_key: u32,
    framebuffer_type: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UpdateRegionMessageContents {
    msg_type: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

#[repr(C)]
union ClientMessageContents {
    init: InitMessageContents,
    update: UpdateRegionMessageContents,
    refresh_mode: i32,
}

#[repr(C)]
struct ClientMessage {
    msg_type: u8,
    contents: ClientMessageContents,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InitMessageResponseContents {
    shm_key_defined: i32,
    shm_size: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UserInputContents {
    input_type: i32,
    dev_id: i32,
    x: i32,
    y: i32,
    d: i32,
}

#[repr(C)]
union ServerMessageContents {
    init: InitMessageResponseContents,
    user_input: UserInputContents,
}

#[repr(C)]
struct ServerMessage {
    msg_type: u8,
    contents: ServerMessageContents,
}

pub struct Qtfb {
    fd: i32,
    shm_ptr: *mut libc::c_void,
    pub shm: &'static mut [u8],
    pub width: usize,
    pub height: usize,
    // Last refresh mode sent. The rm-appload server does an expensive wait on every
    // MESSAGE_SET_REFRESH_MODE, so we only send it on an actual change — a continuous
    // stroke then stays in A2 with zero mode messages.
    current_mode: Cell<i32>,
}

impl Qtfb {
    pub fn connect_from_env() -> io::Result<Self> {
        let key: u32 = std::env::var("QTFB_KEY")
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "QTFB_KEY unset (not launched via appload?)",
                )
            })?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "QTFB_KEY not a number"))?;
        Self::connect(key)
    }

    pub fn connect(key: u32) -> io::Result<Self> {
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0) };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }

        let mut addr: libc::sockaddr_un = unsafe { mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
        let path = CString::new(SOCKET_PATH).unwrap();
        for (i, b) in path.as_bytes_with_nul().iter().enumerate() {
            addr.sun_path[i] = *b as libc::c_char;
        }
        let rc = unsafe {
            libc::connect(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        let init = ClientMessage {
            msg_type: MESSAGE_INITIALIZE,
            contents: ClientMessageContents {
                init: InitMessageContents {
                    framebuffer_key: key,
                    framebuffer_type: FBFMT_RMPP_RGB888,
                },
            },
        };
        let sent = unsafe {
            libc::send(
                fd,
                &init as *const _ as *const libc::c_void,
                mem::size_of::<ClientMessage>(),
                0,
            )
        };
        if sent == -1 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        let mut resp = ServerMessage {
            msg_type: 0,
            contents: ServerMessageContents {
                init: InitMessageResponseContents {
                    shm_key_defined: 0,
                    shm_size: 0,
                },
            },
        };
        let got = unsafe {
            libc::recv(
                fd,
                &mut resp as *mut _ as *mut libc::c_void,
                mem::size_of::<ServerMessage>(),
                0,
            )
        };
        if got < 1 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }

        let (shm_key, shm_size) =
            unsafe { (resp.contents.init.shm_key_defined, resp.contents.init.shm_size) };
        let shm_name = format!("/dev/shm/qtfb_{shm_key}");
        let file = OpenOptions::new().read(true).write(true).open(&shm_name)?;
        let shm_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                shm_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if shm_ptr == libc::MAP_FAILED {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
        let shm = unsafe { std::slice::from_raw_parts_mut(shm_ptr as *mut u8, shm_size) };

        Ok(Self {
            fd,
            shm_ptr,
            shm,
            width: RMPP_WIDTH,
            height: RMPP_HEIGHT,
            current_mode: Cell::new(-1),
        })
    }

    pub fn raw_fd(&self) -> i32 {
        self.fd
    }

    /// Composite an RGBA buffer (`bw`×`bh`) into the framebuffer at (`x`,`y`),
    /// dropping alpha (cards are opaque). Clips to the panel.
    pub fn blit_rgba(&mut self, src: &[u8], bw: usize, bh: usize, x: usize, y: usize) {
        for row in 0..bh {
            let py = y + row;
            if py >= self.height {
                break;
            }
            for col in 0..bw {
                let px = x + col;
                if px >= self.width {
                    break;
                }
                let si = (row * bw + col) * 4;
                let di = (py * self.width + px) * 3;
                self.shm[di] = src[si];
                self.shm[di + 1] = src[si + 1];
                self.shm[di + 2] = src[si + 2];
            }
        }
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, gray: u8) {
        let x0 = x.max(0) as usize;
        let y0 = y.max(0) as usize;
        let x1 = ((x + w).max(0) as usize).min(self.width);
        let y1 = ((y + h).max(0) as usize).min(self.height);
        for yy in y0..y1 {
            let row = yy * self.width;
            for xx in x0..x1 {
                let i = (row + xx) * 3;
                self.shm[i] = gray;
                self.shm[i + 1] = gray;
                self.shm[i + 2] = gray;
            }
        }
    }

    pub fn clear(&mut self, gray: u8) {
        for b in self.shm.iter_mut() {
            *b = gray;
        }
    }

    fn send(&self, msg: &ClientMessage) -> io::Result<()> {
        self.send_flags(msg, 0)
    }

    fn send_flags(&self, msg: &ClientMessage, flags: i32) -> io::Result<()> {
        let rc = unsafe {
            libc::send(
                self.fd,
                msg as *const _ as *const libc::c_void,
                mem::size_of::<ClientMessage>(),
                flags,
            )
        };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn set_refresh_mode(&self, mode: i32) -> io::Result<()> {
        if self.current_mode.get() == mode {
            return Ok(());
        }
        self.send(&ClientMessage {
            msg_type: MESSAGE_SET_REFRESH_MODE,
            contents: ClientMessageContents { refresh_mode: mode },
        })?;
        self.current_mode.set(mode);
        Ok(())
    }

    /// Non-blocking refresh-mode set — used on the inking hot path so a busy e-ink
    /// server never blocks the poll loop (dropping the message is harmless; the mode
    /// is sticky and will be re-sent).
    pub fn try_set_refresh_mode(&self, mode: i32) -> io::Result<()> {
        if self.current_mode.get() == mode {
            return Ok(());
        }
        let r = self.send_flags(
            &ClientMessage {
                msg_type: MESSAGE_SET_REFRESH_MODE,
                contents: ClientMessageContents { refresh_mode: mode },
            },
            libc::MSG_DONTWAIT,
        );
        if r.is_ok() {
            self.current_mode.set(mode);
        }
        r
    }

    /// Non-blocking partial update — the inking hot path. If the server's socket
    /// buffer is full (it's behind on e-ink refreshes), this returns `WouldBlock`
    /// instead of stalling; the caller keeps the dirty region and retries, which
    /// naturally rate-limits ink refreshes to what the panel can absorb. Blocking
    /// sends here were the cause of the mid-writing "hang".
    pub fn try_update_partial(&self, x: i32, y: i32, w: i32, h: i32) -> io::Result<()> {
        self.send_flags(
            &ClientMessage {
                msg_type: MESSAGE_UPDATE,
                contents: ClientMessageContents {
                    update: UpdateRegionMessageContents {
                        msg_type: UPDATE_PARTIAL,
                        x,
                        y,
                        w,
                        h,
                    },
                },
            },
            libc::MSG_DONTWAIT,
        )
    }

    pub fn update_full(&self) -> io::Result<()> {
        self.send(&ClientMessage {
            msg_type: MESSAGE_UPDATE,
            contents: ClientMessageContents {
                update: UpdateRegionMessageContents {
                    msg_type: UPDATE_ALL,
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
            },
        })
    }

    pub fn update_partial(&self, x: i32, y: i32, w: i32, h: i32) -> io::Result<()> {
        self.send(&ClientMessage {
            msg_type: MESSAGE_UPDATE,
            contents: ClientMessageContents {
                update: UpdateRegionMessageContents {
                    msg_type: UPDATE_PARTIAL,
                    x,
                    y,
                    w,
                    h,
                },
            },
        })
    }

    pub fn request_full_refresh(&self) -> io::Result<()> {
        self.send(&ClientMessage {
            msg_type: MESSAGE_REQUEST_FULL_REFRESH,
            contents: ClientMessageContents { refresh_mode: 0 },
        })
    }

    /// Non-blocking recv of one server message. `Ok(Some(event))` for an input
    /// event, `Ok(None)` when there is no pending message or it isn't an input
    /// event, `Err` only on a real socket close. Used by the poll-multiplexed loop
    /// (QTFB touch socket + pen evdev fd) so a quiet socket never blocks the pen.
    pub fn recv_input_nonblock(&self) -> io::Result<Option<InputEvent>> {
        let mut msg = ServerMessage {
            msg_type: 0,
            contents: ServerMessageContents {
                init: InitMessageResponseContents {
                    shm_key_defined: 0,
                    shm_size: 0,
                },
            },
        };
        let got = unsafe {
            libc::recv(
                self.fd,
                &mut msg as *mut _ as *mut libc::c_void,
                mem::size_of::<ServerMessage>(),
                libc::MSG_DONTWAIT,
            )
        };
        if got < 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(c) if c == libc::EAGAIN || c == libc::EWOULDBLOCK => Ok(None),
                _ => Err(e),
            };
        }
        if got == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "qtfb socket closed",
            ));
        }
        if msg.msg_type != MESSAGE_USERINPUT {
            return Ok(None);
        }
        let ui = unsafe { msg.contents.user_input };
        Ok(Some(InputEvent {
            input_type: ui.input_type,
            dev_id: ui.dev_id,
            x: ui.x,
            y: ui.y,
            d: ui.d,
        }))
    }

    /// Blocking recv of one server message. `Some(event)` for input, `None` for a
    /// non-input message, `Err` on socket close.
    pub fn poll_input(&self) -> io::Result<Option<InputEvent>> {
        let mut msg = ServerMessage {
            msg_type: 0,
            contents: ServerMessageContents {
                init: InitMessageResponseContents {
                    shm_key_defined: 0,
                    shm_size: 0,
                },
            },
        };
        let got = unsafe {
            libc::recv(
                self.fd,
                &mut msg as *mut _ as *mut libc::c_void,
                mem::size_of::<ServerMessage>(),
                0,
            )
        };
        if got < 1 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "qtfb socket closed",
            ));
        }
        if msg.msg_type != MESSAGE_USERINPUT {
            return Ok(None);
        }
        let ui = unsafe { msg.contents.user_input };
        Ok(Some(InputEvent {
            input_type: ui.input_type,
            dev_id: ui.dev_id,
            x: ui.x,
            y: ui.y,
            d: ui.d,
        }))
    }
}

impl Drop for Qtfb {
    fn drop(&mut self) {
        let _ = self.send(&ClientMessage {
            msg_type: MESSAGE_TERMINATE,
            contents: ClientMessageContents { refresh_mode: 0 },
        });
        unsafe {
            libc::munmap(self.shm_ptr, self.shm.len());
            libc::close(self.fd);
        }
    }
}
