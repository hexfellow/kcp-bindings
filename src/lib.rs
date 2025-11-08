use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::os::raw::{c_char, c_int, c_long, c_uint, c_void};
use std::rc::Rc;
use std::slice;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
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
    // Put data into deque
    // println!("UDP output: {:?}", data);
    ctx.tx.lock().unwrap().push_back((data, ctx.peer));
    // println!("Pushed data to deque");
    // println!("Deque size: {}", ctx.tx.borrow().len());
    len
}

struct Kcp {
    kcp: *mut IKCPCB,
}

unsafe impl Send for Kcp {}

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

impl Drop for Kcp {
    fn drop(&mut self) {
        unsafe { ikcp_release(self.kcp) };
    }
}

// For now only one conv id each port.
// Ofc this can be improved. Lets do this in the future.
// TODO: Improve this
// When this struct is dropped, the underlying kcp objects will be dropped.
// TODO impl drop for this struct, make sure to stop spawned tasks, drop underlaying kcp objects.
pub struct KcpPortOwner {
    // A Map to remember the peer for each conv id. For multi connection support.
    // Used when sending data, and when receiving data, we can look up the peer from the map to double check validity.
    // map: Arc<tokio::sync::Mutex<HashMap<u32, SocketAddr>>>,
    socket: Arc<UdpSocket>,
    j: tokio::task::JoinHandle<()>,
}

impl KcpPortOwner {
    // async fn new(bind: SocketAddr) -> Result<Self, io::Error> {
    //     let socket = UdpSocket::bind(bind).await?;
    //     let socket = Arc::new(socket);
    //     Ok(Self {
    //         // map: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    //         socket,
    //     })
    // }

    // async fn new_connection(&mut self, conv: u32, peer: SocketAddr) {
    pub async fn new(
        bind: SocketAddr,
        conv: u32,
        peer: SocketAddr,
    ) -> Result<(Self, mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>), anyhow::Error> {
        let socket = UdpSocket::bind(bind).await?;
        let socket = Arc::new(socket);
        // From app layer. App uses tx to send data to KCP layer.
        let (app_send_tx, mut app_send_rx) = mpsc::channel::<Vec<u8>>(100);
        // Data from KCP layer to app layer.
        let (app_recv_tx, mut app_recv_rx) = mpsc::channel::<Vec<u8>>(100);

        let skt = socket.clone();
        let j = tokio::spawn(async move {
            let (kcp, dq) = Kcp::new(conv, peer).expect("Failed to create KCP");
            let kcp = Mutex::new(kcp);
            unsafe {
                let kcp = kcp.lock().unwrap();
                ikcp_nodelay(kcp.kcp, 1, 10, 2, 1);
                ikcp_setmtu(kcp.kcp, 1400);
                ikcp_setoutput(kcp.kcp, Some(udp_output));
            };

            // TODO Remember to check deq everytime we called ikcp_update or ikcp_input

            let start_time = Instant::now();
            let kcp = kcp;

            loop {
                tokio::select! {
                    data = app_send_rx.recv() => {
                        // println!("Sending data to KCP: {:?}", data);
                        // TODO when app_send_tx is dropped, data will be None.
                        // Need to handle this.
                        send_data(&kcp, data.unwrap(), &dq, &skt, start_time).await;
                    }
                    data = socket_rx(&kcp, &dq, &skt, conv, peer, start_time) => {
                        if let Ok(Some(data)) = data {
                            // println!("Received data from socket: {:?}", data);
                            app_recv_tx.send(data).await.unwrap();
                        }
                    }
                }
            }
        });

        // Connection is now ready, add it to the map.
        // self.map.lock().await.insert(conv, peer);

        Ok((Self { socket, j }, app_send_tx, app_recv_rx))
    }
}

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
                    // println!("Sending {:?} to {:?}", data, addr);
                    socket.send_to(&data, addr).await.unwrap();
                }
                None => break,
            }
        }
    }
}

async fn socket_rx(
    kcp: &Mutex<Kcp>,
    dq: &Arc<Mutex<std::collections::VecDeque<(Vec<u8>, SocketAddr)>>>,
    socket: &Arc<UdpSocket>,
    // map: &tokio::sync::Mutex<HashMap<u32, SocketAddr>>,
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
                    // println!("Received {:?} from {:?}", size, addr);
                    // Got data from underlying socket
                    // Call ikcp_input to handle the data
                    // Check if package is valid, and source is registered in map
                    if size < 24 {
                        // Ignore package
                        println!("Ignore package from too small {:?}", addr);
                        return Ok(None);
                    }
                    // println!("UDP buffer: {:?}", udp_buffer);
                    if conv != u32::from_le_bytes(udp_buffer[0..4].try_into().unwrap()) {
                        println!("Ignore package from wrong conv {:?}", addr);
                        return Ok(None);
                    }
                    if addr != peer {
                        println!("Ignore package from wrong peer {:?}", addr);
                        return Ok(None);
                    }
                    // println!("Conv: {:?}", conv);
                    // let record = map
                    //     .borrow_mut()
                    //     .get(&conv)
                    //     .ok_or(anyhow::anyhow!("conv not found"))?
                    //     .clone();
                    // println!("Record: {:?}", record);
                    // if addr != record {
                    //     // Ignore package
                    //     println!("Ignore package from {:?}", addr);
                    //     return Ok(None);
                    // }
                    // println!("Handle package from {:?}", addr);
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
                    // println!("Sending {:?} to {:?}", data, addr);
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
    println!(
        "Received data from KCP: {:?}",
        String::from_utf8_lossy(&data)
    );
    Ok(Some(data))
}
