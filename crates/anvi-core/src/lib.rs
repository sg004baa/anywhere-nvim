//! Portable core of anvi: the resident headless Neovim server, the
//! host/nvim event contract, the session state machine and text normalization.
//!
//! Everything OS-specific (window control, UI Automation, hotkey, tray) lives
//! in the `anvi` binary crate. The clipboard is split: the port (the
//! [`clipboard::Clipboard`] trait, since the RPC endpoint lives here) is in
//! core, its Win32 implementation is in `anvi`.

pub mod clipboard;
pub mod event;
pub mod port;
pub mod server;
pub mod session;
pub mod text;
pub mod ui;

pub use clipboard::{Clipboard, RegType};
pub use event::HostEvent;
pub use server::{NvimConfig, NvimHandles, NvimServer, SpawnCancelled, SpawnPolicy};
pub use session::{Applied, Phase, Session};
