pub mod tcp;
pub mod udp;

use alvr_common::{anyhow::Result, ConResult};

pub trait SocketWriter: Send {
    fn send(&mut self, buffer: &[u8]) -> Result<()>;
}

// Trait used to abstract different socket (or other input/output) implementations. The funtionality
// is the intersection of the functionality of each implementation, that is it inheirits all
// limitations
pub trait SocketReader: Send {
    // Returns number of bytes written. buffer must be big enough to be able to receive a full
    // packet (size of MTU) otherwise data will be corrupted. The size of the data is
    fn recv(&mut self, buffer: &mut [u8]) -> ConResult<usize>;

    fn peek(&self, buffer: &mut [u8]) -> ConResult<usize>;

    // Diagnostics only: bytes currently waiting in the kernel receive buffer, and that buffer's
    // size. Comparing the two is what tells a receive loop that cannot keep up (the queue runs
    // into the buffer size just before packets go missing) from loss that happens before the
    // socket ever sees the packet (the queue stays low while packets still go missing).
    fn queued_bytes(&self) -> Option<usize> {
        None
    }
    // True when every recv() returns one whole datagram. The stream socket then reads each shard
    // with a single call instead of peeking at its prefix first -- see StreamSocket::recv.
    fn is_datagram(&self) -> bool {
        false
    }
    fn buffer_bytes(&self) -> Option<usize> {
        None
    }
}
