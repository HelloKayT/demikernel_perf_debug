// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//==============================================================================
// Imports
//==============================================================================

use super::IoUringRuntime;
use ::arrayvec::ArrayVec;
use ::runtime::{
    memory::Buffer,
    network::{
        consts::RECEIVE_BATCH_SIZE,
        NetworkRuntime,
        PacketBuf,
    },
};

//==============================================================================
// Trait Implementations
//==============================================================================

/// Network Runtime Trait Implementation for I/O User Ring Runtime
impl NetworkRuntime for IoUringRuntime {
    // TODO: Rely on a default implementation for this.
    fn transmit(&self, _pkt: Box<dyn PacketBuf>) {
        unreachable!()
    }

    // TODO: Rely on a default implementation for this.
    fn receive(&self) -> ArrayVec<Buffer, RECEIVE_BATCH_SIZE> {
        unreachable!()
    }
}
