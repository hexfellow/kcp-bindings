use log::debug;
use std::net::SocketAddr;
use std::os::raw::{c_char, c_int, c_long, c_uint, c_void};
use std::slice;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(feature = "hexfellow")]
use tokio::net::UdpSocket;
#[cfg(feature = "hexfellow")]
use tokio::sync::mpsc;

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

struct KcpUserData {
    tx: Arc<Mutex<std::collections::VecDeque<(Vec<u8>, SocketAddr)>>>,
    peer: SocketAddr,
}

#[cfg(feature = "hexfellow")]
unsafe extern "C" fn udp_output(
    buf: *const c_char,
    len: c_int,
    _kcp: *mut IKCPCB,
    user: *mut c_void,
) -> c_int {
    if buf.is_null() || user.is_null() || len <= 0 {
        return -1;
    }

    let ctx = &*(user as *const KcpUserData);

    let data = slice::from_raw_parts(buf as *const u8, len as usize).to_vec();
    ctx.tx.lock().unwrap().push_back((data, ctx.peer));
    len
}

#[cfg(feature = "hexfellow")]
struct Kcp {
    kcp: *mut IKCPCB,
}

#[cfg(feature = "hexfellow")]
unsafe impl Send for Kcp {}

#[cfg(feature = "hexfellow")]
impl Kcp {
    fn new(
        conv: u32,
        peer: SocketAddr,
    ) -> Option<(
        Self,
        Arc<Mutex<std::collections::VecDeque<(Vec<u8>, SocketAddr)>>>,
    )> {
        let dq = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let user_data = KcpUserData {
            tx: dq.clone(),
            peer,
        };
        let user_data_ptr = Box::into_raw(Box::new(user_data));
        let kcp = unsafe { ikcp_create(conv, user_data_ptr as *mut c_void) };
        if kcp.is_null() {
            None
        } else {
            Some((Self { kcp }, dq))
        }
    }
}

#[cfg(feature = "hexfellow")]
impl Drop for Kcp {
    fn drop(&mut self) {
        unsafe { ikcp_release(self.kcp) };
        debug!("Kcp dropped");
    }
}

// For now only one conv id each port.
// Ofc this can be improved. Lets do this in the future.
// TODO: Improve this
// When this struct is dropped, the underlying kcp objects will be dropped.
// TODO impl drop for this struct, make sure to stop spawned tasks, drop underlaying kcp objects.
#[cfg(feature = "hexfellow")]
pub struct KcpPortOwner {
    // A Map to remember the peer for each conv id. For multi connection support.
    // Used when sending data, and when receiving data, we can look up the peer from the map to double check validity.
    // map: Arc<tokio::sync::Mutex<HashMap<u32, SocketAddr>>>,
    socket: Arc<UdpSocket>,
    j: tokio::task::JoinHandle<()>,
}

#[cfg(feature = "hexfellow")]
impl Drop for KcpPortOwner {
    fn drop(&mut self) {
        self.j.abort();
    }
}

#[cfg(feature = "hexfellow")]
impl KcpPortOwner {
    pub async fn new_costom_socket(
        socket: UdpSocket,
        conv: u32,
        peer: SocketAddr,
    ) -> Result<(Self, mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>), anyhow::Error> {
        let socket: Arc<UdpSocket> = Arc::new(socket);
        // From app layer. App uses tx to send data to KCP layer.
        let (app_send_tx, mut app_send_rx) = mpsc::channel::<Vec<u8>>(100);
        // Data from KCP layer to app layer.
        let (app_recv_tx, app_recv_rx) = mpsc::channel::<Vec<u8>>(100);

        let skt = socket.clone();
        let (kcp, dq) = Kcp::new(conv, peer).ok_or(anyhow::anyhow!(
            "Failed to create KCP from underlying C library"
        ))?;
        let j = tokio::spawn(async move {
            let kcp = Mutex::new(kcp);
            unsafe {
                let kcp = kcp.lock().unwrap();
                ikcp_nodelay(kcp.kcp, 1, 10, 2, 1);
                ikcp_setmtu(kcp.kcp, 1400);
                ikcp_setoutput(kcp.kcp, Some(udp_output));
            };
            let start_time = Instant::now();
            loop {
                tokio::select! {
                    data = (app_send_rx.recv()), if !app_send_rx.is_closed() => {
                        if let Some(data) = data {
                            send_data(&kcp, data, &dq, &skt, start_time).await;
                        } else {
                            debug!("App send tx dropped");
                        }
                    },
                    data = socket_rx(&kcp, &dq, &skt, conv, peer, start_time) => {
                        if let Ok(Some(data)) = data {
                            app_recv_tx.send(data).await.unwrap();
                        }
                    }
                }
            }
        });

        Ok((Self { socket, j }, app_send_tx, app_recv_rx))
    }

    pub async fn new(
        bind: SocketAddr,
        conv: u32,
        peer: SocketAddr,
    ) -> Result<(Self, mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>), anyhow::Error> {
        let socket: UdpSocket = UdpSocket::bind(bind).await?;
        Self::new_costom_socket(socket, conv, peer).await
    }

    pub fn get_socket_local_addr(&self) -> Result<SocketAddr, anyhow::Error> {
        self.socket
            .local_addr()
            .map_err(|e| anyhow::anyhow!("Failed to get local address: {e}"))
    }

    pub async fn send_binary(
        tx: &mpsc::Sender<Vec<u8>>,
        data: Vec<u8>,
    ) -> Result<(), anyhow::Error> {
        tx.send(HexSocketParser::create_header(
            &data,
            HexSocketOpcode::Binary,
        ))
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send binary data: {e}"))?;
        tx.send(data)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to send binary data: {e}"))?;
        Ok(())
    }
}

#[cfg(feature = "hexfellow")]
async fn send_data(
    kcp: &Mutex<Kcp>,
    data: Vec<u8>,
    dq: &Arc<Mutex<std::collections::VecDeque<(Vec<u8>, SocketAddr)>>>,
    socket: &Arc<UdpSocket>,
    start_time: Instant,
) {
    unsafe {
        let kcp = kcp.lock().unwrap();
        ikcp_send(kcp.kcp, data.as_ptr() as *const c_char, data.len() as i32);
        ikcp_update(kcp.kcp, start_time.elapsed().as_millis() as u32);
        ikcp_flush(kcp.kcp);
    };
    // Check dqueue
    {
        loop {
            let res = { dq.lock().unwrap().pop_front() };
            match res {
                Some((data, addr)) => {
                    socket.send_to(&data, addr).await.unwrap();
                }
                None => break,
            }
        }
    }
}

#[cfg(feature = "hexfellow")]
async fn socket_rx(
    kcp: &Mutex<Kcp>,
    dq: &Arc<Mutex<std::collections::VecDeque<(Vec<u8>, SocketAddr)>>>,
    socket: &Arc<UdpSocket>,
    conv: u32,
    peer: SocketAddr,
    start_time: Instant,
) -> Result<Option<Vec<u8>>, anyhow::Error> {
    let mut udp_buffer = [0u8; 1500];
    if let Ok(data) =
        tokio::time::timeout(Duration::from_millis(1), socket.recv_from(&mut udp_buffer)).await
    {
        {
            // Successfully got data
            match data {
                Ok((size, addr)) => {
                    if size < 24 {
                        debug!("Ignore package from too small {:?}", addr);
                        return Ok(None);
                    }
                    if conv != u32::from_le_bytes(udp_buffer[0..4].try_into().unwrap()) {
                        debug!("Ignore package from wrong conv {:?}", addr);
                        return Ok(None);
                    }
                    if addr != peer {
                        debug!("Ignore package from wrong peer {:?}", addr);
                        return Ok(None);
                    }
                    unsafe {
                        ikcp_input(
                            kcp.lock().unwrap().kcp,
                            udp_buffer.as_ptr() as *const c_char,
                            size as c_long,
                        );
                    };
                }
                Err(e) => return Err(anyhow::anyhow!("udp recv error: {e}")),
            }
        }
    };
    unsafe {
        ikcp_update(
            kcp.lock().unwrap().kcp,
            start_time.elapsed().as_millis() as u32,
        );
        ikcp_flush(kcp.lock().unwrap().kcp);
    };
    // Check deq
    {
        loop {
            let res = { dq.lock().unwrap().pop_front() };
            match res {
                Some((data, addr)) => {
                    socket.send_to(&data, addr).await.unwrap();
                }
                None => break,
            }
        }
    }
    // Run ikcp_recv to get data
    let data = [0u8; 2048];
    let size = unsafe {
        ikcp_recv(
            kcp.lock().unwrap().kcp,
            data.as_ptr() as *mut c_char,
            data.len() as i32,
        )
    };
    if size < 0 {
        return Err(anyhow::anyhow!("ikcp_recv error: {size}"));
    }
    let data = data[0..size as usize].to_vec();
    debug!(
        "Received data from KCP: {:?}",
        String::from_utf8_lossy(&data)
    );
    Ok(Some(data))
}

#[cfg(feature = "hexfellow")]
#[derive(Debug, Eq, PartialEq)]
pub enum HexSocketOpcode {
    // Continuation = 0x0,
    Text = 0x1,
    Binary = 0x2,
    // ConnectionClose = 0x8,
    Ping = 0x9,
    Pong = 0xA,
}

#[cfg(feature = "hexfellow")]
impl TryFrom<u8> for HexSocketOpcode {
    type Error = anyhow::Error;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x1 => Ok(Self::Text),
            0x2 => Ok(Self::Binary),
            0x9 => Ok(Self::Ping),
            0xA => Ok(Self::Pong),
            _ => Err(anyhow::anyhow!("Invalid opcode: {value}")),
        }
    }
}

#[cfg(feature = "hexfellow")]
pub struct HexSocketParser {
    data: Vec<u8>,
}

#[cfg(feature = "hexfellow")]
impl HexSocketParser {
    pub fn new() -> Self {
        Self { data: Vec::new() }
    }

    // Panics if data is more than UINT16_MAX bytes.
    pub fn create_header(data: &[u8], opcode: HexSocketOpcode) -> Vec<u8> {
        let len = data.len();
        if len > UINT16_MAX as usize {
            panic!("Data is more than UINT16_MAX bytes");
        }
        let len = len as u16;
        let mut header = [0u8; 4];
        header[0] = 0x80 | (opcode as u8);
        header[1] = 0x00;
        let len = len.to_le_bytes();
        header[2..4].copy_from_slice(&len);
        header.to_vec()
    }

    // Returns Err is data is bad.
    // Returns Ok(None) if data is good but not compelete
    // Returns data if parsed any.
    pub fn parse(
        &mut self,
        incoming: &[u8],
    ) -> Result<Option<Vec<(HexSocketOpcode, Vec<u8>)>>, anyhow::Error> {
        let mut ret: Vec<(HexSocketOpcode, Vec<u8>)> = vec![];
        self.data.extend_from_slice(incoming);
        loop {
            let result = self.inner_parse()?;
            match result {
                Some((opcode, data)) => {
                    ret.push((opcode, data));
                }
                None => {
                    if ret.len() > 0 {
                        return Ok(Some(ret));
                    } else {
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn inner_parse(&mut self) -> Result<Option<(HexSocketOpcode, Vec<u8>)>, anyhow::Error> {
        // Check if head if full
        if self.data.len() < 4 {
            return Ok(None);
        }
        // Validate head
        let (len, opcode) = if self.data.len() >= 4 {
            let d = &self.data;
            if d[0] & 0xF0 != 0x80 {
                return Err(anyhow::anyhow!("Invalid header: {:?}", d));
            }
            let opcode = HexSocketOpcode::try_from(d[0] & 0x0F)?;
            let len = u16::from_le_bytes([d[2], d[3]]);
            (len as usize, opcode)
        } else {
            return Ok(None);
        };
        if self.data.len() >= len + 4 {
            let (frame, remaining) = self.data.split_at(4 + len);
            let data = frame[4..].to_vec();
            self.data = remaining.to_vec();
            Ok(Some((opcode, data)))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::vec;

    use super::*;

    #[test]
    fn parse_stream() {
        let data = "hexfellow".as_bytes();
        let head = HexSocketParser::create_header(data, HexSocketOpcode::Text);
        let expected = [129, 0, 9, 0u8];
        assert_eq!(head, expected);

        // Now create a lot of data, pack them to stream, and decode them out
        let data = [
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "1".to_string(),
            "This is a very long string to test the parser, hahahaha".to_string(),
        ];
        let mut stream = vec![];
        for d in data.clone() {
            let head = HexSocketParser::create_header(d.as_bytes(), HexSocketOpcode::Text);
            stream.extend_from_slice(&head);
            stream.extend_from_slice(d.as_bytes());
        }
        // Split stream into small splits
        let random_lens = [3, 10, 20, 50, 100];
        for random_len in random_lens {
            let mut collected = vec![];
            let mut parser = HexSocketParser::new();
            let chunks = stream.chunks(random_len).collect::<Vec<_>>();
            for chunk in chunks {
                let result = parser.parse(&chunk);
                if let Some(data) = result.unwrap() {
                    collected.extend(data);
                }
            }
            let mut texts = vec![];
            for (opcode, data) in collected {
                if opcode == HexSocketOpcode::Text {
                    texts.push(String::from_utf8_lossy(&data).to_string());
                } else {
                    panic!("Invalid opcode: {:?}", opcode);
                }
            }

            assert_eq!(texts, data);
            println!("Test passed for length: {:?}", random_len);
        }
    }
}
