#![allow(dead_code)]

pub mod request;
pub mod response;
pub mod body;
mod core;

pub use tokio::sync::broadcast::{Sender, Receiver};
pub use self::core::*;

use hyper::body::Bytes;
use request::RequestHead;
use response::ResponseHead;

#[derive(Debug, Clone)]
pub enum ProxyState {
    RequestHead(RequestHead),
    RequestChunk{id: u32, chunk: Bytes},
    RequestDone{id: u32},
    ResponseHead(ResponseHead),
    ResponseChunk{id: u32, chunk: Bytes},
    ResponseDone{id: u32},
    UpgradeTx{id: u32, chunk: Bytes},
    UpgradeRx{id: u32, chunk: Bytes},
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ProxyEvent {
    id: u32,
    event: ProxyState,
}
