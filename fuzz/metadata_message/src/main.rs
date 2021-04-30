// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT
#![forbid(unsafe_code)]

use honggfuzz::fuzz;
use log::debug;

use tezos_messages::p2p::binary_message::BinaryRead;
use tezos_messages::p2p::encoding::prelude::*;

fn main() {
    loop {
        fuzz!(|data: &[u8]| {
            if let Err(e) = MetadataMessage::from_bytes(data) {
                debug!(
                    "MetadataMessage::from_bytes produced error for input: {:?}\nError:\n{:?}",
                    data, e
                );
            }
        });
    }
}
