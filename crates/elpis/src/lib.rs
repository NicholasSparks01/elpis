//! elpis: the engine that drives a containerd shim to run coding-agent boxes in microVMs, with no daemon and no containerd.
//!
//! [`session::Session`] plays containerd's part, the same way sbx's built-in containerd does:
//!   1. serves the ttrpc event socket the shim publishes to (TTRPC_ADDRESS) and logs every event
//!   2. runs the shim's "start" action (args + protobuf on stdin) and reads back the shim's pipe address
//!   3. reads the shim's log pipe (\\.\pipe\containerd-shim-<ns>-<id>-log), where the shim and Sailor log
//!   4. speaks ttrpc to the shim's containerd.task.v3.Task service: Create, Start, Connect, Exec, Wait
//!   5. on stop: Kill, Delete, Shutdown
//!
//! Inputs: <work>\bundle\config.json, <work>\rootfs.json, <work>\rwlayer.img, made by hand or by
//! [`workdir::create_box`] from an sbx kit ([`kits`]), a pulled image ([`registry`]) or a template.

pub mod boxfile;
pub mod console;
pub mod credproxy;
pub mod events;
pub mod exec;
pub mod hostports;
pub mod kits;
pub mod log;
pub mod mkfs;
pub mod netstack;
pub mod pb;
pub mod policy;
pub mod registry;
pub mod secrets;
pub mod session;
pub mod shimlog;
pub mod stdio;
pub mod ttrpc;
pub mod unixsock;
pub mod win;
pub mod workdir;

pub use log::{Color, Log};
pub use session::{Config, Session, ShimLog};

pub const TASK_SERVICE: &str = "containerd.task.v3.Task";
